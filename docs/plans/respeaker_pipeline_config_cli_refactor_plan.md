# ReSpeaker 专用录音 CLI 与 Pipeline 配置重构设计

## 1. 目标与边界

CLI 收口为仅支持 Windows ReSpeaker Mic Array v2.0 的录音程序。程序固定使用
WASAPI 独占模式、16 kHz、16-bit、6 通道输入，不再允许通过 CLI 改变设备、
后端、采样率或通道数。

录音始终输出现有三路 WAV。只有显式传入 `--pipeline-config` 时，拆分后的音频块
才额外进入内置 Rust 算法 pipeline。模块按配置顺序串联执行，并通过共享 pipeline
状态把上游结果传给下游。当前只实现 DOA 模块配置，但配置结构允许以后加入 AEC、
BF 等模块。

本次不实现 AEC/BF，不支持外部程序、动态库、脚本或模型插件。

## 2. CLI 契约

```text
respeaker_algo [OPTIONS]

--duration <SECONDS>       录制时长，0 表示直到 Ctrl+C
--out-dir <PATH>           输出目录，默认 target/out
--prefix <PREFIX>          输出前缀，默认当前时间戳
--pipeline-config <PATH>   可选的内置算法 pipeline TOML
--help                     Clap 标准帮助标志
--version                  Clap 标准版本标志
```

删除：

- `list-devices` 与 `help` 子命令；
- `--device`、`--rate`、`--channels`、`--backend`；
- `--doa`、`--doa-csv`、`--doa-beta`、`--doa-cpsd-tau-ms`、
  `--doa-offset-deg`、`--doa-clockwise`。

## 3. Pipeline 配置格式

配置使用带版本号的 TOML。模块是否启用由 `enabled` 控制，默认 `true`；模块按
`[[modules]]` 的声明顺序构建和执行。

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

规则：

- `version` 当前必须为 `1`；
- 当前仅支持 `type = "doa"`；未知类型必须明确报错；
- 同一种已启用模块最多出现一次，避免输出文件冲突；
- 模块依赖必须由声明顺序满足；未来 BF 配置必须位于 DOA 之后，消费 DOA 发布的
  最新方向状态；
- 所有模块均禁用时允许运行，效果等同纯录音；
- 缺省字段使用现有 `DoaConfig::default()`；
- 文件读取、TOML 解析和参数校验必须在启动 WASAPI 采集前完成。

## 4. 代码结构

新增 `src/pipeline.rs`：

- `PipelineConfig`：读取、反序列化并校验 TOML；
- `ModuleConfig`：可扩展的 tagged enum，当前只有 `Doa`；
- `PipelineRuntime`：持有按配置顺序创建的模块运行时；
- `PipelineModuleRuntime`：运行时 enum，当前只有 `Doa(DoaRuntime)`。
- `PipelineState`：保存模块间共享的有类型状态；当前包含最新 `DoaResult`，未来 BF
  从这里读取方向，不重新计算 DOA。

录音循环完成 6 通道拆分后，将 `algo`、`mic`、`reference` 三个只读切片作为一个
pipeline 输入块传入。每个模块依次读取输入和 `PipelineState`，再发布自己的输出
状态供后续模块消费。DOA 当前只消费 `mic` 并更新最新方向；保留完整输入视图，避免
未来 AEC/BF 再次改动 recorder 接口。Pipeline 不修改三路录音缓冲，现有 WAV 内容
保持不变。

删除 `src/audio.rs` 和 `cpal` 依赖。`src/wasapi.rs` 只保留自动查找 ReSpeaker 和
独占采集所需接口，不再暴露设备列表或按索引选择。

## 5. 错误处理与输出

- 未发现 ReSpeaker 时直接返回明确错误；
- pipeline 配置不存在、版本错误、类型未知、模块重复或参数非法时，采集不得启动；
- DOA CSV 仍写入 `{out_dir}/{prefix}_respeaker_doa.csv`；
- pipeline 运行时错误终止录制并返回错误，不静默降级；
- 未配置 pipeline 时不输出 DOA 日志或 CSV。

## 6. 验证与迁移

新增测试覆盖：

- 空配置、合法 DOA 配置和字段默认值；
- 未知版本、未知模块、重复 DOA、非法 DOA 参数；
- `--pipeline-config` 缺省时纯录音路径不创建 pipeline；
- CLI 不再接受已删除的子命令和选项；
- pipeline 输入不修改录音拆分缓冲；
- DOA 结果发布到共享状态，依赖顺序不满足时拒绝配置；
- 现有 DOA、分帧、几何、跟踪和 WAV 测试继续通过。

完成后运行：

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

Windows 真机验收至少覆盖纯录音和带 DOA 配置各一次。
