# ReSpeaker DOA Web Viewer 设计

## 1. 目标与边界

当 Pipeline 配置中存在已启用的 DOA 模块时，程序自动启动本地 Web Viewer；纯录音或仅包含禁用 DOA 的配置不启动。Viewer 仅用于实时观察，不改变 DOA 计算、共享 Pipeline 状态、CSV 或三路 WAV。

本次不增加 CLI 选项，不支持远程访问和历史文件回放。DOA 模块的 `enable_viewer` 参数默认为 `true`，同时控制 Viewer 服务和默认浏览器；启用时服务固定绑定 `127.0.0.1:8765`，设为 `false` 时不启动服务、SSE 或浏览器。

## 2. 数据流与生命周期

```text
DoaRuntime 每个 16 ms 结果
  -> PipelineState.latest_doa
  -> WebBroadcaster（JSON）
  -> SSE /events
  -> 浏览器实时渲染
```

`PipelineRuntime::new` 在创建 DOA 模块后绑定端口，因此端口占用会在 WASAPI 启动前报错。浏览器启动失败只打印告警，不中止录音。Pipeline 正常结束、运行错误或提前释放时，都发送关闭信号并回收 Web 服务线程。

## 3. Web 接口

- `GET /`：返回编译期嵌入的 `web/doa_viewer.html`；
- `GET /events`：SSE 流，事件名为 `doa`；
- 新连接立即收到最新结果，之后接收逐帧结果；
- 最多允许 8 个客户端，超限返回 HTTP 429。

消息包含序号、时间戳、原始/跟踪角度、置信度、跟踪状态、观测是否采用、峰值、MSC 和 RMS。角度在 Searching 阶段允许为 `null`，页面必须安全显示占位符。

## 4. 页面坐标

页面使用代码内 SVG 绘制四麦阵列，不依赖外部图片。只在显示层将算法坐标顺时针旋转 90°，使 0° 向下、90° 向右、180° 向上、270° 向左；mic1 位于右下、mic2 右上、mic3 左上、mic4 左下。原始角和跟踪角分别以不同颜色显示，并保留最近 10 秒跟踪轨迹。

## 5. 验证

- 单元测试验证首页、SSE 最新事件、客户端上限和带活动连接的关闭；
- Pipeline 测试验证只有已启用 DOA 才需要 Viewer；
- 配置测试验证 `enable_viewer` 默认开启，关闭时不创建 Viewer 服务或广播器；
- 页面契约测试验证 0°/90° 标签位置与指针旋转公式；
- 运行格式、Clippy、全部测试和 Release 构建；
- 真机验证需确认浏览器自动打开、角度方向正确且结束录音后端口释放。
