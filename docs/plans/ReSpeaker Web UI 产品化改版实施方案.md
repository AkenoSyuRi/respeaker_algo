# ReSpeaker Web UI 产品化改版实施方案

## 0. Agent 执行要求

目标仓库：

```text
AkenoSyuRi/respeaker_algo
```

目标分支：

```text
master
```

开始修改前必须重新读取当前版本：

```text
AGENTS.md

web/index.html
web/styles.css
web/app.js

src/web.rs
src/controller.rs
src/app_config.rs
src/pipeline.rs
src/events.rs

README.md
configs/doa.toml
configs/doa_bf.toml
configs/bf_fixed.toml
```

以当前仓库代码为事实来源。如果本文中的文件结构、字段或 API 与最新代码产生冲突，以最新代码为准，但应保持本文定义的产品目标和信息架构。

本任务是 **Web UI 产品化重构**。

不要借此任务：

- 重构 DSP。
- 修改 DOA / Beamformer 算法。
- 修改实时线程模型。
- 修改录音数据流。
- 修改 ch0 / ch1..4 / ch5 contract。
- 引入 React/Vue/Svelte 等前端框架。
- 引入 npm、bundler 或新的构建流程。
- 为了“代码漂亮”重构 Controller / Pipeline。
- 改变 AppConfig schema，除非现有实现确实无法完成需求。
- 改变 REST/SSE contract，除非有明确且不可避免的理由。

原则上本次主要修改：

```text
web/index.html
web/styles.css
web/app.js
```

必要时同步修改：

```text
src/web.rs              # 静态页面相关测试
README.md               # 页面操作说明
```

后端现有能力足够支撑第一版 UI。

---

# 1. 产品目标

当前页面功能较完整，但 UI 基本按照内部程序结构直接展开：

```text
Recording
Pipeline
DOA
Beamformer
Profiles
Live metrics
Recordings
Config JSON
Events
Device contract
```

这对于研发调试方便，但对普通客户来说信息量过大。

本次改版目标是：

> 默认页面只让用户看到“我现在要做什么”和“系统是否正常”；算法细节通过分层设置逐步暴露。

形成三个层级：

```text
任务级
    ↓
用户级设置
    ↓
专家 / 调试参数
```

不要删除已有高级配置能力，而是改变它们在 UI 中的位置和优先级。

最终正常客户的核心操作应该接近：

```text
选择工作模式
    ↓
确认保存位置 / 时长
    ↓
开始录音
```

不应该要求客户理解：

```text
pipeline_enabled
acquire_confidence
update_confidence
direction_smoothing_ms
min_wng_db
sd_low_start_hz
```

才能开始使用。

---

# 2. 总体信息架构

页面改为四个一级 Tab：

```text
运行
算法设置
录音记录
高级设置
```

默认进入：

```text
运行
```

Header 保持全局可见。

推荐整体结构：

```text
┌──────────────────────────────────────────────────────────────┐
│ ReSpeaker Audio Manager              ● ReSpeaker 已连接      │
│                                      ● 服务实时连接           │
├──────────────────────────────────────────────────────────────┤
│ [运行]      [算法设置]      [录音记录]      [高级设置]       │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│                    当前 Tab 内容                              │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

一级 Tab 不要超过这四个。

高级配置通过：

```text
Tab
折叠区域
Dialog / Drawer
```

进一步暴露。

---

# 3. Tab 1：运行

这是默认页面，也是客户最主要的页面。

运行页面只回答几个问题：

```text
设备正常吗？
当前是什么模式？
录音了吗？
录了多久？
声源方向如何？
有没有异常？
```

## 3.1 顶部状态

显示：

```text
ReSpeaker Mic Array v2.0
● 已连接
```

如果设备不可用：

```text
● 未检测到 ReSpeaker
```

设备不可用时：

- 明确显示错误原因。
- 开始录音按钮 disabled。
- 不需要让客户看到完整 WASAPI 调试信息。

连接到 Web SSE 的状态独立显示：

```text
● 实时连接
```

不要把：

```text
DEVICE
SESSION STATE
CAPTURED FRAMES
```

做成三个同等重要的大卡片。

`captured_frames` 从客户首页移除。

---

# 4. 工作模式作为首页最重要入口

当前四个 built-in preset：

```text
pure
doa
doa_bf
bf_fixed
```

产品层重新命名为：

| preset | 客户名称 | 说明 |
|---|---|---|
| pure | 原始录音 | 录制 ReSpeaker 原始多通道音频 |
| doa | 声源定位 | 实时检测并显示说话方向 |
| doa_bf | 自动拾音 | 自动跟随声源方向并增强目标声音 |
| bf_fixed | 定向拾音 | 固定增强指定方向的声音 |

在运行页面以 4 张 Mode Card 展示：

```text
当前模式

┌──────────────┐
│ 原始录音     │
│ 多通道采集   │
└──────────────┘

┌──────────────┐
│ 声源定位     │
│ 实时 DOA     │
└──────────────┘

┌──────────────┐
│ 自动拾音     │
│ DOA + BF     │
└──────────────┘

┌──────────────┐
│ 定向拾音     │
│ Fixed BF     │
└──────────────┘
```

当前模式使用明显 selected 状态。

不要在普通页面暴露：

```text
pipeline_enabled
doa.enabled
beamformer.enabled
```

工作模式负责这些内部关系。

---

# 5. 工作模式识别

增加：

```javascript
detectMode(config)
```

根据当前 AppConfig 判断：

### pure

```text
pipeline_enabled == false
```

### doa

```text
pipeline_enabled == true
DOA enabled
BF not enabled
```

### doa_bf

```text
pipeline_enabled == true
DOA enabled
BF enabled
BF.direction_source == "doa"
```

### bf_fixed

```text
pipeline_enabled == true
BF enabled
BF.direction_source == "fixed"
DOA not enabled
```

其它合法组合：

```text
custom
```

Custom 时：

```text
当前模式：自定义
```

四张内置 Mode Card 均不强行选中。

不要为了匹配 UI 而修改用户自定义 Pipeline。

---

# 6. 切换 Built-in Mode 时的重要规则

切换：

```text
原始录音
声源定位
自动拾音
定向拾音
```

可以继续利用现有 built-in preset：

```text
pure
doa
doa_bf
bf_fixed
```

但有一项重要产品要求：

> 切换算法模式不能顺便把用户的录音目录、录音时长和文件名前缀恢复成默认值。

当前 built-in preset 是完整 AppConfig，因此直接 load preset 会覆盖：

```text
recording.duration_seconds
recording.out_dir
recording.prefix
```

前端切换 mode 时必须保存当前 recording 配置。

建议流程：

```text
保存当前 recording settings
        ↓
load built-in preset
        ↓
将之前 recording settings 合并回 preset
        ↓
PUT /api/config
        ↓
renderConfig()
```

因此：

```text
切换算法模式 ≠ 重置录音设置
```

用户主动加载自定义 Profile 时则允许恢复 Profile 中完整的 recording 配置。

---

# 7. 运行页面录音设置

首页只保留三个录音相关配置。

## 保存位置

```text
保存位置

recordings/                         [修改]
```

对应：

```text
recording.out_dir
```

---

## 录音时长

不要直接只展示：

```text
duration_seconds = 0
```

UI 使用：

```text
录音时长

● 手动停止
○ 自定义
```

选择自定义后再显示：

```text
[ 300 ] 秒
```

内部继续：

```text
0 = 手动停止
```

无需修改 AppConfig。

---

## 文件名前缀

默认不占据首页主要空间。

可以放在：

```text
更多录音设置
```

折叠区。

文案：

```text
文件名前缀
留空时自动生成
```

对应：

```text
recording.prefix
```

---

# 8. 开始 / 停止录音

首页提供一个视觉最明显的主操作区。

Idle：

```text
[ 开始录音 ]
```

Starting：

```text
正在启动…
```

Recording：

```text
● 录音中  03:21

[ 停止录音 ]
```

Stopping：

```text
正在停止并保存文件…
```

使用现有：

```text
/api/recordings/start
/api/recordings/stop
```

不要改变 Controller session 状态机。

---

# 9. 开始录音前必须同步当前 UI 配置

当前实现存在：

```text
UI input
↓
Apply draft
↓
Start
```

新的客户界面不要要求用户理解“Apply draft”。

实现：

```javascript
startRecording()
```

时首先：

```text
collectConfigFromUi()
        ↓
PUT /api/config
        ↓
成功
        ↓
POST /api/recordings/start
```

因此用户编辑配置后直接点：

```text
开始录音
```

当前屏幕上的配置就必须成为本次 session 的配置。

不要因为用户忘记点击“应用 draft”而使用旧配置。

---

# 10. 配置持久化

第一版不要做每一个 input change 都自动请求后端。

保持清晰、低风险的模型：

```text
页面编辑
    ↓
dirty state
```

提供：

```text
保存设置
```

其行为为：

```text
PUT /api/config
    ↓
POST /api/config/save
```

开始录音时：

```text
PUT /api/config
    ↓
POST /api/recordings/start
```

是否同时持久化到磁盘，不必强制。

这样可以隐藏：

```text
应用 draft
保存配置
```

两个内部概念。

产品 UI 只出现：

```text
保存设置
```

高级页面仍可解释配置文件持久化。

---

# 11. 录音期间配置语义

Controller 在 Start 时 clone 当前 draft。

因此必须保持：

```text
本次 session 使用启动瞬间的配置 snapshot
```

录音期间如果允许编辑配置，应显示提示：

```text
当前录音使用启动时的配置。
本页修改将在下一次录音时生效。
```

绝对不要让 UI 暗示修改参数会实时改变当前 Pipeline。

---

# 12. 运行页面实时结果

实时区域根据当前模式动态变化。

---

## 原始录音

不显示 DOA 罗盘。

仅显示：

```text
录音中
03:21
保存至 recordings/
```

---

## 声源定位

显示 DOA 罗盘。

默认只显示：

```text
126°

● 方向稳定
```

状态映射：

```text
tracking  → 方向稳定
searching → 正在搜索声源
coasting  → 暂时保持上一方向
```

不要默认展示：

```text
Raw
Tracked
Confidence
Peak
MSC
```

---

## 自动拾音

显示：

```text
DOA 罗盘
当前方向
BF 工作状态
```

正常情况下只显示：

```text
● 自动拾音正常
```

如果：

```text
pipeline.degraded == true
```

显示：

```text
⚠ 算法处理已降级
<error>
```

---

## 定向拾音

不需要显示动态 DOA。

显示固定方向罗盘：

```text
当前拾音方向

        ↑
    ↖       ↗
←       ●       →
    ↙       ↘
        ↓

       90°
```

角度来自：

```text
fixed_internal_angle_deg
```

---

# 13. 实时详细信息

运行页提供：

```text
查看详细信息
```

点击打开 `<dialog>`。

这里可以显示工程指标：

```text
Tracked angle
Raw angle
Confidence
DOA status
Captured frames
BF output frames
Clipped samples
Pipeline state
```

即：

> 工程信息保留，但不占据默认主界面。

如果：

```text
clipped_samples == 0
```

普通页面不要显示。

如果：

```text
clipped_samples > 0
```

首页才出现警告：

```text
⚠ 检测到 Beamformer 输出削波
```

---

# 14. Tab 2：算法设置

算法设置面向：

```text
高级客户
现场调试人员
算法工程师
```

但依然采用 progressive disclosure。

页面分成：

```text
DOA
Beamformer
```

两个 Card。

模块没有启用时：

```text
当前工作模式未使用此模块
```

不要默认提供 `enabled` checkbox。

模块启用关系主要由工作模式决定。

---

# 15. DOA 基础设置

默认显示：

```text
方向校准
角度方向
```

### 方向校准

对应：

```text
angle_offset_deg
```

UI：

```text
方向校准

[ 0.0 ] °
```

说明：

```text
用于补偿设备实际安装方向。
```

---

### 角度方向

对应：

```text
clockwise
```

不要直接显示 bool。

UI：

```text
角度增加方向

● 逆时针
○ 顺时针
```

---

# 16. DOA 专家参数

点击：

```text
专家参数
```

展开：

```text
beta
cpsd_tau_ms
acquire_confidence
update_confidence
max_coast_ms
csv
```

推荐中文 label：

```text
PHAT β
CPSD 平滑时间
目标捕获阈值
目标更新阈值
丢失保持时间
保存 DOA CSV
```

可在 label 下保留小字字段名，例如：

```text
目标捕获阈值
acquire_confidence
```

便于研发定位。

HTML 限制：

```text
beta                min=0 max=1
cpsd_tau_ms         min > 0
acquire_confidence  min=0 max=1
update_confidence   min=0 max=1
max_coast_ms        min=1
```

同时仍必须依赖后端 validate 作为最终校验。

---

# 17. Beamformer 基础设置

默认显示：

```text
跟随方式
拾音方向（条件显示）
输出增益
动态范围处理
```

### 跟随方式

对应：

```text
direction_source
```

UI：

```text
跟随方式

● 自动跟随声源
○ 固定方向
```

内部：

```text
auto  → doa
fixed → fixed
```

---

## 条件显示

如果：

```text
direction_source == doa
```

显示：

```text
找不到声源时的默认方向
```

对应：

```text
fallback_internal_angle_deg
```

不要显示：

```text
fixed_internal_angle_deg
```

如果：

```text
direction_source == fixed
```

显示：

```text
固定拾音方向
```

对应：

```text
fixed_internal_angle_deg
```

不要显示 fallback。

---

# 18. 固定角度选择器

不要只给客户：

```text
[ 90.0 ]
```

实现一个简单的圆形方向选择器。

要求：

- CSS + JS 实现。
- 不引入图形库。
- 点击圆周能够选择角度。
- 同时保留 number input 方便精确输入。
- UI 与当前内部角度约定一致。
- 不得把 Viewer display transform 错误用于 Beamformer steering。

注意：

```text
fixed_internal_angle_deg
```

属于算法内部角。

不要直接拿 Viewer 视觉的：

```text
0° 朝下
```

逻辑写回 Beamformer。

必须明确区分：

```text
internal angle
external/output angle
viewer display angle
```

---

# 19. Beamformer 输出设置

普通设置：

```text
输出增益
```

对应：

```text
output_gain_db
```

限制：

```text
-24 dB ～ +24 dB
```

DRC：

```text
输出动态范围处理
[开关]
```

对应：

```text
enable_drc
```

---

# 20. Beamformer 专家参数

折叠在：

```text
专家参数
```

内部：

```text
algorithm
direction_smoothing_ms
min_wng_db

sd_low_start_hz
sd_low_full_hz
sd_high_full_hz
sd_high_end_hz

wav
compare_wav
```

算法名称产品化：

```text
robust_superdirective
    → 鲁棒超指向 Beamformer

delay_sum
    → Delay-and-Sum
```

频带参数放在独立：

```text
超指向频带
```

小分组中。

约束：

```text
0 <= low_start
low_start <= low_full
low_full <= high_full
high_full <= high_end
high_end <= 8000
```

`min_wng_db`：

```text
0 ～ 6 dB
```

`direction_smoothing_ms`：

```text
>= 0
```

---

# 21. 条件化 UI 是本次改版的硬要求

不要只是把原来的配置表拆成几个 Tab。

必须真正减少当前可见字段。

例如：

```text
BF off
```

不显示 BF 参数。

```text
direction_source = doa
```

不显示 fixed angle。

```text
direction_source = fixed
```

不显示 fallback angle。

```text
pure recording
```

运行页不显示 DOA / BF 实时面板。

```text
DOA disabled
```

算法设置中的 DOA Card 显示：

当前模式未使用 DOA

而不是铺满一排灰色 input。

---

# 22. Tab 3：录音记录

将当前 Archive 从主页移到独立 Tab。

每个录音建议展示：

```text
2026-09-24 22:31

自动拾音
03:42

[播放器]

处理音频
Beamformer
原始 4-Mic
AEC Reference
DOA CSV

[下载…]    [更多…]
```

实际信息必须根据现有 recording manifest 提供的字段生成。

不要假定 backend 不存在的数据。

---

# 23. 文件名称产品化

当前：

```text
algo
mic
ref
bf
doa_csv
manifest
```

UI 显示名称改为：

```text
algo      → 设备处理音频
mic       → 原始 4-Mic
ref       → Playback / AEC Reference
bf        → Beamformer 输出
doa_csv   → DOA 数据
manifest  → 录音信息
```

内部 API kind 保持不变。

---

# 24. 录音试听

继续只给浏览器可以合理播放的文件提供 audio player。

现有逻辑：

```text
algo
ref
bf
```

可以保持。

4ch MIC：

```text
mic
```

不强行用浏览器 audio player。

提供：

```text
下载
```

即可。

---

# 25. 回收站

录音 Tab 内分：

```text
录音
```

以及折叠区域：

```text
回收站
```

避免正常列表同时混入 deleted 项目。

继续使用：

```text
/api/recordings/:id/trash
/api/recordings/:id/restore
```

不要实现真正永久删除，除非当前后端已经提供。

---

# 26. Tab 4：高级设置

高级设置分为：

```text
配置方案
配置文件
设备信息
开发者工具
```

---

# 27. 配置方案

UI 统一使用名称：

```text
配置方案
```

不要在主产品界面同时出现：

```text
Preset
Profile
Draft
Active snapshot
```

用户看到：

```text
系统方案

原始录音
声源定位
自动拾音
定向拾音
```

以及：

```text
我的方案

会议室
实验室
远场测试
...
```

底层仍然保持：

```text
built-in presets
profiles
```

---

# 28. 用户 Profile

保留：

```text
载入
保存当前配置
删除
```

保存 Profile 时：

不要直接读取可能过期的：

```text
config-editor
```

必须：

```text
collectConfigFromUi()
```

生成当前实际配置，再保存。

这样用户刚刚在可视化表单修改的设置不会丢失。

---

# 29. 配置文件管理

以下功能移入高级设置：

```text
恢复默认
导入 TOML
导出 TOML
```

恢复默认必须增加确认 Dialog：

```text
恢复默认设置？

当前未保存修改将丢失。

[取消] [恢复默认]
```

---

# 30. 原始 AppConfig

保留 JSON 编辑能力，但移动至：

```text
高级设置
    ↓
开发者工具
        ↓
原始 AppConfig
```

默认折叠。

明确标识：

```text
开发者功能
```

提供：

```text
应用原始配置
```

不要让 JSON textarea 成为普通配置页面的主数据入口。

---

# 31. 配置状态的前端数据模型

重新整理 `app.js`。

建议 state：

```javascript
const state = {
  config: null,
  snapshot: null,
  latestDoa: null,
  latestBf: null,

  dirty: false,
  activeTab: "run",

  source: null,
  log: [],
  reconnectTimer: null,
  retryDelay: 500,
};
```

其中：

```text
state.config
```

是当前前端 AppConfig 的唯一主要 source of truth。

JSON editor 不是 source of truth。

---

# 32. app.js 建议拆分职责

保持单文件，不需要引入模块系统。

至少整理出以下函数：

```javascript
request()

detectMode(config)

getModules(config)
getModule(config, type)

renderConfig(config)
renderMode(config)
renderSnapshot(snapshot)
renderDoa(data)
renderBeamformer(data)
renderLivePanel()

collectConfigFromUi()

setDirty(dirty)
commitConfig({ persist })

selectBuiltInMode(name)

switchTab(name)
updateConditionalUi()

loadConfig()
loadProfiles()
loadRecordings()

copyDiagnostics()

connectEvents()
```

不要继续把全部逻辑堆在一个 `applyConfig()` 里。

---

# 33. collectConfigFromUi() 的重要规则

不要从零创建 AppConfig。

必须：

```javascript
structuredClone(state.config)
```

或兼容方式 clone 当前 config。

然后只覆盖 UI 管理的字段。

原因：

未来 AppConfig 增加新字段时，旧前端不能因为不知道该字段而删除它。

原则：

> UI 修改自己负责的字段，未知字段原样保留。

Pipeline module 也同样处理。

不要无意义重建整个：

```text
pipeline.modules
```

---

# 34. Pipeline module 顺序

必须继续保持：

```text
DOA
 ↓
Beamformer(direction_source = doa)
```

如果 BF 使用 DOA：

```text
DOA module 必须位于 BF 前面
```

现有 `applyConfig()` 已有 reorder 逻辑。

重构后必须保留该行为。

不要因为 UI 重构破坏 Pipeline validation。

---

# 35. 高级参数完整性

当前简单表单并没有展示所有 BF 配置。

新版专家设置应尽量覆盖当前 AppConfig 中可配置的重要参数：

DOA：

```text
enabled              # 不作为普通 UI toggle
enable_viewer
csv
beta
cpsd_tau_ms
angle_offset_deg
clockwise
acquire_confidence
update_confidence
max_coast_ms
```

Beamformer：

```text
enabled              # 不作为普通 UI toggle
algorithm
direction_source
fixed_internal_angle_deg
fallback_internal_angle_deg
direction_smoothing_ms
min_wng_db
sd_low_start_hz
sd_low_full_hz
sd_high_full_hz
sd_high_end_hz
output_gain_db
wav
compare_wav
enable_drc
```

如果有极少数不适合普通表单的字段，可以只留在 raw AppConfig 中，但不要因为重构导致字段丢失。

---

# 36. Device Contract 的处理

当前页面底部大块：

```text
FIXED CONTRACT
设备与输出说明
```

从主页面移除。

Header 的设备状态旁加入：

```text
ⓘ
```

点击打开设备信息 Dialog。

内容：

```text
ReSpeaker Mic Array v2.0
XMOS XVF-3000

WASAPI Exclusive
16 kHz
PCM16
6 channels

ch0      固件算法输出
ch1-ch4  原始 MIC
ch5      Playback / AEC Reference
```

显示 App Version。

不要改变实际设备 contract。

---

# 37. 实时 Event Log

从首页彻底移除。

移动：

```text
高级设置
    ↓
开发者工具
        ↓
实时事件日志
```

继续维护最多约 80 条记录即可。

保留：

```text
清空
```

---

# 38. 增加“复制诊断信息”

高级设置增加：

```text
复制诊断信息
```

纯前端实现。

内容建议：

```text
ReSpeaker Audio Manager
Version: ...

Web connection: connected
Device: ...
Device available: true/false

Phase: ...
Mode: ...

Pipeline enabled: ...
Pipeline degraded: ...
Pipeline error: ...

Last error source: ...
Last error code: ...
Last error message: ...

Config warning: ...
```

不要默认复制完整 AppConfig。

不要复制音频数据。

使用：

```javascript
navigator.clipboard.writeText(...)
```

失败时给出用户可理解的提示。

---

# 39. 错误与异常展示

正常状态尽量安静。

异常状态主动出现。

优先级：

```text
设备不可用
    ↓
录音错误
    ↓
Pipeline degraded
    ↓
配置 warning
    ↓
BF clipping
```

运行页面顶部使用统一：

```text
status / warning banner
```

不要让错误只存在于 Event Log。

---

# 40. Tab 实现

不引入框架。

使用：

```html
<nav role="tablist">
```

以及：

```html
<section role="tabpanel">
```

未激活 Tab：

```text
hidden
```

按钮维护：

```text
aria-selected
```

Tab：

```text
run
algorithm
recordings
advanced
```

默认：

```text
run
```

可以使用：

```text
sessionStorage
```

记录本次浏览器 session 最后 Tab，但不是硬要求。

---

# 41. Dialog 实现

优先使用浏览器原生：

```html
<dialog>
```

用于：

```text
实时详细信息
设备信息
恢复默认确认
```

不要为此引入第三方 modal library。

确保：

```text
ESC 可关闭
关闭按钮可点击
```

---

# 42. CSS 设计方向

可以延续目前深色主题。

当前：

```text
#0e1110
#151918
#f3b55f
```

视觉风格无需推翻。

主要解决：

```text
信息层级
密度
组件状态
响应式布局
```

新增组件：

```text
.tabs
.tab-button
.tab-panel

.mode-grid
.mode-card
.mode-card.selected

.status-banner

.quick-settings

.setting-card
.setting-row
.setting-description

.expert-section

.live-summary
.live-status

.dialog

.direction-picker

.recording-list
.recording-card

.diagnostic-grid
```

避免：

```text
所有内容都有厚边框
所有值都是大卡片
所有设置同时展开
```

---

# 43. 首页桌面布局

目标屏幕宽度：

```text
1180 px 左右
```

运行页在普通桌面浏览器中尽量让：

```text
设备状态
模式选择
录音设置
开始按钮
```

无需滚动即可看到。

推荐：

```text
Mode cards      一行 4 个

Recording       左侧
Live state      右侧
```

录音开始后，实时状态获得更高视觉优先级。

---

# 44. 响应式

继续支持：

```text
<= 760 px
```

至少做到：

```text
Tab 可横向滚动或合理折行
Mode Card 2 列 / 1 列
Algorithm setting 单列
Live view 单列
Dialog 不超出 viewport
```

项目主要是 Windows Desktop，但不能因为这个理由让小窗口直接坏掉。

---

# 45. 不要修改的后端 API

第一版默认保留：

```text
GET  /api/status

GET  /api/config
PUT  /api/config

POST /api/config/save
POST /api/config/reset
POST /api/config/import
GET  /api/config/export

GET  /api/profiles
GET  /api/profiles/:name
PUT  /api/profiles/:name
DELETE /api/profiles/:name

POST /api/recordings/start
POST /api/recordings/stop

GET  /api/recordings
GET  /api/recordings/:id
GET  /api/recordings/:id/files/:kind
POST /api/recordings/:id/trash
POST /api/recordings/:id/restore

GET /api/events
```

如果现有 API 能完成，不新增 endpoint。

---

# 46. SSE 行为

继续监听：

```text
state_snapshot
device_status
recording_progress
pipeline_status
doa
bf_stats
error
session_finished
```

但改变展示层。

例如：

```text
recording_progress
```

首页更新：

```text
elapsed time
```

而不是重点显示：

```text
captured frames
```

`captured_frames` 仍保留给详细状态。

---

# 47. DOA Viewer 坐标

这是硬性 correctness requirement。

当前 Viewer：

```text
0°
```

视觉朝页面下方。

这属于：

```text
display transform
```

不要修改底层：

```text
DOA internal coordinate
angle_offset_deg
clockwise
Beamformer steering coordinate
```

UI 重构不得顺手“修正”坐标系统。

---

# 48. 固定 BF Direction Picker 坐标

固定 BF 配置使用：

```text
fixed_internal_angle_deg
```

它不是 Viewer 的 external angle。

实现可视化方向 Picker 时必须明确转换。

如果为了视觉一致希望 0° 朝下：

```text
只做显示 transform
```

写回配置时必须转换回：

```text
internal angle
```

不得把显示角直接写入 BF。

为该转换增加独立 JS helper，并加注释。

---

# 49. 页面文案

首页尽量使用客户语言。

避免：

```text
SESSION STATE
DRAFT CONFIG
PIPELINE
CAPTURED FRAMES
BF output
Clipped
Acquire confidence
```

改为：

```text
录音状态
工作模式
已录制时间
声源方向
算法状态
输出削波
```

开发者页面可以保留英文 field name。

---

# 50. 推荐页面最终结构

```text
Header
├── Product name
├── Device status
└── Web connection

Tabs
├── 运行
│   ├── Warning banner
│   ├── Mode selector
│   ├── Recording quick settings
│   ├── Start / Stop
│   └── Live result
│
├── 算法设置
│   ├── DOA
│   │   ├── Basic
│   │   └── Expert
│   └── Beamformer
│       ├── Basic
│       └── Expert
│
├── 录音记录
│   ├── Active recordings/history
│   └── Trash
│
└── 高级设置
    ├── Profiles
    ├── Import / Export / Reset
    ├── Device info
    └── Developer tools
        ├── Raw AppConfig
        ├── Diagnostics
        └── Event log
```

---

# 51. 第一阶段不实施的内容

不要在本任务增加：

```text
实时浏览器 PCM 播放
WebSocket audio
账户
局域网访问
用户认证
远程管理
Pipeline drag & drop
任意 Pipeline graph editor
实时修改正在运行的 DSP 参数
永久删除录音
云上传
```

这些都属于其它需求。

---

# 52. 实施顺序

按以下顺序完成，不要同时推倒全部逻辑。

## Step 1

重构 `index.html`：

```text
Header
Tabs
4 个 tab-panel
Mode cards
Run UI
Algorithm UI
Advanced dialogs
```

此阶段保持必要旧 id 或同步规划 JS 修改。

---

## Step 2

重构 `styles.css`：

```text
Tab
Mode card
Setting card
Dialog
Run layout
Recording list
Conditional visibility
Responsive
```

---

## Step 3

整理 `app.js` state 和 render。

先实现：

```text
state.config
state.snapshot
detectMode
renderConfig
renderSnapshot
switchTab
```

---

## Step 4

实现：

```text
collectConfigFromUi
dirty state
commitConfig
startRecording
```

保证：

```text
修改配置 → 直接开始
```

不会使用旧 draft。

---

## Step 5

实现 Mode selector。

特别测试：

```text
切换 mode 后 recording.out_dir 不变
切换 mode 后 duration 不变
切换 mode 后 prefix 不变
```

---

## Step 6

实现算法设置以及 conditional UI。

---

## Step 7

迁移 recordings UI。

---

## Step 8

迁移 advanced：

```text
Profile
Import
Export
Reset
JSON
Event log
Device info
Diagnostics
```

---

## Step 9

更新 SSE render。

---

## Step 10

更新测试与 README。

---

# 53. 自动验证

完成代码后必须运行：

```text
cargo fmt --all -- --check

cargo clippy --all-targets --all-features -- -D warnings

cargo test --all-targets

cargo build --release
```

不得仅执行 `cargo build`。

---

# 54. Web smoke test

没有 ReSpeaker 硬件也要至少验证：

```text
Web 服务正常启动
页面资源加载成功
/api/status 正常
/api/config 正常
Tab 切换正常
Dialog 正常
Profile 列表正常
录音历史可以加载
SSE 可以连接
```

---

# 55. ReSpeaker 真机验收

如果当前执行环境有硬件，则分别验证：

## 原始录音

```text
选择 原始录音
开始
停止
生成 algo / mic / ref WAV
```

## 声源定位

```text
选择 声源定位
开始
DOA 罗盘实时变化
停止
DOA CSV 正常
```

## 自动拾音

```text
选择 自动拾音
DOA 正常
BF 正常
BF WAV 正常
```

## 定向拾音

```text
选择 定向拾音
选择固定方向
开始
BF 使用正确 internal angle
```

硬件不可用时，不得声称完成硬件验证。

---

# 56. 配置回归测试

重点检查：

```text
pure
doa
doa_bf
bf_fixed
```

全部可以正常 load。

检查：

```text
DOA before BF
```

顺序不会被 UI 破坏。

检查：

```text
update_confidence <= acquire_confidence
```

非法设置由 UI 尽量预防，同时 backend validation 仍然生效。

检查 BF：

```text
min_wng_db
frequency band
output_gain_db
```

非法值应该显示后端返回的用户可理解错误。

---

# 57. Profile 回归

验证：

```text
保存用户 Profile
关闭/刷新页面
重新载入 Profile
所有配置保持
删除 Profile
```

尤其检查新增专家字段不会因为前端 collectConfig 而丢失。

---

# 58. Import / Export 回归

验证：

```text
Export TOML
Import 同一个 TOML
Config 等价
```

Import 后：

```text
Mode selector
Algorithm UI
Raw JSON
```

必须全部同步刷新。

---

# 59. Recording UI 回归

验证：

```text
录音列表
播放器
下载
trash
restore
```

功能与旧页面一致。

UI 重构不能导致历史录音访问能力下降。

---

# 60. Web 静态测试

检查 `src/web.rs` 当前针对：

```text
index.html
styles.css
app.js
```

存在的字符串 contract。

如果本次重构改变对应字符串，应更新测试。

不要删除 Web server 行为测试。

---

# 61. 最终验收标准

全部满足才算任务完成。

### 用户体验

1. 默认进入“运行”。
2. 首页看不到 `pipeline_enabled`。
3. 首页看不到 acquire/update confidence。
4. 首页看不到 raw JSON。
5. 首页看不到 Event Log。
6. 首页可以直接选择四种工作模式。
7. 普通用户最多经过“选择模式 → 开始录音”即可开始工作。
8. 设备异常在首页明确显示。
9. Pipeline degraded 在首页明确显示。
10. 正常情况下页面保持简洁。

### 配置能力

11. 原来的主要配置能力不能丢失。
12. DOA 专家参数可以访问。
13. BF 专家参数可以访问。
14. Raw AppConfig 仍可以访问。
15. TOML import/export 仍然可用。
16. 用户 Profile 仍然可用。

### 行为

17. 切换内置工作模式不会重置录音目录、时长、prefix。
18. 点击开始前自动同步当前 UI 配置到 draft。
19. 当前 session 配置 snapshot 语义不变。
20. Pipeline module 顺序不被破坏。
21. Viewer 坐标转换不影响内部算法坐标。
22. Fixed BF 写回的是 internal angle。

### 架构

23. 不引入前端框架。
24. 不引入 npm/build pipeline。
25. 不修改 DSP。
26. 不修改实时音频线程。
27. 第一版尽量保持现有 REST / SSE API。
28. 页面仍由 Rust binary 内嵌静态资源提供。

### 工程质量

29. `cargo fmt` 通过。
30. `cargo clippy` 通过。
31. `cargo test` 通过。
32. `cargo build --release` 通过。
33. 没有硬件时明确标记未做 hardware smoke test。

---

# 62. 最终结果报告格式

完成后输出：

```text
## 完成内容

- ...

## 主要 UI 变化

- ...

## 修改文件

- web/index.html
- web/styles.css
- web/app.js
- ...

## 保持不变的 Contract

- REST API
- AppConfig
- Recording
- Pipeline
- DOA/BF DSP
- channel mapping

## 自动测试

cargo fmt ...
PASS

cargo clippy ...
PASS

cargo test ...
PASS

cargo build --release
PASS

## 硬件验证

已完成 / 未执行（无 ReSpeaker）

## 剩余问题

仅列真实存在的问题。
没有则明确写：未发现 blocker。
```

不要为了报告显得完整而虚构问题。

---

# 63. 核心产品原则

本次改版最终必须遵守：

> 首页让客户选择“想做什么”。

> 算法设置让高级用户调整“怎么做”。

> 高级设置才暴露“具体怎么算”和诊断信息。

复杂度可以存在于系统内部，但不应该默认存在于客户眼前。