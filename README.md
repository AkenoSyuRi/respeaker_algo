# respeaker_algo

ReSpeaker Mic Array v2.0（XMOS XVF-3000）录音 + 实时 DOA 工具。

当前阶段：**多通道录音 CLI + 实时 4-Mic 单声源二维 DOA**（`--doa` 可选启用，默认关闭）。
后续规划：波束成形（BF）、MIC-REF 相干抑制等。

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

# 实时 DOA（终端每 100ms 显示一次方位角）
respeaker_algo --doa

# DOA + 逐帧 CSV
respeaker_algo --doa --doa-csv

# DOA 角度方向标定示例
respeaker_algo \
  --doa \
  --doa-csv \
  --doa-beta 0.75 \
  --doa-cpsd-tau-ms 100 \
  --doa-offset-deg 90 \
  --doa-clockwise
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

## 实时 DOA

启用 `--doa` 后，基于拆分后的 `ch1..ch4`（4 路原始麦克风，**不使用** ch0 固件 DOA）
做单声源二维水平面方位角估计，每 16ms 一个内部观测，终端限速 10 Hz。
DOA **不修改**三个 WAV 文件的内容与命名。

### 固定时频配置

```text
16 kHz
512 点 Hann 窗（32 ms）
256 点 hop（16 ms）
360 个 1° 候选方向
```

### 算法链

```text
CPSD EMA → coherence-weighted PHAT-β SRP → confidence gate → circular Kalman
```

- 4 路 RFFT + 4 个 PSD / 6 个 CPSD 指数滑动平均（时间常数由 `--doa-cpsd-tau-ms` 折算 alpha）；
- 相邻 pair 频带上限 3500 Hz、对径 pair 上限 2500 Hz（理论混叠边界内），低频 250→400 Hz 淡入；
- 置信度由能量、MSC、主/次峰间隔、robust prominence 合成；
- 圆周恒角速度 Kalman：3 帧方向一致获取、`confidence≥0.40` 且 innovation ≤60° 更新、
  3 帧一致大跳变重置、超过 500ms 无更新回到 Searching。

### CSV 字段

`--doa-csv` 输出 `{out_dir}/{prefix}_respeaker_doa.csv`，首行：

```csv
time_ms,raw_deg,tracked_deg,confidence,status,observation_used,peak_score,second_peak_score,peak_gap_ratio,prominence,mean_msc,rms_dbfs
```

每个 16ms 内部观测一行（不做 10 Hz 限速）；`raw_deg`/`tracked_deg` 为空表示无观测/未锁定；
`status` 为 `searching`/`tracking`/`coasting`。

### 角度定义与标定

内部数学坐标：0° = +X、90° = +Y、逆时针增加。对外输出：

阵列中心为原点，相邻麦克风中心距为 45.7 mm；`ch1..ch4` 对应的坐标为：

```text
mic1/ch1 = (+22.85, -22.85) mm   第四象限
mic2/ch2 = (+22.85, +22.85) mm   第一象限
mic3/ch3 = (-22.85, +22.85) mm   第二象限
mic4/ch4 = (-22.85, -22.85) mm   第三象限
```

```text
output = wrap360(angle_offset_deg + (clockwise ? -internal : internal))
```

默认 `--doa-offset-deg 0`、非顺时针。设备外壳/丝印/LED 的实际 0° 方向需实测标定：
建议现场把声源固定在已知方向，调整 `--doa-offset-deg` 与 `--doa-clockwise` 使
raw 角度与真实方向一致。

### 限制

- 单声源、二维水平面、远场近似；不做多人定位/波束形成/AEC；
- 仅支持 16kHz/6ch 原始输入（Windows 需 WASAPI 独占后端才有完整 6 通道）；
- 32 ms 分析窗带来首帧等待，CPSD warmup（2 帧）与获取门控（3 帧）还会增加首次锁定时间；
- 默认阈值（acquire 0.65 / update 0.40）按真实环境调参，勿凭本地录音随意改动；
  合成信号测试使用放宽的测试阈值。

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
  main.rs      CLI 入口（默认录制模式 / list-devices，DOA 参数）
  audio.rs     cpal 后端：设备枚举、设备选择、流配置查找（非 Windows 主路径）
  wasapi.rs    Windows WASAPI 独占后端（设备枚举、6ch/16k 采集循环）
  recorder.rs  后端分流、采集 → 拆分 → 3 文件 WAV 写入 + DOA 串行处理
  wav.rs       标准 WAVEFORMATEX 流式写入（无通道布局元信息）
  doa/
    mod.rs      DoaConfig/DoaResult/DoaProcessor 编排
    framer.rs   任意块长 → 512/256 分帧
    geometry.rs 阵列坐标、pair、频带权重、steering LUT
    srp.rs      FFT、PSD/CPSD EMA、PHAT-β SRP、置信度
    tracker.rs  置信度门控状态机 + 圆周 Kalman
    output.rs   DoaRuntime：10 Hz 终端 + 逐帧 CSV
```

- DOA 与录音在同一消费线程串行执行（不新增线程、不修改采集回调）；
- 16ms 帧 = 256 采样 @16kHz，DOA 与 WAV 拆分共用 `recorder::split_6ch_into` 的
  `ch1..ch4` 数据。

## 测试

```bash
cargo test
```
