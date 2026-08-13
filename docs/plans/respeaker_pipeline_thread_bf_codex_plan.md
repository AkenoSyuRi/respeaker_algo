# ReSpeaker 算法线程解耦与 Beamforming 集成实施方案

> 执行对象：Codex / coding agent
> 仓库：`AkenoSyuRi/respeaker_algo`
> 代码基线：`390b171afc588ad06d116c90c7828ccab2199ba7`
> 目标平台：Windows，Rust 2024，ReSpeaker Mic Array v2.0，16 kHz / PCM16 / 6ch
> 本文不是概念讨论。请按本文直接修改代码、补测试、更新配置与文档，并执行验证命令。
> 仓库内唯一真源：`docs/plans/respeaker_pipeline_thread_bf_codex_plan.md`。不得复制为第二个同内容方案文件。
> 运行时过载策略：Pipeline 失败只停用算法分支，原始录音继续；capture/raw WAV 失败才终止录制。

---

## 0. 执行要求

开始修改前：

1. 执行 `git rev-parse HEAD`，确认当前基线。
2. 阅读仓库根目录当前的 `AGENTS.md`、`README.md`。
3. 检查工作区状态，不得覆盖、重置或丢弃用户已有修改。
4. 以当前源码为事实来源；若工作区已包含比本文更新的实现，应在不回退现有功能的前提下适配。
5. 不要只生成方案或伪代码，必须完成实际实现、自动测试、配置示例和文档同步。
6. 不要引入 async DSP、复杂 trait 框架、动态插件系统或不必要的泛型抽象；继续使用显式结构体、`std::thread` 和有界通道。
7. 不在 WASAPI 采集线程中执行 FFT、DOA、BF、文件 I/O、Web、CSV 或阻塞式日志。
8. 不改变现有通道契约：

```text
ch0 = ReSpeaker 固件算法输出
ch1..ch4 = 4 路原始 MIC
ch5 = playback / AEC reference
```

9. 默认无 `--pipeline-config` 时仍然只录音；不得默认启用 DOA 或 BF。
10. Viewer 的显示旋转不得进入 BF 的物理 steering 坐标。

---

## 1. 当前实现事实与问题

当前基线的主要数据流为：

```text
wasapi-capture thread
    ↓ sync_channel<Vec<i16>>
recorder main loop
    ↓ 6ch split
    ├─ 同步调用 PipelineRuntime::push_block()
    └─ 同步写入 algo / mic / ref WAV
```

当前主要问题：

1. `wasapi-capture` 使用有界 `sync_channel` 的阻塞 `send()`；下游长期变慢时，采集线程会等待。
2. `recorder.rs` 在同一消费循环内同步执行 DOA、CSV、Viewer publish 和 WAV 写入；算法耗时会增加采集队列回压。
3. `PipelineInputBlock<'a>` 是借用视图，不能直接跨线程传递。
4. 当前 Pipeline 只维护 `latest_doa`，没有 BF 模块。
5. 当前 `DoaResult` 对外角度已应用 `angle_offset_deg` / `clockwise`；BF 应使用转换前的内部物理角，而不是 Viewer 或对外角。
6. 当前 DOA 使用 512 点窗、256 点 hop。若一次 `push_block()` 包含多个 hop，先运行完整 DOA 再运行 BF，会把最后一个 DOA 结果错误地用于整块 BF；算法 worker 必须把输入重新组织为固定 256 帧的 hop。

---

## 2. 本次目标

### 2.1 必须完成

1. 将整个算法 Pipeline 搬到独立 worker 线程。
2. WASAPI 采集线程不得执行阻塞发送；recorder 向算法 worker 只允许 `try_send`。DOA、BF、CSV 和 Viewer 不得再向采集路径传递等待。
3. Pipeline 运行时首次发生队列 `Full`、worker `Disconnected` 或 DSP/CSV/Viewer/BF 错误后：
   - 立即关闭该 worker 的输入并永久停用本次录制的算法投递；
   - 不停止 WASAPI，不截断原始录音；
   - recorder 继续 drain capture queue，并继续写 `algo/mic/ref` 三路 raw WAV；
   - 录制最终结束后返回首个 Pipeline 错误，使进程结果可见失败。
4. Pipeline 正常活动期间，输入必须连续、有序，禁止静默丢块；首次 Pipeline 错误后不得跳过若干块再恢复，也不得继续反复 `try_push`。
5. 为 Pipeline 增加固定 256-frame hop 的重分块，使 DOA 与 BF 在同一时间边界运行。
6. 增加频域 Delay-and-Sum Beamformer，作为正确性基线与 fallback。
7. 增加适配当前 4-Mic 正方形阵列的鲁棒超指向 MVDR：
   - 3D diffuse coherence model；
   - diagonal loading；
   - WNG 下限约束；
   - 低频和高频与 Delay-and-Sum 平滑混合。
8. BF 支持：
   - 消费 DOA 的内部 tracked angle；
   - 使用固定内部角；
   - DOA 丢失时使用固定 fallback 角。
9. BF 输出单通道 PCM16 WAV：`{prefix}_respeaker_bf.wav`。
10. 更新 TOML 配置、README、测试和本方案文档末尾的实现结果。
11. README 和验收必须明确：本轮解除的是 **DSP/CSV/Viewer/BF** 对采集的回压；raw WAV 仍由 recorder 同步写入，磁盘卡顿仍可能填满 capture queue 并触发 fail-fast。

### 2.2 本次不做

1. 不实现在线样本协方差 MVDR、GEV、MWF、GSC、LCMV。
2. 不实现神经网络掩码、VAD 或目标/噪声 PSD 估计。
3. 不实现音频播放输出或虚拟声卡，仅生成 BF WAV。
4. 不修改 Web Viewer 的坐标定义和 UI。
5. 不修改 DOA 的 SRP、置信度和 Kalman 数学逻辑。
6. 不改变采样率、位深、通道数和录音文件命名契约。
7. 不在本轮把 raw WAV 写入再拆到独立 I/O worker；同步 raw I/O 回压是已知限制，必须如实记录。
8. 不增加“Pipeline 过载即停止整次录音”的 strict 模式；本轮唯一运行时策略是算法降级、raw 继续。

## 3. 最终线程与数据流

实现以下结构：

```text
                       main / recorder
                             │
                 startup / shutdown / errors
                             │
        ┌────────────────────┴────────────────────┐
        │                                         │
        ▼                                         ▼
wasapi-capture thread                       algorithm-worker thread
        │                                         │
        │ try_send CaptureBlock                   │ PipelineRuntime owner
        ▼                                         │
bounded capture queue                             │ 6ch → 256-frame hops
        │                                         │ DOA → BF
        ▼                                         │ CSV / Viewer / BF WAV
recorder loop                                     │
        │                                         │
        ├─ split + synchronous raw WAV             │
        └─ if PipelineActive:                      │
             try_send CaptureBlock.clone() ───────┘
```

约束：

- WASAPI 线程只负责读取设备、转换 PCM、构造 `CaptureBlock` 和 `try_send`。
- recorder loop 负责原始录音、生命周期和向算法线程投递；raw WAV I/O 本轮仍在该线程同步执行。
- algorithm worker 独占 `PipelineRuntime`，不需要锁住 DSP 状态。
- **capture queue `Full` 是致命采集 overrun**：设置 stop、结束本次录制并返回错误，因为此时无法保证 raw 连续性。
- **algorithm queue `Full`、worker 断开或 Pipeline 运行时错误不是 raw 录音的停止条件**：
  1. 记录首个 Pipeline 错误；
  2. 立即 drop/关闭 worker 输入端；
  3. 将状态永久切换为 `PipelineFailed`；
  4. 后续 capture block 只写 raw，不再调用 `try_push`；
  5. worker 自行 drain 已成功入队的数据并 finalize；recorder 在最终阶段 join；
  6. 录制结束并完成 raw WAV finalize 后，最终返回该 Pipeline 错误。
- Pipeline 初始化握手失败仍属于启动错误：WASAPI 尚未启动，直接返回，不生成一段“无算法”的意外录音。
- raw WAV 写入失败属于录音失败，应设置 stop 并进入 capture drain/finalize；磁盘过慢也可能使 capture queue 填满。本轮不承诺消除这种 I/O 回压。
- 正常停止时先请求停止采集并 drain capture queue；raw WAV 尽早 finalize；再关闭仍活动的算法发送端并等待 worker drain/finalize/join。
- Pipeline 已失败时，其发送端已经关闭；drain capture queue 期间不得重复报 `Full`/`Disconnected`。

## 4. 新增统一采集块类型

新增 `src/audio.rs`，放置跨 `wasapi`、`recorder` 和 algorithm worker 共用的类型及固定设备常量。

建议结构：

```rust
use std::sync::Arc;

pub const RESPEAKER_SAMPLE_RATE: u32 = 16_000;
pub const RESPEAKER_CHANNELS: usize = 6;
pub const RESPEAKER_MIC_CHANNELS: usize = 4;

#[derive(Clone, Debug)]
pub struct CaptureBlock {
    pub sequence: u64,
    pub start_frame: u64,
    pub frames: usize,
    pub samples: Arc<[i16]>,
}

impl CaptureBlock {
    pub fn validate(&self) -> Result<(), String>;
}
```

要求：

- `samples.len() == frames * 6`。
- `start_frame` 表示本块第一个 6ch frame 在整次录制中的绝对位置。
- `sequence` 单调递增，从 0 开始。
- `Vec<i16>` 读取完成后通过 `Arc<[i16]>` 接管内存，不额外复制整块 6ch PCM。
- recorder 与 algorithm worker 只 clone `CaptureBlock` / `Arc`，不复制原始采集块。
- 将 `recorder.rs` 和 `wasapi.rs` 中重复的采样率、通道数常量改为引用 `audio.rs`。

在 `src/main.rs` 或 crate 根模块注册 `mod audio;`。

---

## 5. WASAPI 采集线程改造

修改 `src/wasapi.rs` 和创建 capture channel 的 recorder 代码。

### 5.1 Capture queue 容量与发送类型

保留当前基线的有界容量，明确写成共享常量：

```rust
pub const CAPTURE_QUEUE_CAPACITY: usize = 128;
```

创建通道时必须使用：

```rust
let (tx, rx) = sync_channel::<CaptureBlock>(CAPTURE_QUEUE_CAPACITY);
```

要求：

- `128` 表示 WASAPI packet 数，不是固定毫秒数；实际缓冲时长依 packet 大小而变。
- 本轮不得随手缩小该容量。后续只能依据 packet 分布、磁盘抖动和 overrun 观测单独调优。
- capture queue 仍服务于 recorder/raw WAV；由于 raw I/O 同步，磁盘卡顿仍可能导致该队列满。

将：

```rust
SyncSender<Vec<i16>>
```

改为：

```rust
SyncSender<CaptureBlock>
```

采集线程维护：

```rust
let mut sequence = 0u64;
let mut total_frames = 0u64;
```

每次读到 packet 后构造：

```rust
CaptureBlock {
    sequence,
    start_frame: total_frames,
    frames,
    samples: samples.into(),
}
```

然后更新 `sequence` 和 `total_frames`。

### 5.2 禁止阻塞发送

必须使用：

```rust
match tx.try_send(block) {
    Ok(()) => {}
    Err(TrySendError::Full(_)) => {
        stop.store(true, Ordering::SeqCst);
        return Err("WASAPI capture queue overrun ...".into());
    }
    Err(TrySendError::Disconnected(_)) => {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        return Err("capture consumer disconnected unexpectedly".into());
    }
}
```

禁止改回 `send()`、`send_timeout()` 或无限重试。capture queue `Full` 必须是明确的致命错误，不能丢 packet 后继续伪装为连续录音。

### 5.3 采集线程错误返回

当前线程内部只 `eprintln!` 错误不够。将 join handle 改为：

```rust
JoinHandle<Result<(), String>>
```

为 `WasapiSession` 增加显式方法：

```rust
pub fn stop_and_join(&mut self) -> Result<(), String>;
```

要求：

- 正常结束时设置 stop、join，返回采集线程结果。
- panic 转换成清晰错误。
- `Drop` 仍做 best-effort stop/join，但正常路径必须显式调用并检查结果。
- queue full、设备读取失败、设备断开均要传回 `run_record()`。

### 5.4 测试

将“发送策略”提取成不依赖声卡的小函数，至少测试：

- 空队列发送成功；
- 满队列立即返回 overrun，不等待；
- 接收端断开返回错误；
- `CaptureBlock` sequence / start_frame 累加正确；
- capture channel 的生产代码使用 `CAPTURE_QUEUE_CAPACITY == 128`。

不要用依赖墙钟的脆弱 sleep 测试来证明非阻塞；使用容量为 1 的 channel 构造确定性满队列。

## 6. 新增 Algorithm Worker

新增 `src/pipeline_worker.rs`。

### 6.1 公共接口

建议接口：

```rust
pub struct PipelineWorkerHandle {
    tx: Option<SyncSender<CaptureBlock>>,
    join: Option<JoinHandle<Result<PipelineWorkerStats, String>>>,
    queue_depth: Arc<AtomicUsize>,
    max_queue_depth: Arc<AtomicUsize>,
}

#[derive(Default, Debug)]
pub struct PipelineWorkerStats {
    pub capture_blocks: u64,
    pub input_frames: u64,
    pub processed_hops: u64,
    pub max_queue_depth: usize,
}

impl PipelineWorkerHandle {
    pub fn spawn(
        config: PipelineConfig,
        out_dir: String,
        prefix: String,
    ) -> Result<Self, String>;

    pub fn try_push(&self, block: CaptureBlock) -> Result<(), String>;

    /// 只关闭输入，不等待 worker；可在 recorder 继续写 raw 时调用。
    pub fn close_input(&mut self);

    /// 若输入尚未关闭则先关闭，然后 join worker 并返回 finalize 结果。
    pub fn finish(&mut self) -> Result<PipelineWorkerStats, String>;
}
```

`try_push()` 只能调用 `try_send()`：

- Full：返回 `Pipeline queue overrun`；
- Disconnected：返回 worker 已失败/退出；
- 不得等待队列腾空。

初始队列容量使用常量：

```rust
const PIPELINE_QUEUE_CAPACITY: usize = 32;
```

Pipeline queue 错误如何影响 raw 录音由 §8.2 统一处理：`try_push()` 只返回错误，不得直接设置全局 capture stop。

`std::sync::mpsc` 不提供可靠队列长度。若保留 `max_queue_depth`，必须按以下顺序维护共享 `Arc<AtomicUsize>`，禁止“发送成功后才递增”：

```text
sender:
    reserved = queue_depth.fetch_add(1) + 1
    try_send(block)
        Ok:
            用 reserved 更新 max_queue_depth
        Err:
            queue_depth.fetch_sub(1)  # 回滚预增
            返回错误

worker:
    recv 成功后：queue_depth.fetch_sub(1)
    然后处理 block
```

原因：若先 `try_send` 成功、worker 立即 `recv/fetch_sub`、sender 随后才 `fetch_add`，`AtomicUsize` 会 underflow 并污染 high-watermark。

补充约束：

- 发送失败必须回滚预增，无论是 `Full` 还是 `Disconnected`。
- worker 每次成功 `recv` 只减一次；建议 `debug_assert!(previous > 0)`。
- high-watermark 仅用于观测，不参与正确性、过载判定或生命周期决策；并发下允许是保守上界。
- 使用 `Ordering::Relaxed` 即可，因为这些原子量只做统计；channel 本身提供数据同步。
- 不要为了精确统计引入锁或无界队列。

### 6.2 同步启动握手

`PipelineRuntime::new()` 可能因 TOML、Web 端口、CSV/WAV 文件创建、BF 权重生成失败。

`spawn()` 必须等待 worker 初始化握手成功后才返回，确保所有初始化错误发生在 WASAPI 启动之前：

```text
spawn thread
    ↓
thread creates PipelineRuntime
    ↓
ready channel returns Ok / Err
    ↓
main starts WASAPI only after Ok
```

不能先开始录音，再异步发现 Viewer 端口或 BF 初始化失败。

### 6.3 顺序和连续性检查

worker 记录预期：

```rust
expected_sequence
expected_start_frame
```

每个 `CaptureBlock` 必须满足：

```text
sequence == expected_sequence
start_frame == expected_start_frame
```

否则 worker 返回错误并退出，禁止掩盖丢块或乱序。该检查是算法路径的防御性校验：触发后按 §8.2 走 Pipeline 降级（关闭输入、raw 继续），**不**因此单独设置 capture stop。

正常路径下 WASAPI 连续编号且 Active 期间 recorder 逐块 `try_push`，worker 不应见到 gap。raw 路径连续性由 capture queue 的致命 `Full` 语义保证，不依赖 worker 的 sequence 检查来停采。

---

## 7. Algorithm Worker 内固定 256-frame 重分块

新增 worker 内部私有结构，例如：

```rust
struct PipelineHopAssembler {
    start_frame: u64,
    filled: usize,
    algo: [i16; HOP_SIZE],
    mic: [i16; HOP_SIZE * MIC_COUNT],
    reference: [i16; HOP_SIZE],
}
```

其中：

```text
HOP_SIZE = 256
MIC_COUNT = 4
```

`CaptureBlock` 中每个 6ch frame 拆为：

```text
algo       <- ch0
mic        <- ch1..ch4，4ch 交织
reference  <- ch5
```

每累计 256 frame 调用一次：

```rust
runtime.push_block(PipelineInputBlock {
    start_frame,
    frames: 256,
    algo: &algo,
    mic: &mic,
    reference: &reference,
})?;
```

### 7.1 双重分帧硬约束

必须保留现有 `src/doa/framer.rs` 的 `FrameAssembler`。两层职责不同：

```text
PipelineHopAssembler
    只把不定长 WASAPI packet 统一成每次 256-frame 的 Pipeline 调用边界
    解决“一块包含多个 hop 时 BF 误用最后一个 DOA”的时序问题

DOA FrameAssembler
    继续维护 512-point analysis window / 256-point hop
    负责 DOA 的重叠分析帧和跨调用历史
```

**禁止因为 worker 已按 256 frame 重分块而删除、旁路或重写 DOA `FrameAssembler`。**

当前 DOA 时间线必须写入测试：

```text
累计 512 samples：第 1 个 DOA analysis frame，只更新 PSD/CPSD EMA
累计 768 samples：第 2 个 DOA analysis frame，只更新 PSD/CPSD EMA
累计 1024 samples：第 3 个 analysis frame，产生第 1 个 DoaResult
```

因此约到 `1024 / 16000 = 64 ms` 才有第一条 DOA 结果。在此之前，`direction_source = "doa"` 的 BF 必须使用 `fallback_internal_angle_deg`。

### 7.2 其它要求

- 常规运行中每次 Pipeline 调用恰好 256 frame。
- DOA 每次调用最多产生一个新结果。
- BF 使用与当前 hop 结束时间对应的最新 DOA 结果。
- worker 停止时，若剩余 `1..255` frame，最后调用一次 partial block；不要在 assembler 层丢弃。
- BF 自己负责 STFT 尾部 zero-pad 和输出长度裁剪。
- assembler 初始化后不得按 hop 反复分配大 Vec。

修改 `PipelineInputBlock`：

```rust
pub struct PipelineInputBlock<'a> {
    pub start_frame: u64,
    pub frames: usize,
    pub algo: &'a [i16],
    pub mic: &'a [i16],
    pub reference: &'a [i16],
}
```

增加长度校验：

```text
algo.len() == frames
mic.len() == frames * 4
reference.len() == frames
```

## 8. recorder.rs 调度改造

修改 `src/recorder.rs`。

### 8.1 初始化顺序

在启动 WASAPI 前完成所有可能失败的文件和算法初始化：

```text
load and validate PipelineConfig
create output directory / paths
create raw WAV sinks
spawn PipelineWorker and wait ready
start WASAPI capture
```

raw WAV 与 worker 的先后可按所有权实现微调，但必须保证 WASAPI 最后启动；若后一步初始化失败，要显式 finalize/关闭已创建的前一步资源。若无 `--pipeline-config`，不创建 worker。

Pipeline 初始化握手失败是启动失败，不采用运行时“raw 继续”降级策略，因为此时还没有开始采集。

### 8.2 recorder loop 与 Pipeline 降级状态

recorder 继续负责原始 3 路 WAV，保持现有输出：

```text
*_respeaker_algo.wav
*_respeaker_mic.wav
*_respeaker_ref.wav
```

维护正交于录制状态的 Pipeline 状态，例如：

```rust
enum PipelineDispatchState {
    Disabled,
    Active(PipelineWorkerHandle),
    Failed {
        worker: PipelineWorkerHandle,
        first_error: String,
    },
}
```

收到 `CaptureBlock` 后：

1. `validate()`；
2. 拆分为现有三个可复用 buffer；
3. 写入 raw WAV；
4. 仅当状态为 `Active` 时调用 `worker.try_push(block.clone())`；
5. `try_push` 首次返回 `Full`/`Disconnected` 时：
   - 保存首个 Pipeline 错误；
   - 只打印一次“Pipeline 已停用，raw 录音继续”的错误；
   - 调用 `worker.close_input()`，让 worker drain 已入队数据并 finalize；
   - 切换为 `Failed`；
   - **不得设置 capture stop**；
   - **不得立即阻塞 join**，否则 recorder 会停止消费 capture queue；
6. 后续所有 block 只写 raw，绝不再次 `try_push`，也不尝试重启 Pipeline。

算法投递禁止调用阻塞 `send()`。Pipeline worker 内部运行时错误若导致接收端退出，下一次 `try_push` 的 `Disconnected` 或最终 `finish()` 会暴露错误，并执行相同降级语义。worker 侧 sequence/start_frame gap 也属于这类 Pipeline 错误，不是 raw 停采条件。

raw WAV 写入错误或 capture 线程错误仍是录音错误：记录错误、设置 stop，进入 capture drain/finalize。recorder **不必**再维护一套 `expected_sequence` 来把 gap 升级为停采；raw 连续性依赖 capture `try_send` 的致命 overrun，而非算法 worker 的防御性检查。

### 8.3 停止、drain、finalize 和错误优先级

录制生命周期建议：

```text
Running
    Ctrl+C / deadline / capture error / raw I/O error
StoppingCapture
    set stop = true
DrainingCaptureQueue
    continue recv until capture sender disconnected
FinalizingRaw
    finalize algo / mic / ref WAV
FinalizingPipeline
    close input if still Active
    finish/join worker if present
JoiningCapture
    stop_and_join capture session if not already joined
Done
```

实现可调整 `JoiningCapture` 与 `FinalizingRaw` 的具体顺序，但必须满足以下语义：

- Ctrl+C 或 deadline 到达时设置 stop，但继续消费 capture queue，直到 WASAPI sender 断开。
- 所有已成功进入 capture queue 的数据都要尝试写入 raw WAV。
- Pipeline 首次失败后，其输入端立即关闭；drain capture queue 期间只写 raw。
- 不要在 Pipeline 失败发生时同步 `finish()`/join worker；应让 recorder 保持消费 capture queue。
- capture queue drain 完成后，优先 finalize 三个 raw WAV，使其文件头不受后续 worker join 延迟影响。
- 若 Pipeline 仍为 `Active`，此时关闭输入；随后对 `Active` 或 `Failed` worker 调用一次 `finish()`。
- `finish()` 必须 drain 已成功进入 algorithm queue 的块、finalize DOA CSV/BF WAV/Web，并返回 worker 内部错误。
- 无论发生何种错误，都尽力 finalize 已创建的 WAV/CSV/Web。
- 错误优先级必须固定为：
  1. 已保存的 `Failed.first_error`（含 algorithm queue `Full`、降级时看到的 `Disconnected`、或等价首个 Pipeline 错误）；
  2. 其后才考虑 `finish()` 返回的 worker 错误、capture join 错误、WAV finalize 错误。
- **`Failed.first_error` 优先于随后的 `finish() == Ok`**：`close_input()` 后 worker 常会干净退出并返回 `Ok(stats)`，不得因此把本次录制写成成功。
- 后续次级错误只附加到错误信息，不覆盖首个实质错误。
- 若唯一错误是 Pipeline overrun/runtime failure，raw WAV 应完整结束，但 `run_record()` 仍返回 `Err`，让调用者知道算法输出不完整。

### 8.4 已知边界

本轮只解除算法分支对 recorder/capture 的回压。recorder 仍同步写三个 raw WAV，因此：

```text
slow disk / antivirus / filesystem stall
    ↓
recorder 消费 capture queue 变慢
    ↓
capture queue 可能 Full
    ↓
致命 capture overrun，录制停止
```

README、验收和最终报告必须明确这一限制，不得表述为“采集线程已不会被任何下游工作拖慢”。

## 9. 保留 DOA 内部角度

修改 `src/doa/mod.rs` 中的 `DoaResult`。

增加：

```rust
pub raw_internal_deg: Option<f32>,
pub tracked_internal_deg: Option<f32>,
```

保留现有：

```rust
pub raw_angle_deg: Option<f32>,
pub tracked_angle_deg: Option<f32>,
```

含义：

```text
*_internal_deg
    0° = +X
    90° = +Y
    counter-clockwise
    未应用 angle_offset_deg / clockwise
    供 BF 使用

raw_angle_deg / tracked_angle_deg
    已应用 output_deg()
    继续供终端、CSV、Web Viewer 使用
```

在 `DoaProcessor::process_frame()` 中直接保存 tracker 的内部角，再生成对外角。

要求：

- 不修改现有 CSV 列，不破坏现有 Viewer JSON/页面契约。
- 增加测试：配置非零 `angle_offset_deg` 且 `clockwise = true` 时，internal angle 保持不变，external angle 正确转换。
- BF 只能读取 `tracked_internal_deg` / `raw_internal_deg`，不得把 external 或 Viewer 角度再反算。

---

## 10. 新增 Beamformer 模块

新增目录：

```text
src/beamformer/
    mod.rs
    stft.rs
    weights.rs
    matrix.rs
```

建议职责：

```text
mod.rs
    BeamformerConfig
    enums
    BeamformerRuntime
    direction selection
    WAV output
    clipping / stats

stft.rs
    4ch arbitrary-block input
    512/256 streaming STFT
    4 forward RFFT
    weighted sum
    1 inverse RFFT
    sqrt-Hann WOLA
    tail flush / exact length

weights.rs
    steering vectors
    Delay-and-Sum LUT
    diffuse covariance
    robust superdirective MVDR LUT
    WNG calculation and loading search
    frequency-band blending

matrix.rs
    fixed 4x4 real symmetric Cholesky
    solve real and imaginary RHS
```

不引入通用线性代数依赖。当前 `R = Γ + λI` 是实对称正定矩阵，可用固定 4x4 Cholesky 分别求解 steering 的实部与虚部。

---

## 11. BF 配置与 Pipeline TOML

扩展 `src/pipeline.rs` 的 `ModuleConfig`：

```rust
Beamformer {
    enabled: bool,
    algorithm: BeamformerAlgorithm,
    direction_source: BeamformerDirectionSource,
    fixed_internal_angle_deg: f32,
    fallback_internal_angle_deg: f32,
    direction_smoothing_ms: f32,
    min_wng_db: f32,
    sd_low_start_hz: f32,
    sd_low_full_hz: f32,
    sd_high_full_hz: f32,
    sd_high_end_hz: f32,
    output_gain_db: f32,
    wav: bool,
    compare_wav: bool, // bf.wav 写为双声道对比：左=mic1×gain、右=BF输出×gain
}
```

枚举：

```rust
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeamformerAlgorithm {
    DelaySum,
    RobustSuperdirective,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeamformerDirectionSource {
    Doa,
    Fixed,
}
```

默认值：

```text
enabled = true
algorithm = robust_superdirective
direction_source = doa
fixed_internal_angle_deg = 0.0
fallback_internal_angle_deg = 0.0
direction_smoothing_ms = 64.0
min_wng_db = 3.0
sd_low_start_hz = 350.0
sd_low_full_hz = 500.0
sd_high_full_hz = 2500.0
sd_high_end_hz = 3500.0
output_gain_db = -3.0
wav = true
```

### 11.1 Serde 默认值是硬要求

上表中每一个可省略字段都必须有与之对应的 `default_*` 函数和字段属性。不得只在文档中声明默认值，也不得依赖字段类型“看起来有 Default”。结构应类似：

```rust
Beamformer {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_beamformer_algorithm")]
    algorithm: BeamformerAlgorithm,
    #[serde(default = "default_beamformer_direction_source")]
    direction_source: BeamformerDirectionSource,
    #[serde(default = "default_fixed_internal_angle_deg")]
    fixed_internal_angle_deg: f32,
    #[serde(default = "default_fallback_internal_angle_deg")]
    fallback_internal_angle_deg: f32,
    #[serde(default = "default_direction_smoothing_ms")]
    direction_smoothing_ms: f32,
    #[serde(default = "default_min_wng_db")]
    min_wng_db: f32,
    #[serde(default = "default_sd_low_start_hz")]
    sd_low_start_hz: f32,
    #[serde(default = "default_sd_low_full_hz")]
    sd_low_full_hz: f32,
    #[serde(default = "default_sd_high_full_hz")]
    sd_high_full_hz: f32,
    #[serde(default = "default_sd_high_end_hz")]
    sd_high_end_hz: f32,
    #[serde(default = "default_output_gain_db")]
    output_gain_db: f32,
    #[serde(default = "default_beamformer_wav")]
    wav: bool,
}
```

函数名可以按现有风格调整，但必须满足：

- 字段属性与默认值表一一对应；
- `configs/doa_bf.toml` 和 `configs/bf_fixed.toml` 省略的字段可以正常解析；
- 单元测试读取解析后的每个字段并与默认值表逐项比较；
- `#[serde(deny_unknown_fields)]` 行为继续保留。

### 11.2 校验

1. 最多一个 enabled DOA，最多一个 enabled Beamformer。
2. `direction_source = "doa"` 时，配置顺序中前面必须已有 enabled DOA。
3. `direction_source = "fixed"` 时允许没有 DOA。
4. 所有角度、频率、dB 和 smoothing 参数必须 finite。
5. 频率满足：

```text
0 <= low_start <= low_full <= high_full <= high_end <= 8000
```

6. `0 <= min_wng_db <= 6.0`；4 路 DAS 的理论上限约为 6.02 dB，留出数值余量。
7. `direction_smoothing_ms >= 0`。
8. `output_gain_db` 建议限制在 `[-24, 24]`（高增益配置用于补偿低电平输入；超出范围拒绝）。
9. `wav = false` 时仍运行 BF 可用于后续模块的扩展接口；本次若未实现下游音频传递，可允许但不创建输出文件。
10. 保持 `version = 1`；这是新增可选模块，不破坏旧 `configs/doa.toml`。

新增配置：

### `configs/doa_bf.toml`

```toml
version = 1

[[modules]]
type = "doa"
enabled = true
enable_viewer = true
csv = true
beta = 0.75
cpsd_tau_ms = 100.0
angle_offset_deg = 0.0
clockwise = false
acquire_confidence = 0.65
update_confidence = 0.40
max_coast_ms = 500

[[modules]]
type = "beamformer"
enabled = true
algorithm = "robust_superdirective"
direction_source = "doa"
fallback_internal_angle_deg = 0.0
direction_smoothing_ms = 64.0
min_wng_db = 3.0
sd_low_start_hz = 350.0
sd_low_full_hz = 500.0
sd_high_full_hz = 2500.0
sd_high_end_hz = 3500.0
output_gain_db = -3.0
wav = true
```

### `configs/bf_fixed.toml`

```toml
version = 1

[[modules]]
type = "beamformer"
enabled = true
algorithm = "delay_sum"
direction_source = "fixed"
fixed_internal_angle_deg = 0.0
fallback_internal_angle_deg = 0.0
direction_smoothing_ms = 0.0
output_gain_db = -3.0
wav = true
```

这两个示例故意省略部分可选字段，用于验证 Serde 默认值确实落到代码，而不是要求配置文件重复全部默认参数。

## 12. Steering 数学定义

统一采用当前阵列内部坐标：

```text
+X = 0°
+Y = 90°
角度逆时针增加
```

麦克风坐标继续复用：

```rust
crate::doa::geometry::RESPEAKER_V2_MICS_M
```

不要复制一份可能漂移的阵列坐标。

对方向 `θ`：

```text
u(θ) = [cos θ, sin θ]
τ_m(θ) = dot(r_m, u) / c
```

定义当前代码 FFT 约定下的目标 steering：

```text
d_m(f, θ) = exp(j 2π f τ_m(θ))
```

BF 输出：

```text
y(f) = w(f, θ)^H x(f)
```

所有权重必须满足：

```text
w^H d ≈ 1
```

不要通过肉眼猜测相位正负。使用与现有 DOA 合成平面波一致的确定性生成器，验证目标方向增益和相位。若 realfft 约定导致符号相反，应修正统一 steering helper，并以测试为准。

---

## 13. Delay-and-Sum 基线

权重：

```text
w_DAS = d / M
M = 4
```

因为输出为 `w^H x`，目标方向满足：

```text
w_DAS^H d = 1
```

要求：

- 为 360 个整数角、257 个 FFT bin、4 路 MIC 预计算 LUT。
- DC bin 使用实数平均权重 `1/M`。
- Nyquist bin 直接置零，并在输出 spectrum 中强制为实数零；该单个 bin 不参与 distortionless/WNG 验收，避免 real IFFT 的 Nyquist 复数歧义。
- 这是独立可选算法，也是 robust superdirective 的 fallback。
- 任何矩阵失败、非 finite 权重、WNG 搜索失败都必须逐 bin 回退到 DAS，不能 panic。

---

## 14. 鲁棒超指向 MVDR

### 14.1 Diffuse covariance

使用 3D diffuse field coherence：

```text
Γ_mn(f) = sinc(2π f ||r_m-r_n|| / c)

sinc(x) = sin(x) / x
sinc(0) = 1
```

构造：

```text
R(f, λ) = Γ(f) + λI
```

`R` 为实对称矩阵。

### 14.2 MVDR 权重

```text
z = R^-1 d
w_SD = z / (d^H z)
```

实现时：

1. 用 f64 构造 `R` 和 `d`；
2. 对 `R` 做 4x4 Cholesky；
3. 使用同一分解分别求解 `Re(d)` 和 `Im(d)`；
4. 组合成复数 `z`；
5. 计算 denominator；
6. 归一化；
7. 校验 finite 与 distortionless constraint；
8. 转成 `Complex32` 存 LUT。

禁止直接求逆矩阵。

### 14.3 WNG 约束

定义：

```text
WNG = 1 / (w^H w)
WNG_dB = 10 log10(WNG)
```

对每个 angle/bin：

1. 尝试很小 loading；
2. 若 WNG 已达到 `min_wng_db`，接受；
3. 否则指数扩大 `λ_high`，直到满足或达到上限；
4. 在 `[λ_low, λ_high]` 做固定 32 次二分；
5. 选择满足 WNG 的最小 loading；
6. 若无法 bracket、Cholesky 失败或结果非 finite，回退 DAS。

建议数值：

```text
initial loading = 1e-10
maximum loading = 1e4
binary iterations = 32
```

将 target WNG 转为线性：

```text
target_wng = 10^(min_wng_db / 10)
```

### 14.4 频带混合

定义 superdirective mix：

```text
f <= low_start:             mix = 0
low_start..low_full:        smoothstep 0→1
low_full..high_full:        mix = 1
high_full..high_end:        smoothstep 1→0
f >= high_end:              mix = 0
```

权重：

```text
v = (1-mix) * w_DAS + mix * w_SD
```

然后重新归一化，使：

```text
w^H d = 1
```

若混合后 WNG 低于阈值：

- 逐步增加 DAS 比例直到满足；或
- 直接回退该 bin 的 DAS。

必须通过测试保证最终 LUT 的所有有效权重满足 WNG 下限和 distortionless constraint。

### 14.5 LUT 内存

预计算：

```text
360 angles × 257 bins × 4 Complex32
```

约 3 MiB，可接受。

运行时不得逐 hop 求矩阵、做 loading 搜索或大量 trig。

---

## 15. Streaming STFT / WOLA

BF 使用与 DOA 一致的：

```text
sample rate = 16 kHz
FFT size = 512
hop = 256
```

但 BF 不能直接复用 DOA 的分析 Hann，因为 BF 需要可重构输出。

使用 periodic sqrt-Hann：

```text
hann_periodic[n] = 0.5 - 0.5*cos(2πn/N)
window[n] = sqrt(hann_periodic[n])
```

analysis 和 synthesis 使用同一 sqrt-Hann，使 50% overlap 下平方和为 1。

实现要求：

1. 任意块长 4ch i16 输入；正常 Pipeline 每次 256 frame，但 BF 类本身仍要正确处理 partial block。
2. 首部内部预填 256 frame 零，使文件 sample 0 能获得完整 overlap contribution。
3. 每 256 个新输入 frame：
   - 取 512 点 4ch frame；
   - 去 DC 可选，但若做必须对所有通道一致且有测试；首版建议不额外做 DC removal；
   - analysis window；
   - 4 路 RFFT；
   - 按当前方向 LUT 做 `w^H x`；
   - DC imag 强制为 0，Nyquist bin 强制为复数零；
   - inverse RFFT；
   - 除以 FFT size；
   - synthesis window；
   - overlap-add；
   - 释放一个 256-sample output hop。
4. finalize 时用零补齐足够 frame，释放所有真实输入对应输出。
5. 丢弃首部预填对应输出，裁剪尾部，使 BF WAV frame 数严格等于 BF 收到的真实 MIC frame 数。
6. 算法输出内部使用 f32；只在写 PCM16 前做 gain、round、saturate。
7. 热路径复用 FFT planner、scratch、spectrum、frame、OLA 和 output Vec。

增加独立 WOLA bypass 测试：

- 输入单通道确定性信号；
- 不做 BF，只走 analysis + synthesis；
- 忽略浮点容差后，输出长度和波形与输入一致；
- 一次输入与随机块长输入结果一致。

---

## 16. BF 方向选择与平滑

`PipelineState` 继续维护最新 DOA，BF 每个 256-frame hop 读取一次。

规则：

### `direction_source = fixed`

```text
target = fixed_internal_angle_deg
```

### `direction_source = doa`

```text
Tracking:
    target = tracked_internal_deg

Coasting:
    若 tracked_internal_deg 存在，继续使用 tracker 预测角

Searching / 无结果:
    target = fallback_internal_angle_deg
```

禁止使用：

```text
raw_angle_deg
tracked_angle_deg
Viewer 旋转角
```

方向平滑使用 circular EMA：

```text
delta = circular_delta(target, current)
alpha = 1 - exp(-hop_seconds / tau_seconds)
current = wrap360(current + alpha * delta)
```

其中：

```text
hop_seconds = 256 / 16000 = 0.016
```

若 `direction_smoothing_ms == 0`，直接使用 target。

LUT 选择首版使用最近整数角：

```text
index = round(current_deg) mod 360
```

不要在本次引入连续角 LUT 插值；1° LUT 加平滑已经足够。后续有真实听感证据再扩展。

---

## 17. BeamformerRuntime 输出

`BeamformerRuntime` 初始化时：

- 校验配置；
- 生成 LUT；
- 创建 streaming STFT；
- `wav = true` 时创建：

```text
{out_dir}/{prefix}_respeaker_bf.wav
```

输出为：

```text
16 kHz
1 channel
PCM16
标准 WAVEFORMATEX
```

复用现有 `WavSink`。

转换：

```text
gain = 10^(output_gain_db / 20)
y_pcm = saturate(round(y * gain * 32768))
```

统计：

```rust
pub struct BeamformerStats {
    pub input_frames: u64,
    pub output_frames: u64,
    pub stft_frames: u64,
    pub clipped_samples: u64,
    pub das_fallback_bins: u64,
    pub min_generated_wng_db: f32,
}
```

finalize 时检查：

```text
output_frames == input_frames
```

否则返回错误。

终端只在结束时输出一次 BF 统计，避免实时日志风暴。

---

## 18. PipelineRuntime 集成

扩展：

```rust
enum PipelineModuleRuntime {
    Doa(DoaRuntime),
    Beamformer(BeamformerRuntime),
}
```

`PipelineState` 至少保留：

```rust
#[derive(Default)]
struct PipelineState {
    latest_doa: Option<DoaResult>,
}
```

固定 256-frame `push_block()` 的模块顺序：

```text
for module in configured order:
    DOA:
        process current mic hop
        update latest_doa
        publish CSV / console / Viewer

    Beamformer:
        choose direction from latest_doa or fixed config
        process current mic hop
        write BF WAV
```

不要把 BF 放到另一个独立线程；DOA 和 BF 属于同一算法 worker，保证状态与 hop 顺序一致。

`finalize()` 顺序：

1. 按模块声明顺序调用各 runtime 的 `finalize()`；DOA 自己 flush CSV，BF 自己 flush STFT/WAV；
2. 所有模块 finalize 后关闭 Web Server；
3. 聚合并返回错误，同时尽量继续完成其余资源关闭。

不要在模块循环之外再次重复 finalize DOA 或 BF。Web Server 保持最后关闭。

---

## 19. 文件级修改清单

### 新增

```text
src/audio.rs
src/pipeline_worker.rs
src/beamformer/mod.rs
src/beamformer/stft.rs
src/beamformer/weights.rs
src/beamformer/matrix.rs
configs/doa_bf.toml
configs/bf_fixed.toml
```

### 修改

```text
src/main.rs
    注册 audio / pipeline_worker / beamformer 模块

src/wasapi.rs
    CaptureBlock
    CAPTURE_QUEUE_CAPACITY = 128
    try_send
    JoinHandle<Result>
    stop_and_join
    overrun error propagation

src/recorder.rs
    worker startup
    nonblocking algorithm dispatch
    PipelineActive / PipelineFailed 降级语义
    Pipeline 首错后关闭输入且不再 try_push
    raw-only continuation
    drain shutdown
    explicit capture/worker join
    final stats

src/pipeline.rs
    public PipelineConfig ownership as worker input
    PipelineInputBlock metadata
    Beamformer config、逐字段 serde default 和 validation
    Beamformer runtime integration
    ordering constraints

src/doa/mod.rs
    preserve internal angles

src/doa/output.rs
    继续只输出 external angle；按新增字段修复构造/测试

README.md
    新线程架构
    Pipeline failure/raw continuation 语义
    raw WAV I/O 回压限制
    BF 配置
    输出文件
    算法和角度定义
    实时延迟与限制

docs/plans/respeaker_pipeline_thread_bf_codex_plan.md
    本文是唯一方案真源
    实现后只在文末追加“实现结果”

Cargo.toml
    原则上无需新增线性代数依赖
```

**不得新增或复制** `docs/plans/pipeline_thread_beamformer_plan.md`，也不得创建其它同内容别名方案。若仓库中已经误生成副本，保留本文件并删除未被其它文档引用的副本，避免双份漂移。

根据实际模块声明位置同步调整 `lib.rs` / `main.rs`；不要机械新增不存在的文件。

## 20. 自动测试

### 20.1 audio / capture

- `capture_block_validates_length`
- `capture_block_rejects_bad_length`
- `capture_try_send_reports_full_without_waiting`
- `capture_sequence_and_start_frame_are_contiguous`
- `capture_queue_capacity_remains_128`

### 20.2 pipeline worker

- `hop_assembler_emits_exact_256_frame_blocks`
- `hop_assembler_is_capture_packet_size_independent`
- `hop_assembler_emits_partial_tail_on_finish`
- `worker_rejects_sequence_gap`
- `worker_rejects_start_frame_gap`
- `pipeline_try_push_reports_full`
- `worker_init_error_is_returned_before_spawn_success`
- `queue_depth_preincrement_rolls_back_on_full`
- `queue_depth_preincrement_rolls_back_on_disconnect`
- `queue_depth_never_underflows_when_receiver_runs_immediately`
- `close_input_does_not_block_waiting_for_worker`

队列深度竞态测试应构造“receiver 在 sender 返回前立即 recv”的确定性同步场景，验证计数不会绕回 `usize::MAX`。不要只断言最终值为 0，还要断言 high-watermark 不出现异常极大值。

### 20.3 recorder / 过载语义

通过可注入的小容量 worker 或测试 stub，不依赖真实声卡，至少覆盖：

- `pipeline_full_disables_pipeline_but_raw_recording_continues`
- `pipeline_disconnect_disables_pipeline_but_raw_recording_continues`
- `pipeline_first_error_stops_all_future_try_push_calls`
- `pipeline_failure_is_returned_after_raw_wavs_are_finalized`
- `capture_overrun_remains_fatal`
- `raw_write_error_stops_capture_and_finalizes_headers_best_effort`

核心断言：Pipeline 在中途失败后，后续所有 CaptureBlock 仍写入三路 raw WAV；最终 raw frame 数等于 recorder 已收到的全部 capture frame；算法分支不会在失败后恢复或反复报错。

### 20.4 DOA / 双重分帧

保留原有测试并新增：

- `doa_result_preserves_internal_angle_before_output_transform`
- `beamformer_never_uses_viewer_or_external_angle`
- `doa_frame_assembler_is_preserved_behind_256_hop_assembler`
- `first_doa_result_occurs_at_1024_samples`
- `beamformer_uses_fallback_before_first_doa_result`

`beamformer_never_uses_viewer_or_external_angle` 可以通过 Pipeline 合成测试：设置明显的 offset/clockwise，检查 BF 仍对内部合成方向取得最大输出。

### 20.5 matrix / weights

- `cholesky_solves_known_spd_matrix`
- `cholesky_rejects_non_spd_matrix`
- `diffuse_covariance_is_symmetric_with_unit_diagonal`
- `steering_vector_has_unit_norm_elements`
- `das_weights_are_distortionless`
- `superdirective_weights_are_distortionless`
- `all_non_nyquist_generated_weights_meet_wng_floor`
- `dc_weights_are_real_and_nyquist_is_zero`
- `invalid_or_singular_case_falls_back_to_das`
- `frequency_mix_has_expected_boundaries`

容差建议：

```text
对 bin `0..FFT_BINS-1`（排除 Nyquist）：`|w^H d - 1| < 1e-4` after f32 conversion
对参与 WNG 约束的非 Nyquist bin：`WNG_dB >= min_wng_db - 0.05 dB`
```

### 20.6 STFT / BF

- `sqrt_hann_cola_square_sum`
- `wola_bypass_roundtrip`
- `wola_chunk_size_independent`
- `wola_finalize_preserves_exact_length`
- `fixed_das_preserves_target_plane_wave`
- `fixed_bf_rejects_wrong_input_length`
- `doa_source_uses_tracking_and_coasting`
- `doa_source_falls_back_while_searching`
- `direction_smoothing_crosses_zero_degrees_correctly`
- `beamformer_output_pcm_saturates_without_wraparound`

### 20.7 Pipeline 配置 / Serde defaults

- 原 `configs/doa.toml` 继续有效；
- `configs/doa_bf.toml` 有效；
- `configs/bf_fixed.toml` 有效；
- `beamformer_omitted_fields_match_documented_defaults` 逐字段校验所有默认值；
- BF 使用 DOA 但位于 DOA 前时拒绝；
- BF 使用 DOA 但没有 DOA 时拒绝；
- fixed BF 无 DOA 时允许；
- 重复 BF 拒绝；
- 非法频率顺序、WNG、NaN/Inf 拒绝；
- unknown field 继续拒绝。

### 20.8 录音契约

保留并扩展现有测试：

- raw algo/mic/ref buffer 内容不因 Pipeline 改变；
- 开启 BF 后三个原始 WAV 的通道数和样本内容不变；
- Pipeline 中途失败后三个原始 WAV 仍包含失败后的 capture block；
- 无配置时不创建 BF WAV / DOA CSV / Viewer；
- BF WAV 为 mono、16 kHz、PCM16、plain WAVEFORMATEX。

## 21. 性能与实时要求

1. WASAPI thread：
   - 不调用阻塞 send；
   - 不做算法和文件 I/O；
   - 每 packet 只发生现有 PCM Vec 构造和一次 `Arc` 接管；
   - capture queue `Full` 立即 fail-fast，不静默丢 packet。
2. recorder loop：
   - 算法投递只 clone Arc + `try_send`；
   - algorithm worker 慢或失败时，立即关闭 Pipeline 输入并转为 raw-only，不等待 worker，也不停止 capture；
   - raw WAV 仍同步写入，因此磁盘 I/O 可能阻塞 recorder 并最终导致 capture overrun。
3. algorithm worker：
   - 每 16 ms 处理一个 hop；
   - LUT 初始化只发生一次；
   - 每 hop 不做矩阵求解；
   - FFT、scratch、frame、OLA、result Vec 全部复用。
4. 正常目标：

```text
combined DOA + BF p99 processing time < 16 ms / hop
10 分钟真机运行 algorithm queue 不持续增长
capture overrun = 0
pipeline overrun = 0
raw WAV write error = 0
```

若 Pipeline overrun/runtime error 实际发生，验收重点不是假装无错误，而是确认：

```text
Pipeline 只报一次并停止接收新块
raw 录音继续到正常结束
最终进程返回 Pipeline error
BF/CSV 等算法输出明确是不完整的
```

5. BF 采用 512 点 causal frame；实时生成首个有效输出需要累计 512 个输入 frame，即约 32 ms。BF WAV 不插入 32 ms 前导零，文件 sample 0 仍对应输入 sample 0；这是处理延迟，不是文件时间轴偏移。

如需要性能观测，可在 algorithm worker 内使用 `Instant` 统计每 hop 的 max / mean / p99 近似值，但不要逐帧打印。`max_queue_depth` 只是保守观测值，不得用于过载正确性判断。

## 22. README 更新要求

README 必须明确：

1. 当前程序有三个执行线程角色：capture、recorder、algorithm worker。
2. Pipeline worker 只有传入配置时启动。
3. 两类队列过载语义不同：

```text
capture queue Full
    无法保证 raw 连续性
    → 停止本次录制并返回 capture overrun

algorithm queue Full / worker failure
    → 关闭 Pipeline 输入
    → 后续只录 raw
    → 不再 try_push、不重启算法
    → 录制结束后返回 Pipeline error
```

4. 已解除的是 DOA/BF/CSV/Viewer 对采集的回压；三个 raw WAV 仍由 recorder 同步写入，磁盘卡顿仍可能导致 capture queue overrun。
5. 新增 BF 输出文件。
6. 两种 BF：

```text
Delay-and-Sum
WNG-constrained robust superdirective MVDR
```

7. robust superdirective 不是在线噪声协方差 MVDR；它使用理论 diffuse covariance。
8. BF 角度使用内部阵列坐标：

```text
+X = 0°
+Y = 90°
CCW
```

9. DOA external angle、Viewer 显示旋转和 BF internal angle 的区别。
10. worker 的 256-frame hop assembler 不替代 DOA 512/256 `FrameAssembler`；第一条 DOA 结果约在 1024 samples / 64 ms，之前 BF 使用 fallback 角。
11. `configs/doa_bf.toml`、`configs/bf_fixed.toml` 使用示例和字段默认值。
12. BF 的 STFT 参数、约 32 ms 处理延迟和频带混合默认值。
13. 当前限制：单目标、二维远场方向、固定 4-Mic 几何、无在线 VAD/噪声协方差、raw WAV I/O 尚未线程解耦。

## 23. 实现顺序

严格按以下阶段推进，每阶段先通过相关测试再继续：

### Phase A：线程、块模型与过载语义

1. 新增 `audio.rs` / `CaptureBlock`。
2. 明确 `CAPTURE_QUEUE_CAPACITY = 128`。
3. WASAPI 改为 `try_send` 和错误返回。
4. 新增 PipelineWorker、启动握手和 `close_input()`。
5. 队列深度统计实现“先预增、发送失败回滚、recv 后递减”。
6. recorder 改为 raw write + nonblocking worker dispatch。
7. 实现 Pipeline 首错后关闭输入、永久停用算法、raw-only 继续。
8. 完成 drain/finalize/join；Pipeline 失败时不得在 recorder loop 内阻塞 join。
9. 运行现有全部测试，确认 DOA 数学输出未改变。

### Phase B：固定 hop 与内部角

1. worker 内实现 256-frame assembler。
2. **保留 DOA `FrameAssembler`，不得用 Pipeline assembler 替代。**
3. 扩展 `PipelineInputBlock` metadata。
4. `DoaResult` 保存 internal angle。
5. 写死并测试 512/768 warmup、1024 samples 首个 `DoaResult` 的时间线。
6. 增加顺序、块边界和坐标测试。

### Phase C：Delay-and-Sum BF

1. 实现 streaming STFT/WOLA。
2. 实现 steering / DAS LUT。
3. 接入 fixed direction。
4. 输出 BF WAV。
5. 完成 identity、长度、合成平面波测试。

### Phase D：DOA 联动

1. 添加 Beamformer 配置、逐字段 Serde defaults 和 Pipeline ordering validation。
2. consume `tracked_internal_deg`。
3. Tracking / Coasting / Searching 策略。
4. circular smoothing。
5. 验证首个 DOA 结果前（约 1024 samples / 64 ms）BF 使用 `fallback_internal_angle_deg`；该测试依赖本阶段方向选择逻辑，不得提前到 Phase B。
6. 添加 `doa_bf.toml`，并验证省略字段使用默认值。

### Phase E：Robust Superdirective

1. 4x4 Cholesky。
2. diffuse covariance。
3. MVDR weights。
4. WNG loading search。
5. 频带混合和 DAS fallback。
6. 全 LUT 数值测试。

### Phase F：文档与完整验证

1. 更新 README。
2. 以 `docs/plans/respeaker_pipeline_thread_bf_codex_plan.md` 为唯一方案文件，在文末追加“实现结果”；不得复制成第二个文件名。
3. 运行完整命令。
4. 给出未执行的硬件验证项，不得伪称已验证。

## 24. 验证命令

必须运行：

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

在自动测试中使用小容量 algorithm queue 或注入 worker stall/error，验证：

```text
Pipeline 过载后 raw 继续
后续不再 try_push
raw WAV 已 finalize 且长度完整
最终返回 Pipeline error
```

真机 smoke test：

```powershell
# 保持纯录音兼容
cargo run --release -- --duration 10 --out-dir target/out

# 现有 DOA 配置
cargo run --release -- --duration 10 --out-dir target/out --pipeline-config configs/doa.toml

# DOA + robust BF
cargo run --release -- --duration 30 --out-dir target/out --pipeline-config configs/doa_bf.toml

# 固定角 DAS
cargo run --release -- --duration 30 --out-dir target/out --pipeline-config configs/bf_fixed.toml
```

真机检查：

- 在无 capture/raw I/O 错误时，3 个原始 WAV 正常且连续；
- DOA CSV / Viewer 正常；
- BF WAV 长度与 raw mic WAV 每通道 frame 数相同；
- 正常负载下 capture/pipeline overrun 均为 0；
- Ctrl+C 后所有 WAV 头可读；
- Viewer 关闭、端口释放；
- 固定角与 DOA 角的声像方向一致；
- 对 0°、90°、180°、270° 已知方向，DAS 不出现明显相位抵消；
- robust BF 在风扇、空调或多方向扩散噪声下与 DAS 对比，不应出现低频爆噪、自激式增益或明显目标自消；
- 人为制造 Pipeline 运行时错误时，终端只报告一次降级，raw 继续到设定时长，最终退出状态为失败；
- 人为制造磁盘或 capture queue overrun 时，确认这是致命录音错误，不把它误归因于算法线程解耦失败。

建议保留对比文件：

```text
*_respeaker_algo.wav
*_respeaker_mic.wav
*_respeaker_bf.wav  # DAS
*_respeaker_bf.wav  # robust，使用不同 prefix 录制
```

## 25. 完成标准

只有同时满足以下条件才算完成：

1. Pipeline 完全运行在独立 algorithm worker。
2. WASAPI 与 recorder→pipeline 均无阻塞 send。
3. capture queue 容量明确为 128；capture `Full` 时致命 fail-fast，不静默丢 packet。
4. algorithm queue `Full`、worker 断开或 Pipeline 运行时错误时：
   - 只记录首个错误到 `Failed.first_error`；
   - 立即关闭输入；
   - 后续不再 `try_push`；
   - 不停止 capture；
   - raw 继续写到正常结束；
   - 最终返回该 Pipeline 错误，且 `first_error` 不被后续 `finish() == Ok` 覆盖。
5. Pipeline 失败时 recorder 不同步等待 worker join；shutdown 能 drain capture queue，并尽力 finalize 所有 WAV/CSV/Web 生命周期。
6. 队列深度统计不存在 `AtomicUsize` underflow；发送失败能回滚预增。
7. 原始三路 WAV 行为和通道契约不变。
8. 文档明确 raw WAV I/O 仍同步，磁盘回压仍可能导致致命 capture overrun。
9. worker 的 256-frame assembler 与 DOA 512/256 `FrameAssembler` 同时保留；第一条 DOA 结果在约 1024 samples，之前 BF 使用 fallback。
10. 每个可省略的 Beamformer 字段都有代码级 Serde default，且示例配置省略字段仍可解析。
11. 旧 `configs/doa.toml` 无需修改即可继续运行。
12. fixed DAS BF 可单独运行并生成等长 mono WAV。
13. DOA→BF 使用 internal tracked angle，而非 external/Viewer angle。
14. robust superdirective 权重满足 distortionless 和 WNG 测试。
15. 数值异常逐 bin 回退 DAS，不 panic。
16. 所有自动测试、fmt、clippy、release build 通过。
17. README 和两个新配置完整。
18. `docs/plans/respeaker_pipeline_thread_bf_codex_plan.md` 是唯一方案文件，文末已追加实现结果，没有同内容副本。
19. 最终报告明确列出：
    - 修改文件；
    - 线程和关闭语义；
    - Pipeline 过载后的 raw-only 行为；
    - BF 数学实现；
    - 默认参数；
    - 自动测试结果；
    - 是否做过真实 ReSpeaker 测试；
    - raw WAV I/O 回压等仍存在的限制或风险。

## 26. 实现后最终回复格式

Codex 完成后使用以下结构汇报：

```text
Implementation summary
- ...

Key design decisions
- ...

Files changed
- ...

Validation
- cargo fmt ...: PASS/FAIL
- cargo clippy ...: PASS/FAIL
- cargo test ...: PASS/FAIL
- cargo build --release: PASS/FAIL
- ReSpeaker hardware test: RUN / NOT RUN

Performance / output contract
- ...

Remaining risks
- ...
```

不得只说“已完成”；必须给出可核对的命令结果和未执行项。

---

## 27. 实现结果

> 状态：已实现（代码在分支 `feat/pipeline-thread-bf`）。

### 实际修改

- 新增 `src/audio.rs`、`src/pipeline_worker.rs`、`src/beamformer/`（mod/stft/weights/matrix）
- 改造 `wasapi.rs`（`CaptureBlock` + `try_send` + `stop_and_join`）、`recorder.rs`（worker 调度与 Pipeline 降级）、`pipeline.rs`（BF 模块与 hop metadata）
- `doa/mod.rs` 保留 internal 角；新增 `configs/doa_bf.toml`、`configs/bf_fixed.toml`；更新 `README.md`
- worker 首错在 finalize 前对发送端可见；raw WAV 在 WASAPI 启动前创建；BF hop 热路径不再复制权重和输出 Vec
- 使用小容量 stalled worker 与测试故障注入，确定性覆盖 Pipeline Full/Disconnected、raw-only 延续、capture overrun 和 raw finalize

### 验证结果

- `cargo fmt --all -- --check`：PASS
- `cargo clippy --all-targets --all-features -- -D warnings`：PASS
- `cargo test --all-targets`：PASS（95 tests）
- `cargo build --release`：PASS
- ReSpeaker 真机测试：NOT RUN

### 与方案的偏差及原因

- 无功能性偏差；clippy 清理时对测试专用 API 使用了 `#[cfg(test)]`，公开未读字段保留并 `allow(dead_code)`。

### 剩余风险

- raw WAV I/O 仍在 recorder 同步执行；磁盘回压仍可能导致 capture overrun。
- 真机下 DOA+BF 的听感与 overrun 统计尚未验证。
