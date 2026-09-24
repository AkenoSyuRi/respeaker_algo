# respeaker_algo

ReSpeaker Mic Array v2.0（XMOS XVF-3000）Windows 专用录音与内置算法 Pipeline 管理服务。

程序无参数启动，在 `http://127.0.0.1:8765` 提供本地 Web 管理界面。用户点击开始后，程序才
使用 WASAPI 独占模式采集 **16 kHz / 16-bit / 6 通道**交织 PCM，并可在独立
**algorithm worker** 中运行实时 4-Mic 二维 DOA 与频域 Beamformer。

## 线程模型

运行时有四个角色：

```text
main / Web Server         AudioController         recording session       algorithm-worker
  REST + SSE               状态和 session owner     WASAPI + raw WAV         256-frame hop → DOA → BF
  loopback only            有界控制通道             外部 stop / finalize       CSV / BF WAV
```

过载语义：

| 队列 | Full / 失败时 |
|------|----------------|
| capture | 无法保证 raw 连续 → **停止本次录制** |
| algorithm | 关闭 Pipeline 输入 → **后续只录 raw** → 结束后仍返回 Pipeline 错误 |

已解除的是 DOA / BF / CSV / Viewer 对采集的回压。三个 raw WAV 仍由 recorder **同步写入**，
磁盘卡顿仍可能导致 capture queue overrun。

## 固件与通道布局

配合 `6_channels_firmware.bin` 使用：

| 通道 | 内容 | 录音文件 |
|------|------|----------|
| ch0 | 固件算法处理音频 | `*_respeaker_algo.wav` |
| ch1–ch4 | mic1–mic4 原始数据 | `*_respeaker_mic.wav`（4ch） |
| ch5 | 回放/AEC 参考 | `*_respeaker_ref.wav` |
| — | Beamformer 输出（可选） | `*_respeaker_bf.wav`（mono；`compare_wav = true` 时为双声道：左=mic1、右=BF。两边都先乘 `output_gain_db`，若 `enable_drc` 再过 DRC） |

输出为标准 16-bit PCM `WAVEFORMATEX`，不写入扬声器布局 channel mask。

## 构建与运行

```powershell
cargo build --release

target\release\respeaker_algo.exe
```

启动后可在“运行”页选择原始录音、声源定位、自动拾音或定向拾音，编辑录音设置后直接开始。开始时页面会先同步当前设置；录音中修改的内容将在下一次录音时生效。需要保留设置时，使用“保存设置”写入配置文件。
配置保存在 `%LOCALAPPDATA%\respeaker_algo\config.toml`，录音输出目录和时长由页面配置。
录音中的配置修改只对下一次 session 生效；Ctrl+C 会先停止录音并完成 WAV/manifest finalize。

## Pipeline 配置

`[[modules]]` 按声明顺序执行。`direction_source = "doa"` 的 Beamformer 必须位于 enabled DOA
之后。最多各启用一个 DOA 与一个 Beamformer。现有 `configs/*.toml` 仍作为 preset/import 示例，
正常操作不需要手写配置文件。

示例：

- [configs/doa.toml](configs/doa.toml) — 仅 DOA
- [configs/doa_bf.toml](configs/doa_bf.toml) — DOA + robust superdirective BF（15 dB 输出增益 + 输出 DRC，双声道对比）
- [configs/bf_fixed.toml](configs/bf_fixed.toml) — 固定角 Delay-and-Sum（15 dB 输出增益 + 输出 DRC，双声道对比）

Beamformer 可省略字段的默认值：`algorithm = robust_superdirective`、
`direction_source = doa`、`direction_smoothing_ms = 64`、`min_wng_db = 3`、
频带 `350/500/2500/3500` Hz、`output_gain_db = -3`、`wav = true`、
`compare_wav = false`、`enable_drc = true`。

## 角度坐标

阵列内部物理角（DOA tracker / BF steering）：

```text
+X = 0°，+Y = 90°，逆时针增加
```

对外角（终端 / CSV / Viewer JSON）：

```text
output = wrap360(angle_offset_deg + (clockwise ? -internal : internal))
```

Viewer 罗盘另将显示坐标顺时针旋转 90°（0° 朝下）；**不得**把该旋转或 external 角送入 BF。

## 实时 DOA

```text
512 点 Hann / 256 hop；前两帧只更新 EMA；约 1024 samples（64 ms）才有第一条 DoaResult
```

worker 的 256-frame hop assembler **不替代** DOA 内部 `FrameAssembler`。

## Beamformer

两种算法：

1. **Delay-and-Sum** — 正确性基线与数值 fallback
2. **WNG-constrained robust superdirective MVDR** — 使用理论 3D diffuse covariance，
   **不是**在线样本协方差 MVDR

STFT：512 / 256，periodic sqrt-Hann WOLA；约 32 ms 处理延迟（文件 sample 0 仍对齐输入
sample 0）。`direction_source = doa` 时在 Searching / 无结果阶段使用
`fallback_internal_angle_deg`。

当前限制：单目标、二维远场、固定 4-Mic 几何、无在线 VAD/噪声协方差；第一版不提供浏览器
实时 PCM 监听、局域网访问或账户系统。

## Windows WASAPI 独占

- 设备不能被其它程序占用；
- 固件格式固定 16 kHz/6ch，channel mask 必须为 0；
- `PollingExclusive`、100 ms 缓冲、1 ms 轮询；
- capture 使用非阻塞 `try_send`，queue Full 为致命 overrun。

## 代码结构

```text
src/main.rs              常驻 Web 服务入口
src/audio.rs             CaptureBlock / 设备常量
src/app_config.rs        AppConfig / profiles / presets
src/controller.rs         状态机和 session ownership
src/events.rs             EventBus / SSE envelope / snapshot
src/recorder.rs          raw WAV + Pipeline 降级调度
src/pipeline_worker.rs   algorithm worker / hop assembler
src/pipeline.rs          TOML 与模块串联
src/beamformer/          STFT、权重、MVDR、运行时
src/doa/                 DOA
src/recordings.rs        manifest / 历史文件 / trash
src/wasapi.rs / wav.rs / web.rs
configs/  docs/plans/  web/
```

## 验证

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

自动测试不需要声卡。真机还需验证纯录音、`doa.toml`、`doa_bf.toml`、`bf_fixed.toml`。
