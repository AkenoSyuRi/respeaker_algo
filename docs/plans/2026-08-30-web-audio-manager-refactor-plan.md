# ReSpeaker Web 音频管理界面重构执行计划

> 执行对象：Codex / coding agent
> 仓库：`AkenoSyuRi/respeaker_algo`
> 设计基线：`9df2a2193f60568a7ecea8689fc1a42fbbe583a8`
> 目标平台：Windows、Rust 2024、ReSpeaker Mic Array v2.0
> 目标形态：程序无参数启动后提供常驻本地 Web 管理界面；录音与算法由 Web 控制
> 本文是实施方案，不是当前实现说明。执行时必须重新核对实际代码，不得回退基线之后的功能。

---

## 0. 执行要求

开始修改前：

1. 阅读仓库根目录当前的 `AGENTS.md`、`README.md` 和本文。
2. 执行 `git rev-parse HEAD`、`git status --short`、`git diff --stat`、`git diff --cached --stat`。
3. 若 HEAD 已不同于本文基线，以当前源码为事实来源适配本文，不得 reset、checkout 或覆盖用户修改。
4. 先运行当前基线的自动验证，记录结果：

   ```powershell
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-targets
   cargo build --release
   ```

5. 按本文阶段顺序实施。每一阶段先通过本阶段窄测试，再进入下一阶段。
6. 不重写 DOA、Beamformer、DRC、WASAPI、WAV 核心算法；本次主要重构控制面、生命周期、配置与展示。
7. 不在 WASAPI 采集线程中执行 Web、JSON、文件列表扫描、FFT、SRP、Kalman、BF、DRC、CSV 或阻塞日志。
8. 不引入 React、Node、前端构建系统、数据库、动态插件框架或通用工作流引擎。第一版使用 Axum + 原生 HTML/CSS/JavaScript。
9. 不创建 Git commit，除非用户另行明确要求。

## 1. 当前实现事实

基线代码是一次性 CLI 录音程序：

```text
main::Cli
  -> recorder::run_record
       -> optional PipelineConfig::load(path)
       -> optional PipelineWorkerHandle::spawn
       -> WASAPI capture
       -> recorder loop
       -> raw WAV finalize
       -> Pipeline finalize
       -> process exit
```

必须保留的当前行为：

- 固定 Windows WASAPI 独占模式、16 kHz、PCM16、6 通道、channel mask 0。
- 通道语义保持：`ch0` 固件算法输出，`ch1..ch4` 原始 MIC，`ch5` playback/AEC reference。
- 三路 raw WAV 始终为：
  - `*_respeaker_algo.wav`
  - `*_respeaker_mic.wav`
  - `*_respeaker_ref.wav`
- capture queue Full 是致命错误：无法保证 raw 连续性时停止录音。
- algorithm queue Full、worker 断开或算法首错只停用 Pipeline 输入；raw 录音继续，录音结束后仍返回 Pipeline 错误。
- raw WAV 先 finalize，再等待 Pipeline 收尾；Pipeline 首错不得丢失或重复拼接。
- Pipeline worker 继续使用固定 256-frame hop，DOA 内部继续使用独立 512/256 `FrameAssembler`。
- DOA internal 角供 BF steering；external 角供终端、CSV、Web 显示；Viewer 显示旋转不得进入 BF。
- BF 输出继续保持 512/256 periodic sqrt-Hann WOLA、输入输出主帧数一致和尾部 flush。
- `enable_drc = true` 继续是 Beamformer 缺省值；DRC 位于 output gain 之后、PCM16 转换之前。
- `compare_wav = true` 时左右声道分别使用独立 DRC 状态。
- 自动测试不依赖真实声卡；真机结果必须单独报告。

当前不适合直接套 Web 的耦合点：

- `run_record()` 同时拥有配置路径解析、Ctrl+C 注册、文件初始化、采集、阻塞循环和收尾。
- `ctrlc::set_handler()` 位于单次录音函数内，无法支持同一进程反复开始/停止录音。
- `RecordOptions` 接受配置文件路径，而不是已经验证的内存配置快照。
- `PipelineRuntime` 拥有 DOA Web Server 生命周期，导致没有 DOA 时 Web 不存在。
- 当前 Web 只发布 DOA，不能表示设备、录音、Pipeline 降级、文件与配置状态。

## 2. 最终产品行为

最终可执行程序不再依赖录音 CLI 参数：

```powershell
target\release\respeaker_algo.exe
```

启动行为：

1. 加载持久化 App 配置；配置损坏时使用安全默认值，并把解析错误展示在 Web 中，不因用户配置损坏而阻止 Web 启动。
2. 启动 `AudioController`。
3. 绑定 `127.0.0.1:8765`。
4. 打印 Web URL，并尝试打开默认浏览器；打开浏览器失败只告警，不退出服务。
5. 初始状态为 `Idle`，不自动占用 ReSpeaker，也不创建录音文件。
6. 用户在 Web 点击开始后才初始化输出、Pipeline 和 WASAPI。
7. 关闭浏览器页面不停止录音；程序进程和 Controller 仍然存活。
8. Ctrl+C 触发全局 graceful shutdown：若正在录音，先停止并完成所有 finalize，再关闭 Web 和进程。

第一版默认只允许本机访问：

- 固定绑定 loopback，不提供 `0.0.0.0` 配置。
- 不实现账户、远程访问、TLS 或局域网共享。
- 若以后要开放局域网，必须另写安全设计，至少覆盖认证、CSRF、目录权限和网络暴露。

## 3. 本次范围

### 3.1 必须完成

- 常驻本地 Web 管理界面。
- 可重复开始、停止多次录音，无需重启进程。
- 在 Web 中读取、编辑、校验、保存和恢复配置。
- 支持纯录音、DOA、DOA + BF、固定方向 BF 四种当前能力组合。
- 大部分当前 TOML 字段有直接控件；低频高级字段放入“高级设置”。
- 运行中展示设备、录音时间/帧数、Pipeline 状态、DOA、BF 统计和错误。
- 浏览、播放/下载和安全移入回收区当前录音结果。
- 读取现有 `configs/*.toml` 作为可导入 preset，但最终正常操作不要求用户手写配置文件。
- 保留当前所有音频、队列、错误、角度、WAV 和 DRC contract。
- 更新 README、AGENTS 和配置说明，使公开文档与最终 Web 行为一致。

### 3.2 第一版明确不做

- 浏览器实时 PCM/Opus 音频监听。
- 浏览器麦克风输入或 WebRTC。
- 局域网远程访问、账户系统、多用户并发控制。
- 运行中任意修改全部 DSP 参数。
- 任意设备、采样率、通道数或 channel mapping。
- AEC、FDAEC、RES、NS 或新的 DSP 算法。
- Pipeline 拖拽编排器、动态插件、脚本模块、外部进程模块。
- 改变 BF/DRC 算法数学或重新调参。

## 4. 最终架构与 ownership

```text
main
  ├─ load AppConfigStore
  ├─ register Ctrl+C once
  ├─ spawn AudioController
  └─ start WebServer
          │
          ├─ REST requests -> ControllerCommand
          └─ SSE <- AppEventBus

AudioController（唯一服务状态 owner）
  ├─ Idle / Starting / Recording / Stopping
  ├─ current draft + saved profiles
  ├─ active RecordingSession
  ├─ last session/error/status snapshot
  └─ recording catalog

RecordingSession thread
  ├─ immutable ActiveSessionConfig
  ├─ external stop flag
  ├─ raw WAV writers
  ├─ WASAPI capture session
  └─ optional PipelineWorkerHandle
          ├─ DOA -> latest_doa -> events/CSV
          └─ BF -> gain -> optional DRC -> BF WAV
```

ownership 规则：

- Axum handler 不直接持有、启动、停止或 join WASAPI/Pipeline。
- `AudioController` 是服务状态和当前 session handle 的唯一 owner。
- 每次 `RecordingSession` 拥有不可变的 active config snapshot。
- Web 编辑的是下一次录音使用的 draft；录音中修改 draft 不影响当前 session。
- 音频线程不获取 Web 状态锁；状态与事件通过非阻塞原子计数或通道发布。
- DOA/BF 不拥有 HTTP Server；Pipeline 只持有轻量事件 publisher。
- 录音文件目录扫描不在 recorder loop 或 algorithm worker 中执行。

## 5. 服务状态机

使用显式状态，而不是多个松散布尔值：

```rust
enum ServicePhase {
    Idle,
    Starting { session_id: String },
    Recording { session_id: String },
    Stopping { session_id: String },
}
```

另行保存：

```rust
struct ServiceSnapshot {
    revision: u64,
    phase: ServicePhase,
    device: DeviceSnapshot,
    recording: Option<RecordingSnapshot>,
    pipeline: PipelineSnapshot,
    last_error: Option<ServiceError>,
    config_warning: Option<String>,
}
```

合法转换：

```text
Idle --Start--> Starting
Starting --Ready--> Recording
Starting --Failure--> Idle + last_error
Recording --Stop/Ctrl+C/Duration--> Stopping
Recording --CaptureFatal--> Stopping
Recording --PipelineFailure--> Recording + PipelineDegraded
Stopping --Finished--> Idle + last_session
```

状态规则：

- `Start` 仅在 Idle 接受；否则返回 HTTP 409。
- `Stop` 仅在 Starting/Recording 接受；重复 Stop 幂等返回当前 Stopping 状态。
- Pipeline 降级不把服务相位改为 Failed，也不停止 raw 录音。
- capture/raw WAV 失败进入 Stopping，最终 session 标记失败。
- session 失败后服务回到 Idle，允许用户修正后再次开始。
- 每次状态变化递增 `revision` 并发布完整 `state_snapshot` 事件。

## 6. 配置模型

### 6.1 三层配置

严格区分：

1. `DraftConfig`：Web 当前编辑值。
2. `SavedProfile`：持久化配置档案。
3. `ActiveSessionConfig`：点击开始时克隆并冻结的配置快照。

第一版不实现 DSP 热更新。录音进行中仍可编辑和保存 draft，但 UI 必须显示“下次录音生效”。

### 6.2 App 配置结构

新增版本化 App 配置，复用现有 `PipelineConfig`，不得在前后端复制两套默认值：

```rust
struct AppConfig {
    version: u32,
    recording: RecordingConfig,
    pipeline_enabled: bool,
    pipeline: PipelineConfig,
}

struct RecordingConfig {
    duration_seconds: u64,
    out_dir: String,
    prefix: Option<String>,
}
```

要求：

- `PipelineConfig`、`ModuleConfig` 增加内部所需的 `Serialize`、`Clone` 和访问/转换接口。
- 保留 `serde(deny_unknown_fields)`。
- 默认 `pipeline_enabled = false`，即纯录音。
- Web 可以启用/禁用 DOA、BF，但后端仍执行当前版本和依赖校验。
- `direction_source = "doa"` 时后端自动要求 enabled DOA 位于 BF 前面。
- 最多各启用一个 DOA/BF。
- `enable_drc` 缺省仍为 true。
- 不把固定硬件契约暴露成可编辑配置。

### 6.3 配置存储

第一版使用：

```text
%LOCALAPPDATA%\respeaker_algo\config.toml
%LOCALAPPDATA%\respeaker_algo\profiles\*.toml
```

若 `LOCALAPPDATA` 不存在，启动失败并给出明确错误；不要静默写入仓库或可执行文件目录。

写入要求：

- 先写同目录临时文件，flush 后原子 rename，避免半写配置。
- profile 名只允许安全文件名字符，API 不接受任意路径。
- 配置损坏时保留原文件，不自动覆盖；Web 展示错误并提供“恢复默认值”和重新保存。
- 提供 TOML 导入/导出。浏览器用 `FileReader` 读取用户选择的文件内容，再把文本发送给 API；服务端不接受浏览器传来的本地路径。
- 将 `configs/doa.toml`、`configs/doa_bf.toml`、`configs/bf_fixed.toml` 作为内置 preset 数据源或等价测试夹具。

### 6.4 第一版控件分组

主界面直接展示：

- 录音：时长、输出目录、可选前缀。
- DOA：启用、CSV、角度 offset、clockwise、acquire/update confidence。
- BF：启用、算法、方向来源、fixed/fallback angle、方向平滑、输出增益、WAV、compare WAV。
- DRC：启用。

折叠的高级设置展示：

- DOA：`beta`、`cpsd_tau_ms`、`max_coast_ms`。
- BF：`min_wng_db` 和四个 superdirective 频带参数。

本次不把 `DrcConfig` 全部内部系数暴露到 UI；先保留当前固定 pipeline preset。若以后开放完整 DRC 参数，必须另行定义验证、运行中状态重置和兼容策略。

## 7. Web API 与事件协议

### 7.1 REST API

实现以下本地同源接口：

| Method | Path | 行为 |
|---|---|---|
| GET | `/api/status` | 返回完整 `ServiceSnapshot` |
| GET | `/api/config` | 返回当前 draft 和配置告警 |
| PUT | `/api/config` | 校验并更新 draft；不自动影响 active session |
| POST | `/api/config/save` | 原子保存当前 draft |
| POST | `/api/config/reset` | 恢复代码默认值，不直接开始录音 |
| POST | `/api/config/import` | 接收 TOML 文本，解析后替换 draft |
| GET | `/api/config/export` | 下载当前 draft TOML |
| GET | `/api/profiles` | 列出 profile |
| PUT | `/api/profiles/{name}` | 保存或覆盖指定 profile |
| GET | `/api/profiles/{name}` | 读取 profile |
| DELETE | `/api/profiles/{name}` | 删除 profile；严格校验 name |
| POST | `/api/recordings/start` | 冻结 draft 并异步开始；返回 202 + session id |
| POST | `/api/recordings/stop` | 请求停止；返回 202 |
| GET | `/api/recordings` | 列出历史 session/legacy 文件组 |
| GET | `/api/recordings/{id}` | 返回 manifest 与文件列表 |
| GET | `/api/recordings/{id}/files/{kind}` | allowlist 文件下载/播放，支持 HTTP Range |
| POST | `/api/recordings/{id}/trash` | 把整个 session 文件组移入 `.trash` |
| POST | `/api/recordings/{id}/restore` | 从 `.trash` 恢复 |
| GET | `/api/events` | SSE 统一事件流 |

API 错误统一返回：

```json
{
  "error": {
    "code": "invalid_config",
    "message": "update_confidence 不得大于 acquire_confidence",
    "field": "pipeline.modules[0].update_confidence"
  }
}
```

HTTP 语义：

- 400：JSON/TOML/字段格式错误。
- 404：profile、session 或文件类型不存在。
- 409：当前状态不允许该操作、输出目标冲突、配置依赖冲突。
- 422：结构正确但业务校验失败。
- 500：内部 I/O、线程或不可恢复服务错误。

### 7.2 SSE 事件

使用统一 envelope：

```json
{
  "type": "doa",
  "seq": 123,
  "timestamp_ms": 1024.0,
  "payload": {}
}
```

事件类型至少包含：

- `state_snapshot`
- `device_status`
- `recording_progress`
- `pipeline_status`
- `doa`
- `bf_stats`
- `session_finished`
- `error`

事件规则：

- 新 SSE 客户端先收到完整 `state_snapshot`，再接收 live events。
- DOA 继续以算法结果节拍发布；慢浏览器只丢 UI 事件，不得回压 algorithm worker。
- broadcast lag 后客户端必须重新收到最新 snapshot，不能静默永久缺状态。
- `error` 使用结构化来源：`config`、`device`、`capture`、`raw_wav`、`pipeline`、`web`。
- Web JSON 序列化失败不得在音频线程阻塞重试。

## 8. 录音 session、文件与 manifest

### 8.1 保持现有输出命名

第一版继续使用当前平铺输出，避免破坏已有脚本：

```text
{prefix}_respeaker_algo.wav
{prefix}_respeaker_mic.wav
{prefix}_respeaker_ref.wav
{prefix}_respeaker_bf.wav        # 可选
{prefix}_respeaker_doa.csv       # 可选
{prefix}_respeaker_session.json  # 新增 manifest
```

开始录音前计算所有可能目标路径。只要任一目标已存在就返回 409，不得用 `File::create` 静默覆盖历史录音。

### 8.2 Session manifest

manifest 至少保存：

```rust
struct SessionManifest {
    schema_version: u32,
    session_id: String,
    prefix: String,
    started_at: String,
    finished_at: Option<String>,
    status: SessionResultStatus,
    active_config: AppConfig,
    captured_frames: u64,
    pipeline_stats: Option<PipelineWorkerStats>,
    bf_stats: Option<BeamformerStats>,
    error: Option<ServiceError>,
    files: Vec<SessionFile>,
}
```

要求：

- start 时先创建状态为 `starting` 的 manifest 临时数据；录音成功启动后写 `recording`。
- finalize 完成后原子写最终 manifest。
- 进程异常退出后若留下非终态 manifest，列表中显示 `interrupted`，不得伪装成成功。
- 旧录音没有 manifest 时，按已知后缀和共同 prefix 分组为 `legacy`，只提供查看、下载和移入回收区。
- 4ch MIC WAV 可能不被浏览器原生播放器正确播放；UI 允许下载，并明确标注，不把浏览器播放成功作为硬性要求。

### 8.3 安全文件访问

- session id、profile name、file kind 都必须是服务端解析的标识符，不接受任意相对/绝对路径。
- 服务端根据 manifest 和 allowlist 解析真实路径。
- canonicalize 后确认路径仍在配置的 `out_dir` 内。
- HTTP 文件响应支持 `Range`、正确的 `Content-Type` 和 `Content-Length`。
- “删除”实现为移动到 `out_dir/.trash/{session_id}/`，不直接永久删除。
- 移动前验证所有源/目标都位于同一个已确认的 `out_dir`；任一移动失败时返回错误并保留可恢复信息。

## 9. 文件级修改计划

### 9.1 `src/main.rs`

最终职责：

- 无参数启动服务。
- 解析持久化路径并加载 App 配置。
- 创建 EventBus 和 AudioController。
- 全进程只注册一次 Ctrl+C。
- 启动 Web Server，等待 shutdown。
- shutdown 时请求停止当前 session，等待 finalize 后再退出。

最终删除 Clap `Cli`、录音参数解析及对应旧 CLI 测试。

### 9.2 新增 `src/app_config.rs`

负责：

- `AppConfig`、`RecordingConfig`、默认值和版本校验。
- App config 与现有 `PipelineConfig` 的组合校验。
- TOML/JSON 序列化。
- `%LOCALAPPDATA%` 路径解析。
- 原子加载/保存、profile name 校验、import/export。
- 内置纯录音/DOA/DOA+BF/固定 BF preset。

### 9.3 新增 `src/controller.rs`

负责：

- `ControllerCommand`。
- `ServicePhase`、`ServiceSnapshot`、状态转换。
- 当前 `RecordingSessionHandle` ownership。
- Start/Stop/Shutdown 并发规则。
- session ready/finished 回调。
- Pipeline degraded 与 capture fatal 的区别。
- 向 EventBus 发布状态快照。

建议使用显式 `std::thread` + 有界控制通道，不把 recorder 改成 async。控制消息频率很低，容量固定并测试 Full 行为。

### 9.4 新增 `src/events.rs`

负责：

- `AppEvent`、`ServiceError` 及 JSON envelope。
- 非阻塞 EventPublisher。
- 最新 `ServiceSnapshot` 缓存。
- SSE broadcast 和 lag 后 resync 所需接口。

不要定义复杂 trait hierarchy。测试中提供轻量 no-op/test publisher 即可。

### 9.5 修改 `src/recorder.rs`

把一次性入口拆成可被 session thread 调用的服务入口：

```rust
pub struct RecordingRequest {
    pub recording: RecordingConfig,
    pub pipeline: Option<PipelineConfig>,
    pub session_id: String,
}

pub struct RecordingControl {
    pub stop: Arc<AtomicBool>,
    pub events: EventPublisher,
}

pub fn run_recording(
    request: RecordingRequest,
    control: RecordingControl,
) -> Result<RecordingSummary, String>;
```

具体要求：

- 删除 recorder 内的 `ctrlc::set_handler()`。
- 不再从 recorder 读取 Pipeline 配置路径。
- 使用已经校验和冻结的内存配置。
- 增加 session ready 事件；只有 raw writers、Pipeline 和 WASAPI 全部启动后才进入 Recording。
- 以低频率发布 progress，不能每个采样或每帧做 JSON。
- 保留当前 drain、raw finalize、Pipeline finish、WASAPI join 和首错优先级。
- 返回结构化 summary，供 manifest、Controller 和 UI 使用。
- Start 前执行 no-clobber 路径检查。

### 9.6 修改 `src/pipeline.rs`

- `PipelineConfig` 与模块配置增加 App 配置所需的序列化和 clone 能力。
- 提供明确的构造/访问接口，不让 Web 依赖私有 enum 的内部布局。
- `PipelineRuntime` 不再创建或关闭 `WebServerHandle`。
- 构造时接收 `EventPublisher`。
- DOA 结果仍先更新 `latest_doa`，再发布事件；BF 仍只消费 internal angle。
- `finalize()` 继续完成 DOA CSV、BF/DRC 和统计收尾，但不关闭全局 Web。

### 9.7 修改 `src/pipeline_worker.rs`

- 构造参数增加 EventPublisher/session id，但不改变 32 容量、`try_send` 和 256-hop assembler。
- worker stats 与首错通过结构化 session 事件回传。
- 继续在错误后先 drop receiver，使 recorder 立即看到 disconnected。
- 不把 Controller/Web 锁带进 worker 热路径。

### 9.8 重构 `src/web.rs`

从“DOA 专用 Viewer server”改成全局 App Web Server：

- server 在进程启动时创建，不依赖 DOA。
- 提供静态资源、REST API 和统一 SSE。
- handler 只校验请求、向 Controller 发送命令并返回响应。
- profile/recordings 文件 I/O 使用受控路径，必要时放入 blocking task，不阻塞 Tokio 事件循环。
- 保留 loopback 绑定、浏览器启动失败非致命、active SSE client 下可快速 shutdown。
- 删除 DOA 专用 `WebServerHandle` ownership 进入 Pipeline 的关系。

如果单文件明显过长，只允许按职责拆成：

```text
src/web.rs
src/web_api.rs
src/recordings.rs
```

不要为少量路由建立通用框架。

### 9.9 新增/替换前端文件

```text
web/index.html
web/styles.css
web/app.js
```

最终删除或并入 `web/doa_viewer.html`，不得维护两个不同 Viewer。

页面至少包含：

1. 顶部服务状态、设备状态、开始/停止按钮、录音计时。
2. 配置页：录音、DOA、BF、DRC 基础卡片与高级折叠区。
3. Live 页：DOA compass、raw/tracked angle、confidence、Tracking/Coasting/Searching、Pipeline degraded 提示、BF stats。
4. Recordings 页：session 列表、配置摘要、文件大小、播放/下载、移入回收区、恢复。
5. Profiles：加载、保存、导入、导出、恢复默认。

前端规则：

- 从 `/api/status` 获取初始状态，再连接 SSE。
- SSE 断开后指数退避重连并重新获取 snapshot。
- 所有表单在浏览器侧做基础校验，但后端校验才是最终真源。
- 录音中配置控件仍可编辑 draft，但清楚显示“当前录音不受影响”。
- Starting/Stopping 时禁用重复 Start；错误必须显示具体来源和消息。
- Viewer 继续保持 0° 朝下的显示变换；发送和保存的数值仍为 external angle。
- 不在页面保存或计算 BF internal steering angle。

### 9.10 `src/wasapi.rs`

- 保留采集实现和 capture overrun 语义。
- 将 ReSpeaker 探测能力以只读接口提供给 Controller，使 Web 在 Idle 时可刷新设备状态。
- 设备探测失败不能让 Web Server 退出；开始录音时仍必须重新确认设备并返回实际错误。
- 不增加 shared-mode fallback 或任意设备选择。

### 9.11 `src/wav.rs`、`src/doa/`、`src/beamformer/`、`src/drc/`

原则上不改变 DSP 与 WAV 数学。

允许的最小改动：

- 为 UI/status 暴露只读统计快照。
- 将现有打印转换为结构化事件，同时保留必要日志。
- 增加序列化用的只读 DTO。

禁止借本次重构调整算法默认值、滤波器、门限、阵列几何、权重或 DRC preset。

### 9.12 `Cargo.toml`

最终删除不再使用的 `clap`。

若实现安全的静态/录音文件 Range 响应需要，允许增加最小范围的 `tower-http` `fs` 功能和对应 Tokio `fs/io-util` 功能。不得增加前端构建依赖或完整 Web framework。

### 9.13 文档与配置

- `README.md` 改为无参数启动与 Web 操作说明。
- `AGENTS.md` 更新新的 main/controller/web/recorder ownership 和验证命令。
- 保留 `configs/*.toml` 作为 preset/import 示例和回归夹具，但删除“必须通过 `--pipeline-config` 使用”的公开表述。
- 文档明确：配置只在下次录音生效、固定硬件契约、Pipeline 降级语义、DRC 默认开启、真机测试边界。

## 10. 分阶段实施任务

### Task 1：锁定基线 contract

修改前补充或确认自动测试，覆盖：

- 纯录音不创建算法输出。
- Pipeline Full 后 raw 继续且最终返回错误。
- capture Full 仍停止录音。
- worker 首错在 finish 前可见且不重复。
- BF/DRC 输出长度、32-sample DRC flush、compare 独立状态。
- internal/external DOA 角边界。

验证：现有四件套全部通过。此任务不得改变生产行为。

### Task 2：配置与事件基础

实现 `app_config.rs`、`events.rs`：

- AppConfig 默认值、版本、TOML/JSON roundtrip。
- 现有三份 Pipeline TOML 可转换为对应 Web preset。
- 配置损坏回退、原子保存、profile name 拒绝路径穿越。
- 统一事件 envelope 和 snapshot cache。

窄测试：

- default config = 纯录音。
- `enable_drc` 省略后仍为 true。
- DOA+BF 错误顺序被拒绝。
- unknown field、NaN/Inf、非法目录/profile name 被拒绝。
- import/export roundtrip 不丢字段。

### Task 3：Recorder 可重复 session 化

在保持旧入口测试可用的中间状态下，先抽出：

- 外部 stop flag。
- in-memory PipelineConfig。
- session ready/progress/finished 回调。
- RecordingSummary。
- no-clobber 检查和 manifest。

然后把旧 `run_record()` 改成临时薄适配器；最终切换 main 后删除适配器。

窄测试：

- start failure 时已创建 raw 文件能 best-effort finalize。
- duration/stop 会 drain capture 并完成收尾。
- session config 在开始后不可被 draft 修改影响。
- 相同 prefix 不覆盖已有文件。
- manifest 成功、失败、interrupted 状态可区分。

### Task 4：AudioController 与状态机

实现有界命令通道和 session thread：

- Start/Stop/Shutdown。
- session Ready/Finished 内部消息。
- Idle/Starting/Recording/Stopping 转换。
- Pipeline degraded 状态。
- 重复录音和错误后恢复。

自动测试不启动声卡。使用纯状态转换测试或 `#[cfg(test)]` session runner 注入；不要为了测试生产代码引入庞大 trait。

必须覆盖：

- Idle Start 成功进入 Starting。
- Recording 时第二次 Start 返回 conflict。
- Stop 幂等且不重复 join。
- startup failure 回 Idle 并保留错误。
- pipeline failure 保持 Recording。
- capture fatal 进入 Stopping。
- 完成后可以开始第二次 session。
- Shutdown 等待当前 session finalize。

### Task 5：Web 与 Pipeline 解耦

- 将当前 DOA Web broadcaster 泛化为 App EventPublisher。
- 移除 `PipelineRuntime` 对 Web Server 生命周期的 ownership。
- DOA 事件、BF stats、Pipeline 首错进入统一 EventBus。
- 保证 Web 慢客户端不会影响 worker。

回归测试：

- Pipeline 无 DOA 时也可运行。
- DOA publisher 缺失/no-op 时算法结果不变。
- BF 继续使用 internal angle。
- publisher 失败或无订阅者不影响 DSP。

### Task 6：常驻 Web Server 与 REST/SSE

- 程序未录音时启动 Web。
- 实现第 7 节路由、错误 envelope、SSE snapshot/live/resync。
- Controller 命令只通过通道交互。
- active SSE client 下 shutdown 可及时返回。

HTTP 测试使用 `127.0.0.1:0`，不得依赖固定 8765 或声卡。

必须覆盖：

- 首页和静态资源。
- status/config/profile CRUD。
- Start/Stop 状态冲突。
- 新 SSE 客户端先获得 snapshot。
- 超过客户端上限返回 429。
- lag 后恢复最新状态。
- profile/session/file path traversal 被拒绝。

### Task 7：Web 前端

先完成功能正确性，再做视觉细化：

1. Dashboard 和 Start/Stop。
2. 配置表单和后端错误映射。
3. Live DOA/BF/Pipeline 状态。
4. Recordings、播放/下载、trash/restore。
5. profile/import/export。

用真实浏览器验证：

- 首次启动、无设备、配置损坏、端口占用。
- 1280×720、1920×1080 和窄窗口布局。
- SSE 断线重连。
- Starting/Stopping 按钮禁用。
- 0°/90°/180°/270° Viewer 显示方向。
- 错误和 Pipeline degraded 不被普通进度覆盖。

### Task 8：切换最终入口并移除 CLI

- `main.rs` 改为无参数 Web 服务入口。
- Ctrl+C 只注册一次。
- 删除 Clap 和旧 CLI tests/adapter。
- 程序启动不占用声卡；点击 Start 才占用。
- 浏览器打开失败不退出；Web bind 失败明确退出。

完成后搜索并清理仅由本次删除产生的未使用代码：

```powershell
rg -n "Cli|pipeline_config: Option<PathBuf>|--pipeline-config|--duration|--out-dir|--prefix" src README.md AGENTS.md
```

不得删除 `configs/*.toml` 的回归和导入价值。

### Task 9：录音目录与文档收尾

- manifest、legacy 分组、Range 文件响应、trash/restore 完成。
- README/AGENTS/configs 文档同步。
- Web 中提供版本、固定设备 contract 和输出格式说明。
- 明确浏览器对 4ch WAV 播放的限制。

### Task 10：提交前完整验证

执行：

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

再执行：

```powershell
git diff --check
git status --short
git diff --stat
```

检查所有新文件已被纳入 diff，且 `target/`、录音、用户 profile、manifest、截图和临时配置没有进入 Git。

## 11. 真机验收矩阵

自动测试通过后，在真实 ReSpeaker 上逐项执行并分别记录结果：

| 场景 | 预期 |
|---|---|
| 启动程序但不录音 | Web 可用，声卡未被独占 |
| 无设备启动 | Web 可用，设备显示未连接，Start 给出明确错误 |
| 纯录音 | 只创建三路 raw WAV + manifest |
| DOA | raw + CSV；Web 实时显示 DOA |
| DOA + robust BF + DRC | raw + CSV + stereo BF，BF 使用 internal DOA |
| fixed Delay-and-Sum + DRC | 无 DOA/CSV，fixed BF 正常 |
| 录音中关闭浏览器 | 录音继续；重开页面恢复状态 |
| Web Stop | drain、WAV header、CSV、BF/DRC tail 均完整 |
| Ctrl+C | active session graceful finalize 后进程退出 |
| 连续录两次 | 第二次无需重启，无 Ctrl+C 注册或线程残留错误 |
| Pipeline 人工过载/故障 | UI 显示 degraded，raw 继续，最终 session 标记错误 |
| capture overrun | 本次 session 停止并标记 capture fatal |
| ReSpeaker 中途断开 | UI 显示设备/采集错误，服务回 Idle，可重新开始 |
| 重复 prefix | Start 返回冲突，不覆盖旧文件 |
| DRC 开启 | 默认开启、tail flush 保留、compare 两声道状态独立 |

真机验收还应检查：

- 三路 raw 通道内容没有交换。
- BF WAV 左=mic1、右=BF 的 compare contract 保持。
- Viewer external 角方向正确，BF internal steering 未被 UI 旋转污染。
- 输出目录长期录音下文件大小、浏览器下载和 Range 播放正常。
- 退出后没有遗留占用 ReSpeaker 的线程或进程。

## 12. 完成标准

只有同时满足以下条件才可声明完成：

1. 无参数启动后 Web 界面始终可用，未点击 Start 时不占用声卡。
2. 同一进程可完成至少两次 Start/Stop，全部资源正确释放。
3. Web 可覆盖当前纯录音、DOA、DOA+BF、fixed BF、DRC 主要配置。
4. 配置保存、profile、导入/导出和配置损坏恢复均有测试。
5. 当前录音使用冻结快照，编辑 draft 不影响运行中 DSP。
6. capture fatal 与 Pipeline degraded 语义和 UI 展示准确。
7. raw WAV、CSV、BF/DRC、角度和队列 contract 未回退。
8. 历史录音可安全列出、下载/播放并移入可恢复回收区。
9. 所有自动验证通过，且没有声卡依赖测试。
10. 真机验收矩阵已执行；未执行的项目必须明确写为未验证，不能用自动测试代替。
11. README、AGENTS、configs 与最终行为一致，不再把 CLI/配置路径描述为主要操作方式。

## 13. 后续可选阶段

不属于本文完成条件，后续另行设计：

- 在 256-frame hop 边界热更新 fixed BF angle、DOA offset/threshold。
- 后台重建 BF LUT 后原子切换 WNG/频带配置。
- 可配置 DRC preset 和完整 DRC 参数，并定义状态保留/重置策略。
- 浏览器实时音频监听、Opus/WebSocket/WebRTC。
- Session 子目录迁移与旧平铺输出兼容。
- 局域网远程访问、认证与 TLS。
- 桌面托盘、开机启动或 WebView 原生外壳。
