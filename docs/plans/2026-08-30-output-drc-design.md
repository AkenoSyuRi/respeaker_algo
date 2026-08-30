# 输出 DRC 设计说明

日期：2026-08-30  
仓库：`respeaker_algo`  
来源：从 `tss_algo_pipeline` 移植三区 look-ahead DRC，接到 Beamformer 输出路径。

本文是已确认的设计，不是实现计划。实现前另写 task plan。

---

## 1. 目标

对算法输出在现有数字增益之后再做输出 DRC。`compare_wav` 对照声道（mic1）走同一条后处理链。

处理顺序固定为：

```text
浮点样本（BF 或 mic1 / 32768）
  → × 10^(output_gain_db/20)     // 现有配置，doa_bf / bf_fixed 为 15 dB
  → DRC（TSS 管线默认：+5 dB pregain、2 ms look-ahead、Gate + Compressor + Limiter）
  → × 32768、四舍五入、削波成 PCM16
```

不把 DRC 做成 `[[modules]] type = "drc"`，不引入模块间音频总线。

## 2. 非目标

- 不移植多通道 shared / hybrid / median-reference 检测。
- 不通过 FFI 绑定 C++ `tss_native_drc`。
- 不把 Gate / Compressor / Limiter 细节参数暴露到 TOML。
- 不补偿 look-ahead 延迟（不丢弃文件开头 32 sample 来对齐时间）。
- 本次不把 LCMV 接到 DRC；`src/drc/` 保持可复用，供后续算法调用。
- 测试运行时不调用 Python / C++，也不读取来源仓库路径。一致性验收使用本仓库内预先生成的 golden vector。

## 3. 架构

新增独立 DSP 模块 `src/drc/`。Beamformer 在写 WAV 前调用。Pipeline 只多传一个 `enable_drc` 布尔值。

```text
BeamformerStft hop
        │
        ▼
   output_gain_db
        │
        ├─ enable_drc = false → 现有 apply_gain / 削波
        │
        └─ enable_drc = true
                │
                ├─ mono：TssDrc（算法）
                └─ compare_wav：TssDrc_mic1 + TssDrc_bf（同参数、独立包络）
                        │
                        ▼
                   PCM16 WAV
                   finalize 再 flush 32 sample 尾巴
```

`compare_wav` 使用两个独立实例，不用共享增益。对照的是「同一条后处理链」，不是立体声联动压限。

## 4. 模块边界

### 4.1 文件

| 路径 | 职责 |
|------|------|
| `src/drc/config.rs` | `DrcConfig`：TSS 默认值；管线用法固定 `predelay_s = 0.002` |
| `src/drc/core.rs` | `CompressorState`：Gate + 软膝压缩 + look-ahead + 自适应释放 |
| `src/drc/mod.rs` | 单通道 `TssDrc`：`process_in_place` / `reset` / `flush` / `latency_samples` |
| `src/drc/testdata/pipeline_16k_mono.json` | 16 kHz mono golden：元数据 + `input` / `expected` |
| `tools/generate_drc_golden.py` | 一次性生成脚本，不参与 `cargo test` |
| `LICENSES/tss_drc.MIT` | 原 sndfilter / TSS DRC Python port 的 MIT 声明 |

接入点：`src/main.rs` 增加 `mod drc`；`src/beamformer/mod.rs` 写 WAV 前调用；`src/pipeline.rs` 解析 `enable_drc`。文档更新 `README.md`、`AGENTS.md`。

### 4.2 公开 API

只提供单通道接口：

```rust
impl TssDrc {
    pub fn pipeline(sample_rate: u32) -> Result<Self, String>;
    pub fn latency_samples(&self) -> usize;
    pub fn reset(&mut self);
    pub fn process_in_place(&mut self, samples: &mut [f32]);
    pub fn flush(&mut self, dest: &mut [f32]);
}
```

- `pipeline(16000)` 的 latency 必须是 32。
- `flush` 向状态送零，把 look-ahead 尾巴写进 `dest`；调用方保证 `dest.len() == latency_samples()`。
- 输入按 TSS 语义处理：非有限样本当作 0。

`DrcConfig` 与 core 状态不对外成为 BF 配置的一部分。

### 4.3 DRC 参数（写死，不进 TOML）

与 `tss_algo_pipeline` 管线一致：`DrcConfig.default()`，再把 `predelay_s` 设为 `0.002`。

| 参数 | 值 |
|------|-----|
| `gate_enabled` | `true` |
| `gate_open_threshold_db` | `-48` |
| `gate_close_threshold_db` | `-54` |
| `gate_attack_s` | `0.003` |
| `gate_release_s` | `0.050` |
| `gate_hold_ms` | `10` |
| `gate_floor_db` | `-80` |
| `pregain_db` | `5.0` |
| `compressor_threshold_db` | `-20` |
| `compressor_knee_db` | `25` |
| `compressor_ratio` | `8` |
| `compressor_attack_s` | `0.001` |
| `compressor_release_s` | `0.600` |
| `predelay_s` | `0.002` |
| `release_zone1` | `0.090` |
| `release_zone2` | `0.160` |
| `release_zone3` | `0.420` |
| `release_zone4` | `0.980` |
| `postgain_db` | `0` |
| `wet` | `1` |
| `auto_makeup_gain` | `false` |
| `limiter_enabled` | `true` |
| `limiter_ceiling_dbfs` | `-1.0` |
| `limiter_release_s` | `0.050` |

`release_zone1..4` 属于单通道压缩器，不是多通道检测参数。它们把 `compressor_release_s` 对应的释放样本数划成四个控制点 `y_i = release_samples * release_zone_i`，再拟合成三次多项式，按当前压缩量（dB）查自适应释放速度。来源默认 `0.090 / 0.160 / 0.420 / 0.980`；漏掉它们时释放曲线会与 TSS 不一致。

16 kHz 下 2 ms look-ahead = 32 sample。

**增益对齐（与 TSS 管线的差别）：**

| | 数字增益 | DRC `pregain_db` | 进入压缩器前 |
|--|----------|------------------|--------------|
| TSS 管线 | `OUTPUT_GAIN = 10`（+20 dB） | +5 dB | 约 +25 dB |
| 本项目 `doa_bf` / `bf_fixed` | `output_gain_db = 15` | +5 dB | 约 +20 dB |

保留本项目现有 15 dB，不把 `output_gain_db` 改成 20。DRC 算法参数与 TSS 一致，但 DRC 输入比 TSS 管线低 5 dB。这是项目增益选择，不是参数表笔误。

## 5. Beamformer 接入

### 5.1 配置

`BeamformerConfig` 与 TOML 只增加：

```toml
enable_drc = true   # 默认 true
```

默认 `true`。省略字段时启用输出 DRC；写 `enable_drc = false` 可关闭。

### 5.2 增益与 PCM

- `enable_drc = false`：保持现有 `apply_gain`（`y * gain * 32768` → 四舍五入削波）。
- `enable_drc = true`：先乘 `output_gain`，再 `process_in_place`，最后 `y * 32768` 削波。DRC 输入可以超过 `[-1, 1]`，由 limiter 收口。

`compare_wav` 左右声道都走增益 + 各自 DRC，再交错写入。

### 5.3 帧数与文件长度

STFT 契约不变：`output_frames == input_frames`。

`finalize` 在 STFT flush 之后，若启用 DRC，再 `flush` 32 sample 并写入 WAV。WAV 比录音时长多 2 ms。这 32 sample 不计入 `output_frames`。

不在文件开头丢掉 32 sample 来做延迟补偿。

### 5.4 Scratch 预分配

在 `BeamformerRuntime::new` 里按最终 WAV 声道数预留，之后 `on_hop` / `finalize` 的 DRC 与 PCM 路径不得触发 `Vec` 扩容：

| 缓冲 | 容量 |
|------|------|
| `pcm_scratch` | `wav_channels * HOP_SIZE`（mono=256，`compare_wav`=512） |
| `float_scratch` | `HOP_SIZE`（增益后、送进 DRC 的单通道块） |
| `mic0_buf` | `2 * HOP_SIZE`（512；首个 STFT 输出 hop 被丢弃，第二个 hop 输出前最多积压 512 点） |

`compare_wav` 交错写入时先填满左/右各 hop，再写入 `pcm_scratch`；不得按样本 `push` 到未预留的 256 容量上。`mic0_buf` 必须预留两个 hop：第一个 hop 的 STFT 输出因延迟对齐被丢弃、不会 drain，第二个 hop 到齐前缓冲会增长到 512 点。`process_in_place` 只就地改传入切片，DRC 内部 delay line 在构造时分配（1024 float），实时路径不再 `Vec::push` 扩容。

## 6. 错误、分配与 I/O

这三项分开，不要混成一句：

**错误**

- `TssDrc::pipeline`：`sample_rate == 0` 返回错误。正常 BF 路径传入 `doa::SAMPLE_RATE`（16000）。
- `enable_drc` 不改变现有 `output_gain_db` / 频带 / 方向等校验。
- 非有限输入当 0，与 TSS core 一致；不另做 NaN 日志，不因此返回 `Err`。

**分配**

- 见 5.4。实时 hop 不得分配新堆缓冲；`flush` 写入调用方提供的 32 点栈/预分配切片。

**额外文件**

- DRC 不写自己的 dump WAV。唯一输出仍是现有 `*_respeaker_bf.wav`。
- 不在 audio callback 里跑 DRC（现有算法线程不变）。

## 7. 测试

确定性合成信号，不依赖声卡。

`src/drc/` 行为测试：

- `latency_samples`：16 kHz + 2 ms → 32
- `reset`：同一输入处理两遍，中间 `reset` 后逐样本一致
- `look_ahead_delay`：冲激在输入第 0 点，输出能量峰值约在第 32 点
- `gate_attenuates_silence`：低于关门阈值的低电平被压到接近 `gate_floor`
- `compressor_reduces_loud_tone`：0 dBFS 正弦稳态峰值低于输入
- `limiter_caps_ceiling`：超天花板信号被压到约 -1 dBFS
- `chunk_consistency`：整段一次处理 vs 256 点分块，逐样本一致
- `flush_equals_trailing_zeros`：`process(input)` 再 `flush(32)` 的拼接，必须与 `process(input ‖ zeros(32))` 逐样本相等（`atol = 0`）。输入末尾必须有明显能量，使 flush 出的 32 点不全接近 0，避免「丢掉延迟样本、只补 32 个零」也能通过。
- `nonfinite_matches_zeros`：把 `NaN` / `+Inf` / `-Inf` 放进一块，输出必须与对应位置为 `0` 的输入逐样本相等；随后再送同一块有限信号，两套状态的输出仍相等且全部有限。标准 JSON 不能可靠编码这些值，因此不放进 golden，只做独立单测。
- `long_silence_stays_finite`：先送一块能打开门限/压缩器的有限信号，再连续送至少 16000 点零；全程输出有限，之后再送一块正常信号仍有限，且与「同一序列一次跑完」的实例逐样本一致。

`src/drc/` 来源一致性（golden vector）：

- 提交 `src/drc/testdata/pipeline_16k_mono.json`，字段至少包括：
  - `source_repo = "tss_algo_pipeline"`
  - `source_commit = "c61c5d6b904fa56055b06320810893e851c6e938"`
  - `sample_rate = 16000`、`predelay_s = 0.002`、`mode = "independent"`
  - `chunk_sizes`：固定为 `[1, 15, 16, 17, 80, 161, 256, 3, 511, 988]`（合计 2048，等于 `len(input)`）
  - 第 4.3 节其余管线参数可省略（生成脚本按 default + `predelay_s=0.002`）
  - `input` / `expected`：有限 float 数组
- 生成入口（不参与 `cargo test`）：

```text
uv run --directory C:\Projects\GitProjects\tss_algo_pipeline python C:\Projects\RustProjects\respeaker_algo\tools\generate_drc_golden.py
```

脚本先检查 `git -C <tss_algo_pipeline> status --porcelain` 为空，再检查 `git -C <tss_algo_pipeline> rev-parse HEAD` 等于 `source_commit`；任一条件不满足都拒绝写入。随后用 `TssDrc.process_frame_independent` 按 `chunk_sizes` 逐块处理。之后 `cargo test` 只读 JSON。
- 激励需同时覆盖：开门/关门附近的低电平、软膝区、过阈值压缩、突然掉落（自适应释放）。不包含 NaN/Inf。
- 断言：`process_in_place` 整段输出与 `expected` 比较，`rtol = 0`、`atol = 5e-7`（与来源仓库 C++ / Python 对照容差相同）。再按 JSON 里的 `chunk_sizes` 分块处理同一 `input`，也必须落在该容差内。

BF 接入：

- `enable_drc = false`：现有增益 / 对比 WAV 行为不变
- `enable_drc = true`：单声道 WAV 比输入多 32 sample；前 32 点接近 0
- `enable_drc_flush_keeps_delayed_tail`：输入在结束前保持有能量时，WAV 最后 32 点不得全接近 0。更强检查：对「增益后的 BF 浮点输出 ‖ 32 个零」用独立 `TssDrc::pipeline` 跑一遍，与 WAV 浮点还原逐样本对照（PCM 量化误差允许 ±1 LSB）
- `compare_wav + DRC`：左右都经过增益 + DRC，长度相同，都带 32 sample 非全零尾巴（同样用各自独立 DRC 对照）
- `compare_wav_scratch_capacity_stays_fixed`：构造后记录 `pcm_scratch` / `float_scratch` / `mic0_buf` 的 capacity，连续处理至少 3 个 hop 并 finalize，三者 capacity 均不得增长

提交前：

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

硬件听感不进 CI。需要时用 `doa_bf.toml` / `bf_fixed.toml` 人工对比。

## 8. 文档

- `README.md`：BF WAV 说明改为「增益之后可接 DRC」；`compare_wav` 左右都是增益 + DRC；列出 `enable_drc` 默认 `true`。不要写成「与 TSS 管线电平一致」。
- `AGENTS.md`：补充 BF 输出路径为增益 → 可选 DRC → PCM16。
- 配置示例打开 `enable_drc = true`。
- `tools/generate_drc_golden.py` 只用于再生 golden，不写入 README 主流程。
