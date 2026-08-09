# respeaker_algo

ReSpeaker Mic Array v2.0（XMOS XVF-3000）录音与语音算法工具。

当前阶段：**多通道录音 CLI**。后续规划：16ms 帧实时 DOA、波束成形（BF）等。

## 固件与通道布局

配合 `6_channels_firmware.bin`（出厂固件）使用。设备以 USB Audio Class 1.0
声卡形式出现（免驱），输出 **16kHz / 16-bit / 6 通道**交织 PCM：

| 通道 | 内容 |
|------|------|
| ch0 | 算法处理后的音频（供 ASR 使用） |
| ch1 – ch4 | mic1 – mic4 原始数据（DOA / BF 算法输入） |
| ch5 | 回放数据（AEC 参考信号） |

固件升级与参数调优（DOAANGLE、AGC 等）参考
[Seeed Studio Wiki](https://wiki.seeedstudio.com/cn/ReSpeaker_Mic_Array_v2.0/)。

## 构建

```bash
cargo build --release
```

## 用法

```bash
# 默认：直接启动录制，直到 Ctrl+C 停止
# 输出 target/out/{时间戳}_respeaker_{algo,mic,ref}.wav 三个文件
respeaker_algo

# 录制 10 秒后自动停止（--duration / -d）
respeaker_algo --duration 10

# 自定义输出目录 / 文件前缀
respeaker_algo --duration 10 --out-dir target/out --prefix mysession

# 按设备索引/名称关键字指定设备
respeaker_algo --device 0 --duration 10

# 列出音频输入设备（Windows 用 WASAPI 视图；--verbose 打印设备 ID / 支持配置）
respeaker_algo list-devices
respeaker_algo list-devices --verbose

# 强制指定采集后端（auto / cpal / wasapi）
respeaker_algo --backend wasapi
```

### 输出文件

`6_channels_firmware.bin` 的 6 通道在录制时被拆分为 3 个文件（同前缀）：

| 文件 | 通道 | 内容 |
|------|------|------|
| `{前缀}_respeaker_algo.wav` | 1 | ch0：算法处理后的音频（供 ASR） |
| `{前缀}_respeaker_mic.wav` | 4 | ch1–4：mic1–mic4 原始数据（DOA/BF 输入） |
| `{前缀}_respeaker_ref.wav` | 1 | ch5：回放数据（AEC 参考） |

`{前缀}` 默认为启动时刻的时间戳（`YYYYMMDD_HHMMSS`），可用 `--prefix` 覆盖。

> 通道元信息：输出为标准 16-bit PCM `WAVEFORMATEX`（不含 `WAVEFORMATEXTENSIBLE`
> 的 `dwChannelMask` 通道布局），播放器/分析软件会将 `*_respeaker_mic.wav`
> 的 4 个通道显示为 **1、2、3、4**，而不是 L/R/C/LFE。

## 采集后端

| 后端 | 平台 | 说明 |
|------|------|------|
| `wasapi`（默认，Windows） | Windows | WASAPI 独占模式，直接以 16-bit/6ch/16kHz 初始化 `IAudioClient`，拿到固件完整 6 通道数据。独占模式要求设备未被其它程序占用 |
| `cpal` | 全平台 | 跨平台共享模式。Linux（ALSA）可直接拿到 6ch/16k；Windows 上系统混音器只暴露 48kHz/2ch（拿不到 mic 原始通道） |

`--backend auto`（默认）：Windows 走 `wasapi`，其它平台走 `cpal`。

### Windows WASAPI 独占的已知坑（已踩平）

1. **channel mask 必须为 0**：该设备（usbaudio.sys）在独占模式下显式指定 5.1 布局
   （`0x3F`）会返回 `AUDCLNT_E_UNSUPPORTED_FORMAT`；且 `wasapi::WaveFormat::new`
   传 `None` 会自动生成 `0x3F`，必须显式传 `Some(0)`。
2. **`GetNextPacketSize` 在独占模式下不可用**：WASAPI 中该调用仅共享模式有效，
   wasapi-rs 对独占模式直接返回 `None`。独占模式应直接 `GetBuffer`/`read_from_device`，
   返回 0 帧表示无更多数据。
3. **不要用 `EventsExclusive`**：该模式强制 buffer == period（3ms 缓冲过小），
   实测事件节拍异常、收不到数据。用 `PollingExclusive`（buffer 与 period 分离，
   100ms 缓冲 + 1ms 轮询）。

## 代码结构

```
src/
  main.rs      CLI 入口（默认录制模式 / list-devices）
  audio.rs     cpal 后端：设备枚举、设备选择、流配置查找（非 Windows 主路径）
  wasapi.rs    Windows WASAPI 独占后端（设备枚举、6ch/16k 采集循环）
  recorder.rs  后端分流、采集 → 拆分 → 3 文件 WAV 写入
```

- 16ms 帧 = 256 采样 @16kHz，后续 DOA/BF 按此对齐消费 ch1–ch4；
- `recorder::split_6ch` 提供 6 通道交织 → algo/mic/ref 拆分，算法模块可复用。

## 测试

```bash
cargo test
```
