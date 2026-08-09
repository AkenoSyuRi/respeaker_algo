# ReSpeaker 4-Mic 实时 DOA 集成实施方案

> 本文档用于让 Codex 直接在仓库中完成实现，不是仅供讨论的设计草案。

## 0. 执行要求

仓库：`https://github.com/AkenoSyuRi/respeaker_algo`

编写本文档时的基线：

```text
branch: master
commit: 95abb03a2672c5f4ec2397e490429b5c241a331d
```

Codex 必须遵守以下规则：

1. 先检查仓库根目录及相关子目录中的 `AGENTS.md`、`README.md` 和现有代码，再修改。
2. 直接完成代码、测试和 README 更新，不要只输出分析或伪代码。
3. 若当前 `HEAD` 已不是上述 commit，基于当前代码适配本方案；不得 `reset --hard`、删除或覆盖用户已有改动。
4. 保留现有录音行为和输出格式；DOA 默认关闭，只有显式传入 `--doa` 才启用。
5. 不修改 WASAPI/CPAL 的采集机制，不把 FFT、SRP 或 Kalman 放进音频采集回调。
6. 不引入神经网络、波束形成、AEC、多人定位或 GUI；这些不在本次范围内。
7. 不创建不必要的抽象或依赖。FFT 使用 `realfft`；Kalman、CSV 输出和角度运算自行实现。
8. 完成后运行本文档要求的格式化、静态检查、测试和 release 构建。
9. 不提交、不推送、不创建 PR，除非用户另行要求。

---

## 1. 目标

在现有 ReSpeaker Mic Array v2.0 六通道录音程序中，增加一个可选的实时单声源二维 DOA 模块：

```text
PHAT-β 可靠性加权 SRP
+ CPSD/PSD EMA
+ 多指标置信度
+ 置信度门控
+ 圆周恒角速度 Kalman
```

目标能力：

- 输入：`ch1..ch4` 的 4 路原始麦克风，16 kHz、16-bit PCM；
- 输出：0～360° 水平面方位角；
- 单声源、远场近似；
- 32 ms 分析窗、16 ms 帧移；
- 每 16 ms 生成一次内部观测，终端限速到 10 Hz；
- 可选保存逐帧 DOA CSV；
- 不改变三个现有 WAV 文件的内容、通道顺序和命名方式；
- 录音和 DOA 在同一消费线程内串行执行，第一版不额外创建 DOA 线程。

处理链路：

```text
WASAPI / CPAL 采集线程
        │
        ▼
sync_channel<Vec<i16>>                    任意块长、6ch 交织
        │
        ▼
recorder::write_split_loop
        │
        ├── ch0       ───────────────────► *_respeaker_algo.wav
        ├── ch1..ch4 ────────────────────► *_respeaker_mic.wav
        ├── ch5       ───────────────────► *_respeaker_ref.wav
        │
        └── ch1..ch4
                │
                ▼
        DoaProcessor
          ├── 任意块长 → 512 点窗 / 256 点步长
          ├── 4 路 RFFT
          ├── PSD/CPSD EMA
          ├── 相干度与频带可靠性权重
          ├── PHAT-β SRP 360° 搜索
          ├── 峰值、次峰、prominence、MSC、能量置信度
          ├── 获取/更新/跳变/丢失门控
          └── 圆周 Kalman
                │
                ├── 10 Hz 终端显示
                └── 可选 *_respeaker_doa.csv
```

---

## 2. 固定参数与设计选择

本次实现使用以下固定时频参数，不提供窗长和帧移 CLI：

```rust
pub const SAMPLE_RATE: u32 = 16_000;
pub const MIC_COUNT: usize = 4;
pub const FRAME_SIZE: usize = 512; // 32 ms
pub const HOP_SIZE: usize = 256;   // 16 ms
pub const FFT_BINS: usize = FRAME_SIZE / 2 + 1; // 257
pub const ANGLE_COUNT: usize = 360;             // 0..359°
```

选择 32/16 而不是 16/8 的原因：

- 单帧互谱统计更稳定；
- 频率分辨率从 62.5 Hz 提升为 31.25 Hz；
- 300～3500 Hz 内有更多频率点参与投票；
- 62.5 次/s 的内部更新率已经足够跟踪人的转头和移动；
- 增加的约 16 ms 首帧等待对 DOA 显示和方向控制可以接受。

默认算法参数：

```text
speed_of_sound           = 343.0 m/s
scan_step                = 1°
PHAT beta                = 0.75
CPSD EMA time constant   = 100 ms
CPSD warmup frames       = 3
minimum frequency        = 250～400 Hz 平滑淡入
adjacent-pair maximum    = 3500 Hz，末端 250 Hz 平滑淡出
opposite-pair maximum    = 2500 Hz，末端 250 Hz 平滑淡出
coherence gate           = smoothstep(0.15, 0.55, |rho|)
second-peak exclusion    = ±20°
terminal output interval = 100 ms
acquire confidence       = 0.65
update confidence        = 0.40
jump candidate confidence= 0.65
acquire/jump frames      = 3
acquire consistency      = 20°
normal innovation gate   = 60°
maximum coast time       = 500 ms
```

所有时间相关系数必须由秒数和实际 hop 计算，不得把 EMA `alpha` 写死：

```rust
let hop_seconds = HOP_SIZE as f32 / SAMPLE_RATE as f32;
let alpha = (-hop_seconds / tau_seconds).exp();
```

100 ms 时间常数、16 ms hop 时：

```text
alpha ≈ exp(-0.016 / 0.100) ≈ 0.8521
```

---

## 3. ReSpeaker 阵列几何和角度定义

拆分后的 `mic` 数据顺序是原始 USB `ch1..ch4`，对应以下平面坐标，单位为米：

```rust
pub const RESPEAKER_V2_MICS_M: [[f32; 2]; MIC_COUNT] = [
    [ 0.02285, -0.02285], // mic1/ch1，第四象限
    [ 0.02285,  0.02285], // mic2/ch2，第一象限
    [-0.02285,  0.02285], // mic3/ch3，第二象限
    [-0.02285, -0.02285], // mic4/ch4，第三象限
];

pub const MIC_PAIRS: [(usize, usize); 6] = [
    (0, 1), // adjacent
    (0, 2), // opposite
    (0, 3), // adjacent
    (1, 2), // adjacent
    (1, 3), // opposite
    (2, 3), // adjacent
];
```

几何检查：

```text
相邻基线 = 0.0457 m
相对基线 = sqrt(2) * 0.0457 ≈ 0.06463 m
```

理论空间混叠频率约为：

```text
adjacent: 343 / (2 * 0.0457)  ≈ 3753 Hz
opposite: 343 / (2 * 0.06463) ≈ 2654 Hz
```

因此本方案保守使用：

```text
adjacent pair: 最高 3500 Hz
opposite pair: 最高 2500 Hz
```

### 3.1 内部角度定义

内部统一使用数学坐标：

- `0°` 指向 `+X`；
- `90°` 指向 `+Y`；
- 角度逆时针增加；
- 内部弧度范围可连续展开；对外显示时 wrap 到 `[0, 360)`。

设备外壳、丝印或 LED 的实际 0° 方向需要实测标定。对外角度转换必须集中在一个函数中：

```rust
output_deg = wrap_360(
    angle_offset_deg
        + if clockwise { -internal_deg } else { internal_deg }
);
```

默认：

```text
angle_offset_deg = 0
clockwise        = false
```

统一提供以下角度辅助函数，所有模块复用同一实现：

```rust
fn wrap_360(deg: f32) -> f32 {
    deg.rem_euclid(360.0)
}

fn circular_delta_deg(a: f32, b: f32) -> f32 {
    (a - b + 180.0).rem_euclid(360.0) - 180.0
}

fn circular_distance_deg(a: f32, b: f32) -> f32 {
    circular_delta_deg(a, b).abs()
}
```

多个候选角的圆周均值使用：

```text
mean = atan2(sum(sin(theta)), sum(cos(theta)))
```

禁止在 SRP、Kalman、CSV 和终端输出的不同位置分别做方向变换。

---

## 4. 依赖修改

在 `Cargo.toml` 的 `[dependencies]` 中增加：

```toml
realfft = "3.5"
```

使用：

```rust
use realfft::num_complex::Complex32;
use realfft::{RealFftPlanner, RealToComplex};
```

不要额外引入：

- `nalgebra`；
- `csv`；
- `serde`；
- `rayon`；
- VAD 或音频 DSP 大型依赖。

CSV 使用 `std::io::BufWriter` 和 `writeln!`；2×2 Kalman 手写。

---

## 5. 目标文件结构

新增：

```text
src/doa/
  mod.rs          公共接口、DoaProcessor、DoaRuntime 编排
  framer.rs       任意块长 4ch 交织 PCM → 512/256 分帧
  geometry.rs     阵列坐标、pair、频带和 steering LUT
  srp.rs          FFT、PSD/CPSD EMA、PHAT-β SRP、置信度
  tracker.rs      门控状态机和圆周 Kalman
  output.rs       CSV 与 10 Hz 终端输出
```

修改：

```text
Cargo.toml
src/main.rs
src/recorder.rs
README.md
```

不要修改：

```text
src/audio.rs
src/wasapi.rs
src/wav.rs
```

除非编译所必需；若必须修改，应保持现有行为并在最终总结中说明原因。

---

## 6. 公共数据结构

在 `src/doa/mod.rs` 中提供以下等价接口。字段名允许为 Rust 风格微调，但语义必须保持。

```rust
pub struct DoaConfig {
    pub sample_rate: u32,
    pub beta: f32,
    pub cpsd_tau_ms: f32,
    pub angle_offset_deg: f32,
    pub clockwise: bool,

    pub acquire_confidence: f32,
    pub update_confidence: f32,
    pub max_coast_ms: u32,
}

impl Default for DoaConfig {
    fn default() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            beta: 0.75,
            cpsd_tau_ms: 100.0,
            angle_offset_deg: 0.0,
            clockwise: false,
            acquire_confidence: 0.65,
            update_confidence: 0.40,
            max_coast_ms: 500,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackStatus {
    Searching,
    Tracking,
    Coasting,
}

#[derive(Clone, Debug)]
pub struct DoaResult {
    /// 当前分析窗最后一个采样点所对应的捕获时间。
    pub timestamp_ms: f64,

    /// SRP 原始角度，已做 offset/clockwise 转换；无有效观测时为 None。
    pub raw_angle_deg: Option<f32>,

    /// Kalman 输出，已做 offset/clockwise 转换；尚未获取目标时为 None。
    pub tracked_angle_deg: Option<f32>,

    pub confidence: f32,
    pub status: TrackStatus,
    pub observation_used: bool,

    pub peak_score: f32,
    pub second_peak_score: f32,
    pub peak_gap_ratio: f32,
    pub prominence: f32,
    pub mean_msc: f32,
    pub rms_dbfs: f32,
}

pub struct DoaProcessor {
    // FrameAssembler
    // SrpPhatBeta
    // ConfidenceGate/CircularKalman
    // reusable scratch buffers
}

impl DoaProcessor {
    pub fn new(config: DoaConfig) -> Result<Self, String>;

    /// `mics_interleaved` 为 mic0,mic1,mic2,mic3 循环交织的 i16 PCM。
    /// 本函数允许任意块长，但长度必须是 4 的整数倍。
    /// 每产生一个 512/256 分析帧，就向 `results` 追加一个结果。
    /// 调用方负责在调用前 clear，以复用容量。
    pub fn push_interleaved(
        &mut self,
        mics_interleaved: &[i16],
        results: &mut Vec<DoaResult>,
    ) -> Result<(), String>;
}
```

`DoaConfig::validate()` 或 `DoaProcessor::new()` 必须检查：

```text
sample_rate == 16000
0.0 <= beta <= 1.0
cpsd_tau_ms > 0
0 <= acquire_confidence <= 1
0 <= update_confidence <= 1
update_confidence <= acquire_confidence
max_coast_ms > 0
所有浮点参数有限，不允许 NaN/Inf
```

---

## 7. 固定分帧器

### 7.1 需求

当前采集端发送的 `Vec<i16>` 长度不固定，不能把每个采集包直接当成一个 STFT 帧。必须跨包维护 4 路环形缓冲。

建议接口：

```rust
pub struct FrameAssembler {
    ring: [[f32; FRAME_SIZE]; MIC_COUNT],
    write_pos: usize,
    filled: usize,
    samples_since_emit: usize,
    total_samples: u64,
}

impl FrameAssembler {
    pub fn new() -> Self;

    /// 写入同一时刻的 4 路归一化采样。
    /// 首次累计满 512 点时返回 true；以后每新增 256 点返回 true。
    pub fn push(&mut self, sample: [f32; MIC_COUNT]) -> bool;

    /// 按时间先后复制当前 512 点窗；out[通道][时间]。
    pub fn copy_frame(&self, out: &mut [[f32; FRAME_SIZE]; MIC_COUNT]);

    pub fn total_samples(&self) -> u64;
}
```

### 7.2 分帧规则

1. 将 i16 转为：

```rust
let x = sample as f32 / 32768.0;
```

2. 第 512 个采样写入后立即产生第一帧；
3. 之后在第 768、1024、1280……个采样后产生帧；
4. `copy_frame()` 输出必须从最旧采样到最新采样；
5. 输入块边界不能影响输出帧；
6. `mics_interleaved.len() % 4 != 0` 必须返回错误，不得静默丢弃尾部样本。

### 7.3 时间戳

结果时间戳定义为当前分析帧的末端采样时间，即结果在实时系统中可获得的时间：

```rust
let timestamp_ms =
    frame_end_sample as f64 * 1000.0 / SAMPLE_RATE as f64;
```

第一帧时间戳应为 32 ms，而不是 16 ms。

---

## 8. FFT 与预处理

`SrpPhatBeta` 初始化时：

```rust
let mut planner = RealFftPlanner::<f32>::new();
let fft = planner.plan_fft_forward(FRAME_SIZE);
```

一次创建并复用：

- 4 路 FFT 输入缓冲；
- 4 路 257-bin 复数输出缓冲；
- 一个可供 4 路顺序复用的 FFT scratch；
- Hann 窗；
- PSD/CPSD 状态；
- SRP score；
- median/MAD scratch；
- 每帧加权复投票项。

热路径中不得为每个角度、pair 或 bin 分配新 `Vec`。FFT 必须调用 `process_with_scratch()`，不要在每通道每帧使用可能重新准备 scratch 的便捷路径：

```rust
let mut scratch = fft.make_scratch_vec();
// 四个通道依次复用 scratch
fft.process_with_scratch(&mut input[ch], &mut spectrum[ch], &mut scratch)?;
```

每通道每帧处理：

1. 计算 512 点均值；
2. 原始帧减均值；
3. 同时使用减均值后的未加窗数据计算 4 通道总体 RMS；
4. 乘 Hann 窗；
5. 执行 512 点 RFFT。

Hann 窗统一使用：

```rust
w[n] = 0.5 - 0.5 * cos(2*pi*n/(FRAME_SIZE-1))
```

FFT 无需做幅度归一化，因为后续使用相干度，公共缩放会抵消；但不得在不同通道采用不同缩放。

总体 RMS：

```text
mean_square = 所有通道、所有 512 点减均值样本平方的平均
rms         = sqrt(mean_square)
rms_dbfs    = 20 * log10(max(rms, 1e-12))
```

---

## 9. PSD/CPSD EMA

共有：

```text
4 个自功率谱 PSD
6 个互功率谱 CPSD
257 个频点
```

互谱约定必须统一为：

```rust
cpsd_ij = X_i * X_j.conj();
```

更新公式：

```text
P_i(k,t) = alpha * P_i(k,t-1)
         + (1-alpha) * |X_i(k,t)|²

C_ij(k,t) = alpha * C_ij(k,t-1)
          + (1-alpha) * X_i(k,t) * conj(X_j(k,t))
```

第一帧不能从全零状态按 EMA 慢慢爬升，必须直接初始化：

```text
P_i(k,0)  = |X_i(k,0)|²
C_ij(k,0) = X_i(k,0) * conj(X_j(k,0))
```

随后正常 EMA。

第 1～2 个分析帧只更新状态，不允许进入跟踪器获取目标；累计到第 3 帧后开始生成正常结果并允许进入获取门控。测试和 README 必须与该行为一致。

复相干度：

```text
rho_ij(k) = C_ij(k) / sqrt(P_i(k) * P_j(k) + eps)
gamma     = clamp(|rho_ij(k)|, 0, 1)
MSC       = gamma²
```

建议：

```rust
const EPS_POWER: f32 = 1e-12;
```

若 PSD 非有限或过小，该频点本帧不参与投票。

---

## 10. 频带权重

对每个 pair 预计算频带权重。频率：

```rust
f_hz = bin as f32 * SAMPLE_RATE as f32 / FRAME_SIZE as f32;
```

定义：

```rust
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge0 == edge1 {
        return if x < edge0 { 0.0 } else { 1.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
```

低频权重：

```text
0                 : f <= 250 Hz
smoothstep        : 250 < f < 400 Hz
1                 : f >= 400 Hz
```

高频权重：

```text
adjacent pair:
  1                 : f <= 3250 Hz
  1-smoothstep      : 3250 < f < 3500 Hz
  0                 : f >= 3500 Hz

opposite pair:
  1                 : f <= 2250 Hz
  1-smoothstep      : 2250 < f < 2500 Hz
  0                 : f >= 2500 Hz
```

最终：

```text
band_weight = low_frequency_weight * high_frequency_weight
```

只把 `band_weight > 0` 的 pair/bin 项放入 steering 和每帧投票项中。

---

## 11. PHAT-β 与可靠性权重

本实现使用相干度归一的 PHAT-β，避免绝对 PCM 幅度改变频点权重。

对每个有效 pair/bin：

```text
rho        = CPSD / sqrt(PSD_i * PSD_j + eps)
gamma      = clamp(|rho|, 0, 1)
unit_phase = rho / max(gamma, eps)
```

相干度门控：

```text
coherence_gate = smoothstep(0.15, 0.55, gamma)
```

PHAT-β 幅度：

```text
beta_weight = gamma^(1-beta)
```

最终投票权重：

```text
vote_weight = band_weight * coherence_gate * beta_weight
```

最终复投票项：

```text
weighted_vote = unit_phase * vote_weight
```

性质：

```text
beta = 1 → 通过门控后的纯 PHAT 相位投票
beta = 0 → 相干度幅度完整参与投票
beta = 0.75 → 默认的部分白化
```

禁止直接使用未归一化 `|CPSD|^(1-beta)`，否则输入增益和 FFT 缩放会改变 DOA 权重。

`mean_msc` 应按 `band_weight` 对 `gamma²` 加权平均，但不要用 `coherence_gate` 参与其分母，否则低相干频点被先剔除后会虚高：

```text
mean_msc = sum(band_weight * gamma²) / sum(band_weight)
```

PSD/CPSD 无效的项不进入分子和分母。

---

## 12. Steering LUT 和 SRP 扫描

### 12.1 到达时差和符号约定

候选方向：

```text
u(theta) = [cos(theta), sin(theta)]
```

pair 时差参数：

```text
tau_ij(theta) = dot(r_i - r_j, u(theta)) / c
```

由于互谱定义为：

```text
C_ij = X_i * conj(X_j)
```

steering 相位必须是：

```text
steer_ij(k,theta) = exp(-j * 2*pi*f_k*tau_ij(theta))
```

得分：

```text
score(theta) =
    sum Re(weighted_vote_ij(k) * steer_ij(k,theta))
    ------------------------------------------------
                 sum vote_weight_ij(k) + eps
```

这组符号不得随意更改；必须通过合成方向测试验证。

### 12.2 LUT 数据布局

初始化时预计算：

```text
360 angle × 所有有效 pair/bin 的 Complex32 steering
```

推荐布局：

```rust
steering[angle_index * term_count + term_index]
```

其中 `term_index` 与本帧 `weighted_vote[term_index]` 完全一致，便于顺序连续读取。

每帧：

1. 先计算所有 `weighted_vote` 和 `vote_weight_sum`；
2. 若 `vote_weight_sum` 太小或非有限，则本帧无有效观测；
3. 扫描 360 个角度；
4. 每个角度除以同一个 `vote_weight_sum`；
5. 所有 score 必须有限，异常时本帧无效而不是 panic。

---

## 13. 峰值与亚角度插值

### 13.1 主峰

在 360 个 score 中找到最大值：

```text
peak_index
peak_score
```

### 13.2 三点抛物线插值

使用循环邻居：

```text
y_minus = score[(peak_index + 359) % 360]
y0      = score[peak_index]
y_plus  = score[(peak_index + 1) % 360]
```

偏移：

```text
den = y_minus - 2*y0 + y_plus

delta = if |den| > eps {
    0.5 * (y_minus - y_plus) / den
} else {
    0
}

delta = clamp(delta, -0.5, 0.5)
raw_internal_deg = wrap_360(peak_index + delta)
```

### 13.3 次峰

从主峰中心排除循环角距离 `<= 20°` 的候选，在剩余角度中寻找最大值：

```text
second_peak_score
```

不得简单使用排序后的第二大数组元素，因为它通常仍处于同一主瓣。

### 13.4 峰值间隔

```text
peak_gap_ratio =
    max(peak_score - second_peak_score, 0)
    / max(abs(peak_score), 1e-6)
```

---

## 14. Robust prominence 和置信度

复用 360 长度 scratch：

1. 复制 score 并用 `f32::total_cmp` 排序，取 median；
2. 计算 `abs(score-median)`；
3. 再排序取 MAD；
4. 不允许在 score 中存在 NaN；遇到 NaN 时本帧判无效。

```text
prominence = (peak_score - median) / max(MAD, 1e-6)
```

各子质量：

```text
q_energy = smoothstep(-60.0, -40.0, rms_dbfs)
q_msc    = smoothstep( 0.15,  0.55, mean_msc)
q_gap    = smoothstep( 0.02,  0.15, peak_gap_ratio)
q_prom   = smoothstep( 2.0,   6.0,  prominence)
```

最终置信度：

```text
spectral_quality = cbrt(q_msc * q_gap * q_prom)
confidence       = clamp(q_energy * spectral_quality, 0, 1)
```

注意：

- 能量门只用于置信度，不改变 CPSD EMA 更新；
- 无效/全零/非有限帧的 confidence 必须为 0；
- `raw_angle_deg` 可以保留诊断值，但只有通过门控的观测才允许更新 Kalman；
- 初始阈值必须放在 `DoaConfig` 或清晰的常量中，便于后续调参；
- 不要根据一段本地录音偷偷修改默认阈值而不在 README 中记录。

---

## 15. 圆周 Kalman

### 15.1 状态

使用恒角速度模型：

```text
x = [theta_unwrapped, omega]
```

其中：

- `theta_unwrapped` 是连续弧度，不强制限制到 `[-pi, pi)`；
- `omega` 单位 rad/s；
- 对外输出时再 wrap。

状态转移：

```text
F = [[1, dt],
     [0,  1]]
```

`dt = HOP_SIZE / SAMPLE_RATE = 0.016 s`。

使用白噪声角加速度模型：

```text
Q = sigma_a² *
    [[dt⁴/4, dt³/2],
     [dt³/2, dt²  ]]
```

默认：

```text
sigma_a = 360 deg/s²
```

### 15.2 圆周观测残差

```rust
fn wrap_pi(x: f32) -> f32 {
    (x + std::f32::consts::PI)
        .rem_euclid(std::f32::consts::TAU)
        - std::f32::consts::PI
}
```

更新前先预测，再计算：

```text
innovation = wrap_pi(measured_wrapped - predicted_wrapped)
measurement_unwrapped = predicted_unwrapped + innovation
```

因此：

```text
预测 359°，观测 1° → innovation = +2°
```

不得直接对 0～360° 数字做普通差值。

### 15.3 动态测量噪声

```text
sigma_meas_deg = 3 + 22 * (1-confidence)²
R              = deg_to_rad(sigma_meas_deg)²
```

高置信度观测更强地更新轨迹，低置信度观测更弱。

### 15.4 初始化和约束

获取目标时：

```text
theta = measured angle
omega = 0
P angle std    = 10°
P velocity std = 180°/s
```

每次预测/更新后：

```text
omega clamp 到 ±360°/s
P 保持对称
所有状态必须有限
```

Coasting 且无观测更新时可对角速度施加轻微阻尼：

```text
omega *= 0.98
```

只在无观测时阻尼，不要在正常连续跟踪时每帧强制衰减。

---

## 16. 置信度门控状态机

跟踪状态：

```text
Searching
Tracking
Coasting
```

### 16.1 Searching → Tracking

只有：

```text
confidence >= 0.65
```

的观测才是获取候选。

获取条件：

1. 连续 3 个候选观测；
2. 相邻候选的循环角距离不超过 20°；
3. 候选角度使用单位圆向量平均，不能直接平均 359° 和 1°；
4. 满足后用候选圆周均值初始化 Kalman，进入 `Tracking`。

任一帧置信度不足或候选方向不一致，重新开始计数。

### 16.2 Tracking 正常更新

每帧先预测。

若：

```text
confidence >= 0.40
且 |innovation| <= 60°
```

则执行 Kalman 更新：

```text
status = Tracking
coast_time = 0
observation_used = true
```

否则不更新，进入或保持 `Coasting`。

### 16.3 大角度跳变

高置信度观测若与预测相差超过 60°，不能立即把 Kalman 拉过去，也不能永久忽略。建立独立 jump candidate：

```text
confidence >= 0.65
连续 3 帧
候选之间循环角距离 <= 20°
```

满足后认为说话人切换或方向真实突变，直接以候选圆周均值重置 Kalman并进入 `Tracking`。

跳变候选不满足连续性时重新计数。

### 16.4 Coasting 和丢失

无有效更新时：

- 继续 Kalman predict；
- `status = Coasting`；
- `observation_used = false`；
- `tracked_angle_deg` 继续输出预测值；
- 累加 coast 时间。

超过 500 ms：

```text
status = Searching
tracked_angle_deg = None
清空 acquire/jump candidate
重置 Kalman initialized 状态
```

不得在无语音时用低置信度原始角度持续更新轨迹。

---

## 17. 运行时输出

在 `src/doa/output.rs` 实现 `DoaRuntime` 或等价结构，拥有：

```text
DoaProcessor
复用的 Vec<DoaResult>
可选 BufWriter<File>
终端上次输出时间
```

建议接口：

```rust
pub struct DoaRuntime {
    processor: DoaProcessor,
    results: Vec<DoaResult>,
    csv: Option<BufWriter<File>>,
    last_console_timestamp_ms: f64,
}

impl DoaRuntime {
    pub fn new(config: DoaConfig, csv_path: Option<&str>) -> Result<Self, String>;
    pub fn push_block(&mut self, mics_interleaved: &[i16]) -> Result<(), String>;
    pub fn finalize(&mut self) -> Result<(), String>;
}
```

### 17.1 终端

最多每 100 ms 输出一次，不得每 16 ms 打印。

推荐格式：

```text
DOA t=  12.320s raw= 82.4° track= 80.9° conf=0.78 status=tracking used=yes
```

无角度时打印 `--`。

不要求 ANSI 光标覆盖；普通逐行输出即可，保证日志和重定向可用。

### 17.2 CSV

启用 `--doa-csv` 时输出：

```text
{out_dir}/{prefix}_respeaker_doa.csv
```

首行固定为：

```csv
time_ms,raw_deg,tracked_deg,confidence,status,observation_used,peak_score,second_peak_score,peak_gap_ratio,prominence,mean_msc,rms_dbfs
```

要求：

- 每个内部 16 ms 结果写一行，不做 10 Hz 限速；
- `Option<f32>::None` 写空字段；
- 状态写 `searching`、`tracking`、`coasting`；
- 浮点数使用稳定的小数位，例如角度 3 位、置信度/指标 6 位；
- 使用 `BufWriter`；
- `finalize()` 时 flush；
- CSV 创建或写入失败返回明确错误。

---

## 18. CLI 修改

在 `src/main.rs`：

```rust
mod doa;
```

在 `Cli` 增加：

```rust
/// 启用实时 4-Mic DOA（仅支持 16kHz/6ch ReSpeaker 输入）
#[arg(long)]
doa: bool,

/// 保存逐帧 DOA CSV；该选项同时隐式启用 DOA
#[arg(long)]
doa_csv: bool,

/// PHAT 部分白化指数，范围 0..=1
#[arg(long, default_value_t = 0.75)]
doa_beta: f32,

/// CPSD/PSD EMA 时间常数，单位 ms
#[arg(long, default_value_t = 100.0)]
doa_cpsd_tau_ms: f32,

/// 输出角度旋转补偿，单位度
#[arg(long, default_value_t = 0.0)]
doa_offset_deg: f32,

/// 输出角度改为顺时针增加
#[arg(long)]
doa_clockwise: bool,
```

语义：

```text
doa_enabled = cli.doa || cli.doa_csv
```

参数无效时应在启动采集前尽量早报错，例如 beta 越界或 tau 非正。

在 `RecordOptions` 中不要散落多个 DOA 字段，增加一个清晰的配置：

```rust
pub struct DoaRunOptions {
    pub enabled: bool,
    pub csv: bool,
    pub config: DoaConfig,
}
```

然后：

```rust
pub struct RecordOptions {
    // existing fields
    pub doa: DoaRunOptions,
}
```

如果模块依赖关系更合理，也可把 `DoaRunOptions` 放到 `doa` 模块中。

---

## 19. `recorder.rs` 集成

### 19.1 不修改采集回调

保持：

```text
WASAPI/CPAL callback/thread → SyncSender<Vec<i16>>
```

DOA 只能在接收端 `write_split_loop()` 中运行。

### 19.2 初始化时机和输入限制

获得 `actual_rate`、`actual_ch` 后：

```text
若 DOA 未启用：保持原逻辑。
若 DOA 启用：必须要求 actual_rate == 16000 且 actual_ch == 6。
```

不满足时返回：

```text
实时 DOA 仅支持 ReSpeaker 16kHz/6ch 原始输入；当前为 {rate}Hz/{channels}ch
```

不要在 Windows CPAL 降级到 48k/2ch 后静默运行错误的 DOA。

DOA CSV 路径使用与 WAV 相同的 `out_dir` 和 `prefix`。

### 19.3 拆分缓冲复用

把当前每包新建 3 个 Vec 的逻辑改成容量复用版本：

```rust
fn split_6ch_into(
    samples: &[i16],
    algo: &mut Vec<i16>,
    mic: &mut Vec<i16>,
    reference: &mut Vec<i16>,
) -> Result<(), String>;
```

要求：

- `samples.len() % 6 == 0`，否则返回错误；
- 函数开始 `clear()` 三个输出；
- 必要时 `reserve()`；
- 输出顺序与现有 `split_6ch()` 完全一致；
- 更新现有单元测试。

### 19.4 主循环顺序

`write_split_loop()` 增加可选的 `&mut DoaRuntime` 或直接在内部拥有它。每次收到 samples：

```rust
split_6ch_into(...)?;

doa_runtime.push_block(&mic)?; // 若启用

algo_writer.write_samples(&algo)?;
mic_writer.write_samples(&mic)?;
ref_writer.write_samples(&reference)?;
```

DOA 处理不会修改 `mic`。

若实现时为减少录音受 DOA 错误影响而选择先写 WAV、再做 DOA，也可以，但必须保持同一采集块的顺序，且错误处理清晰。不要为此增加复杂线程。

循环结束后：

```text
先 finalize/flush DOA CSV
再完成现有 WAV finalize
```

或者相反均可，但所有错误必须被报告。

### 19.5 原有行为保持

DOA 关闭时：

- 参数默认行为不变；
- 输出文件仍只有现有三个 WAV；
- 录音数据逐样本不变；
- `list-devices` 不变；
- 非 6 通道回退路径不变；
- 不产生 DOA 日志和 CSV。

---

## 20. 数值和健壮性要求

1. 所有外部浮点配置检查 `is_finite()`；
2. 每帧 score、confidence、Kalman 状态不得传播 NaN/Inf；
3. 全零输入不得 panic，confidence 为 0，不能获取目标；
4. 长度不整除 4/6 的交织数据必须返回错误；
5. 使用 `rem_euclid` 实现 wrap，避免负角度 `%` 错误；
6. 排序浮点用 `total_cmp`，不要 `partial_cmp(...).unwrap()`；
7. 角度差全部使用循环差函数；
8. FFT 和所有大缓冲只初始化一次；
9. 每帧热点中不得创建 angle×pair×bin 的临时容器；
10. 终端输出限速，不得让日志 I/O 阻塞每个 16 ms 观测；
11. 不使用 `unsafe`；
12. 不吞掉 FFT、CSV、参数或通道布局错误。

---

## 21. 单元测试

所有测试必须可在无声卡、非 Windows 环境运行。

### 21.1 几何测试

验证：

```text
4 个坐标正确
4 个 adjacent pair 长度为 0.0457 m
2 个 opposite pair 长度约 0.06463 m
pair 无重复、i < j、覆盖 6 对组合
```

允许误差 `1e-6`。

### 21.2 分帧边界测试

输入递增序列，验证：

```text
第一帧结束样本 = 512
后续结束样本     = 768, 1024, 1280...
每帧样本时间顺序正确
```

### 21.3 任意块长一致性测试

同一段 4ch 数据分别使用：

```text
一个完整块
逐帧输入
不规则块：17、103、7、512、31……
```

输出分析帧、时间戳和最终 DOA 结果必须一致；浮点结果允许极小误差。

### 21.4 合成平面波方向测试

生成连续确定性多音信号，不使用外部音频文件：

```text
x_m[n] = Σ sin(2*pi*f*(n/fs) + 2*pi*f*dot(r_m,u(theta))/c + phase_f)
```

频率选择 FFT bin 上的多个音调，覆盖不同频段且低于 2500 Hz，例如：

```text
500.0 Hz
718.75 Hz
968.75 Hz
1468.75 Hz
1968.75 Hz
```

不同音调使用不同固定相位和幅度，避免退化。

测试方向：

```text
0°, 45°, 90°, 135°, 180°, 225°, 270°, 315°
```

每个方向使用全新的 `DoaProcessor`，输入至少 1 秒。预期：

```text
稳定后的原始角度循环误差 <= 2°
跟踪角度循环误差 <= 3°
状态最终为 Tracking
```

这个测试必须能发现：

- CPSD 共轭顺序反了；
- steering 符号反了；
- 通道顺序错误；
- 顺/逆时针角度错误。

### 21.5 0° 跨界 Kalman 测试

按高置信度输入：

```text
350°, 355°, 359°, 1°, 5°, 10°
```

验证：

- 内部展开角度连续；
- 不跳到 180°；
- 相邻输出的循环差合理；
- 最终角度接近 10°。

### 21.6 获取门控测试

验证：

```text
2 帧高置信度 → 不得获取
3 帧方向一致高置信度 → 获取
中间插入低置信度 → 计数重置
3 帧方向不一致 → 不获取
```

### 21.7 大角度跳变测试

先稳定跟踪 20°，随后输入高置信度 200°：

```text
第 1、2 个跳变观测不能立即重置
连续第 3 个一致跳变观测后重置到约 200°
```

### 21.8 Coasting 测试

获取目标后连续输入低置信度：

```text
<= 500 ms：status 为 Coasting，仍有 tracked_angle
> 500 ms：status 为 Searching，tracked_angle 为 None
```

### 21.9 无效输入测试

覆盖：

```text
全零输入
极低电平独立噪声
长度非 4 整数倍
非法 beta/tau/confidence
```

要求无 panic；全零和低电平独立噪声不能获取目标。

独立噪声可用测试内确定性 LCG 生成，不增加 `rand` 依赖。

### 21.10 现有拆分测试

更新 `split_6ch_works`，验证复用版函数仍输出：

```text
algo = [0, 6]
mic  = [1,2,3,4,7,8,9,10]
ref  = [5,11]
```

再增加非整 6 通道长度返回错误测试。

---

## 22. README 更新

README 增加“实时 DOA”章节，至少说明：

1. 运行命令：

```bash
# 只显示实时 DOA
respeaker_algo --doa

# 显示并保存逐帧 CSV
respeaker_algo --doa --doa-csv

# 角度方向标定示例
respeaker_algo \
  --doa \
  --doa-csv \
  --doa-beta 0.75 \
  --doa-cpsd-tau-ms 100 \
  --doa-offset-deg 90 \
  --doa-clockwise
```

2. 固定时频配置：

```text
16 kHz
512 点 Hann 窗（32 ms）
256 点 hop（16 ms）
360 个 1° 候选方向
```

3. 算法链：

```text
CPSD EMA → coherence-weighted PHAT-β SRP → confidence gate → circular Kalman
```

4. DOA 输入是拆分后的 `ch1..ch4`，不使用 `ch0` 固件 DOA；
5. DOA 不修改 WAV 内容；
6. CSV 字段说明；
7. 内部 0°/+X/逆时针定义，以及 `offset/clockwise` 的标定方式；
8. 当前限制：单声源、二维水平面、远场模型；
9. 仅支持 16kHz/6ch；Windows 需要 WASAPI 独占后端才能获得完整原始通道；
10. 32 ms 窗带来的首帧等待，以及 CPSD warmup/获取门控还会增加首次锁定时间；
11. 推荐现场先固定声源在已知方向，确定 `--doa-offset-deg` 和是否需要 `--doa-clockwise`。

同步更新顶部“当前阶段”描述和代码结构树，体现 DOA 已实现，而不是仍写“后续规划”。

---

## 23. 本地验证命令

Codex 完成实现后必须依次运行：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

如果 `cargo fmt --check` 失败，先执行：

```bash
cargo fmt --all
```

然后重新运行全部检查。

不得通过：

- 删除失败测试；
- 给大范围代码增加 `#[allow(...)]`；
- 降低断言到没有实际意义；
- 跳过 Windows 条件代码的编译问题；
- 把 clippy 错误留给用户。

若当前环境不是 Windows，至少保证：

- 非 Windows 构建和测试通过；
- Windows 专属文件未被无关修改；
- `#[cfg(windows)]` 引用和模块边界从代码上保持正确。

---

## 24. 设备验收步骤

代码测试通过后，用户可在 Windows ReSpeaker 设备上执行：

```powershell
cargo build --release

target\release\respeaker_algo.exe `
  --backend wasapi `
  --duration 30 `
  --doa `
  --doa-csv `
  --out-dir target/out `
  --prefix doa_test
```

预期文件：

```text
target/out/doa_test_respeaker_algo.wav
target/out/doa_test_respeaker_mic.wav
target/out/doa_test_respeaker_ref.wav
target/out/doa_test_respeaker_doa.csv
```

人工测试：

1. 阵列和声源保持同一水平面；
2. 距离建议 1～2 m；
3. 在 0°、45°、90°……315° 依次说话；
4. 记录 raw、tracked、confidence；
5. 确认旋转方向；
6. 通过 `--doa-offset-deg` 和 `--doa-clockwise` 完成设备坐标标定；
7. 检查静止说话时 tracked 抖动；
8. 检查停讲话后进入 Coasting，并在约 500 ms 后回到 Searching；
9. 检查移动说话人方向变化是否连续；
10. 检查三路 WAV 与未开启 DOA 时内容、长度和通道顺序一致。

---

## 25. 验收标准

实现完成必须同时满足：

### 构建与兼容性

- `cargo fmt --check` 通过；
- `cargo clippy ... -D warnings` 通过；
- `cargo test --all-targets` 通过；
- `cargo build --release` 通过；
- DOA 默认关闭；
- 原有 CLI、设备枚举和 WAV 输出不回归。

### 算法

- 使用 512/256，不是 256/128；
- 使用 4 个 PSD 和 6 个 CPSD EMA；
- EMA alpha 根据时间常数计算；
- 使用相干度归一的 PHAT-β；
- 相邻/相对 pair 使用不同上限频带；
- 只使用 6 个互谱 pair，不加入自谱；
- 360°、1° 网格；
- steering 共轭和符号通过合成方向测试；
- 置信度至少包含能量、MSC、主次峰间隔、robust prominence；
- Kalman 正确跨越 0/360°；
- 低置信度不更新；
- 3 帧获取、3 帧大跳变重置、500 ms coast 丢失均有测试。

### 实时工程

- 任意采集块长可正确分帧；
- 热路径复用 FFT 和大缓冲；
- 不在采集回调运行 DOA；
- 不增加第一版 DOA 工作线程；
- 终端最多 10 Hz；
- CSV 使用缓冲写入；
- 全零、非法长度、非法配置不 panic。

### 文档

- README 的阶段说明、代码树、命令、参数、角度定义、CSV 和限制均已更新。

---

## 26. 非目标与后续扩展

本次不要实现：

- 多声源峰值跟踪；
- 3D 方位角/俯仰角；
- MUSIC、NormMUSIC、MVDR/Capon；
- NN DOA；
- 波束形成；
- AEC 参考通道抑制；
- 实测 RTF 字典；
- WebSocket、GUI、LED 控制；
- DOA 独立线程或 SIMD 手工优化；
- 离线 WAV DOA 子命令。

代码结构需要允许以后增加，但不得为未来功能提前引入复杂框架。

后续可能的第二阶段：

1. 利用 `ch5` 回放参考做 MIC-REF 相干度抑制；
2. 利用 `ch0` 处理后语音做辅助活动检测；
3. 保存 SRP 空间谱用于调参；
4. 添加离线 WAV 回放和旧版 SRP-PHAT A/B；
5. 根据实测数据调整 confidence 阈值；
6. 固定设备后加入实测 RTF 字典。

---

## 27. Codex 最终回复格式

完成后，Codex 的最终回复应包含：

1. 实际修改的文件；
2. 算法和录音链路的集成位置；
3. CLI 示例；
4. 测试与构建命令的真实结果；
5. 是否存在未能在当前环境执行的 Windows 设备实测；
6. 需要用户现场确定的仅限角度 offset/clockwise 标定项；
7. 不要声称未实际运行的测试已通过。
