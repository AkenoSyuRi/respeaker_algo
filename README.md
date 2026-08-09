# respeaker_algo

ReSpeaker Mic Array v2.0（XMOS XVF-3000）Windows 专用录音与内置算法 Pipeline 工具。

程序固定使用 WASAPI 独占模式采集 **16 kHz / 16-bit / 6 通道**交织 PCM。默认只录音；
传入 `--pipeline-config` 时，拆分后的音频还会依次经过配置的内置 Rust 算法模块。
当前 Pipeline 仅实现实时 4-Mic 单声源二维 DOA，后续可按顺序扩展 AEC、BF 等模块。

## 固件与通道布局

配合 `6_channels_firmware.bin` 使用：

| 通道 | 内容 | 录音文件 |
|------|------|----------|
| ch0 | 固件算法处理音频 | `*_respeaker_algo.wav` |
| ch1–ch4 | mic1–mic4 原始数据 | `*_respeaker_mic.wav`（4ch） |
| ch5 | 回放/AEC 参考 | `*_respeaker_ref.wav` |

输出为标准 16-bit PCM `WAVEFORMATEX`，不写入扬声器布局 channel mask。

## 构建与运行

```powershell
cargo build --release

# 纯录音，直到 Ctrl+C
target\release\respeaker_algo.exe

# 录制 10 秒并指定输出位置
target\release\respeaker_algo.exe `
  --duration 10 `
  --out-dir target/out `
  --prefix recording

# 录音并串联运行 DOA Pipeline
target\release\respeaker_algo.exe `
  --duration 10 `
  --pipeline-config configs/doa.toml
```

CLI 仅提供录音相关选项：`--duration`、`--out-dir`、`--prefix` 和
`--pipeline-config`，另保留标准 `--help`/`--version`。程序自动查找名称包含
`ReSpeaker` 的输入设备；采样率、通道数、设备和后端不可通过 CLI 覆盖。

## Pipeline 配置

配置使用版本化 TOML。`[[modules]]` 按声明顺序串联执行；模块将结果发布到共享状态，
供后续模块消费。例如未来 BF 必须位于 DOA 之后，读取 DOA 发布的最新方向。

当前示例：[configs/doa.toml](configs/doa.toml)

```toml
version = 1

[[modules]]
type = "doa"
enabled = true
csv = true
beta = 0.75
cpsd_tau_ms = 100.0
angle_offset_deg = 0.0
clockwise = false
acquire_confidence = 0.65
update_confidence = 0.40
max_coast_ms = 500
```

模块未写入配置即不启用；`enabled = false` 可临时关闭。当前只接受一个已启用的
`type = "doa"`，未知模块、重复模块、版本错误或非法参数都会在启动采集前报错。
`csv = true` 额外生成 `{prefix}_respeaker_doa.csv`。

## 实时 DOA

DOA 消费 ch1–ch4，不使用 ch0 固件方向结果，也不修改三路录音缓冲：

```text
512 点 Hann 窗（32 ms）/ 256 点 hop（16 ms）
CPSD EMA → coherence-weighted PHAT-β SRP → confidence gate → circular Kalman
360 个 1° 候选方向 / 终端最多 10 Hz / CSV 每个内部结果一行
```

阵列中心为原点，`+X = 0°`、`+Y = 90°`、逆时针增加，相邻麦克风中心距 45.7 mm：

```text
mic1/ch1 = (+22.85, -22.85) mm   第四象限
mic2/ch2 = (+22.85, +22.85) mm   第一象限
mic3/ch3 = (-22.85, +22.85) mm   第二象限
mic4/ch4 = (-22.85, -22.85) mm   第三象限
```

输出转换为：

```text
output = wrap360(angle_offset_deg + (clockwise ? -internal : internal))
```

现场需用已知方向声源标定 `angle_offset_deg` 和 `clockwise`。当前算法限制为单声源、
二维水平面和远场模型。

## Windows WASAPI 独占

- 设备不能被其它程序占用；
- 固件格式固定为 16 kHz/6ch，channel mask 必须为 0；
- 使用 `PollingExclusive`、100 ms 缓冲和 1 ms 轮询；
- 未发现 ReSpeaker 或独占初始化失败时程序直接退出，不降级到共享模式。

## 代码结构

```text
src/main.rs       录音 CLI 入口
src/recorder.rs   WASAPI 采集、六通道拆分、WAV 与 Pipeline 调度
src/wasapi.rs     Windows WASAPI 独占采集
src/pipeline.rs   TOML 配置、模块顺序、共享状态与运行时
src/wav.rs        标准 WAVEFORMATEX 流式写入
src/doa/          分帧、几何、SRP、跟踪和输出
configs/          可直接使用的 Pipeline 配置
docs/plans/       算法与架构设计
```

## 验证

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

自动测试不需要声卡。发布前还应在真实 ReSpeaker 上分别验证纯录音和
`--pipeline-config configs/doa.toml` 两条路径。
