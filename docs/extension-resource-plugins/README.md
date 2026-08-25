# Universal Resource Plugins

这是 Navop 通用中间件插件机制的 **Phase 1 reference package**。它定义了一个
“manifest + declarative UI + native IPC provider” 的最小闭环，让 Nacos、
Elasticsearch、RocketMQ、Kafka、Docker、Kubernetes、API Test 等资源可以沿用
同一套宿主能力，而不必把每个领域都硬编码进公共协议。

本目录中的七个 extension package 是 **parser-valid reference manifests**，用于
验证协议和目录契约，不包含真实 provider binary，也不代表这些中间件已经可以在
Navop 中直接连接。

## 设计原则

- **通用外壳，领域命名空间**：公共协议只提供 `resource/*`、`job/*` 和
  `ui/action`；领域方法保留为 namespaced string，例如
  `kafka/topic/list`、`kubernetes/pod/logs`。
- **Declarative UI 只描述视图**：模板是受限 HTML，不执行 JavaScript、shell 或
  任意内联代码。按钮通过 `action` 和 `data-*` 生成可序列化的 UI action。
  模板禁止 inline `style` 与 `<style>`；manifest 可声明扩展目录内的 external CSS
  file。CSS 子集只支持简单 tag/class/id selector 和 GPUI 可表达的 layout、
  sizing、position、border、文本与 overflow 属性，先应用 CSS，再应用 Tailwind
  utility。
- **IPC 只传 versioned DTO**：宿主复用 `extension-host` 的
  `ProcessRpcSession`，provider 通过 local socket JSON-RPC 工作。
- **敏感能力显式声明**：网络、文件、secret、spawn、写操作和危险操作必须通过
  manifest 权限与宿主确认流程控制。
- **大结果可扩展**：小结果可用 inline JSON；大结果使用 `ResultRef::Blob`，
  持续日志或消息使用 `ResultRef::EventStream`。

## 目录

- [`architecture.md`](architecture.md)：组件边界、生命周期和安全边界。
- [`protocol.md`](protocol.md)：公共 RPC DTO、job、UI patch 及领域方法约定。
- [`examples/`](examples/)：七个 provider 的参考 manifest 与 declarative UI 模板。

## 七个参考 provider

| Provider | Extension ID | 典型能力 |
| --- | --- | --- |
| Nacos | `com.navop.nacos` | namespace、配置、服务与实例 |
| Elasticsearch | `com.navop.elasticsearch` | index、mapping、search、document |
| RocketMQ | `com.navop.rocketmq` | topic、route、message、consumer group |
| Kafka | `com.navop.kafka` | topic、partition、produce、consume、group |
| Docker | `com.navop.docker` | container、image、logs |
| Kubernetes | `com.navop.kubernetes` | resource、pod logs、events |
| API Test | `com.navop.api-test` | request、environment、collection |

## 当前边界

当前已完成的宿主闭环：

1. Home/sidebar/tab 的 panel activation、catalog projection、template/style
   text loading 和 host-owned renderer mounting。
2. 多 panel 共享同一 provider runtime 的引用计数 shutdown；最后一个 panel
   关闭后才停用 provider，panel close 同时释放宿主拥有的 terminal sessions。
3. `ui/action` 双向桥接、state patch 应用、request timeout 和错误呈现。
4. IPC process supervisor、崩溃重启、退避、restart budget 和 shutdown 状态机。
5. Runtime generation 防护：provider 进程被替换后，旧 panel/client 的 stale
   patch 不会写入新 runtime，重启后的 Active 事件会刷新 panel generation。
6. 宿主侧 bounded BlobStore 与结果链路：`resource/invoke` / `job/result` 返回的
   超过 4 MiB 的 inline JSON 会被宿主缓存为 `host-blob-*` 结果；host blob 按
   runtime + generation 授权读取，支持分块、EOF、幂等 close、
   per-blob/total quota、显式 reclaim 和 runtime restart/deactivation 清理。
   provider-owned blob ID 继续路由回 provider，`host-blob-` 命名空间不会透传，
   stale generation 也无法读取 replacement generation 的数据。
7. Provider 发起的 `ui/dialog` reverse Host API 生命周期：宿主按 extension、
   runtime、generation 和 `request_id` 命名空间隔离请求；pending 请求去重并
   限制数量，stale generation 直接拒绝。runtime deactivation、extension
   deactivation 或成功重启会显式返回 `dismissed`，且清理动作不会被折叠成
   `confirmed`。宿主 presenter 负责用户结果和 Esc / mask / close 清理。
8. Event stream 的宿主生命周期：`event/open`、`event/read` 与
   `event/close` 由 `EventActivationManager` 按 extension、runtime、
   generation 和 `stream_id` 精确授权；open stream 去重并限制数量，
   stale generation 不能读取或关闭 replacement process 的流。runtime
   deactivation、extension deactivation 与成功重启会清理旧代际流；close 或
   provider 返回 `closed` 后宿主注册表同步释放。

以下部分仍需后续实现：

1. 七个真实 provider binary 及其各自的认证、连接池、分页、重试和数据模型。
2. Reverse Host API 的 host blob 写入链路；磁盘 spilling、TTL/reclaim 调度和
   UI 展示策略。
3. Event stream 的后台订阅任务、UI bridge、跨 restart 恢复，以及超出
   provider read batch 的统一背压策略；当前完成的是宿主注册表与生命周期，不是
   自动订阅引擎。
4. Secret store、capability sandbox、危险写操作的逐项确认与审计。
5. Terminal session / capability manager 到 declarative `<terminal>` 的 trusted
   host 注入链路。当前组件只允许引用宿主已批准 session，provider 不能声明
   shell、command、cwd、env 或连接参数。
6. `connections` 的强类型 schema；当前它只是供 catalog 展示的松散 metadata。
7. 真实 GPUI Dialog presenter / Window activation manager。`ui/dialog` 已有宿主
   生命周期管理，但生产端仍是排队 presenter，尚无真实 GPUI modal 渲染与 focus
   owner；`ui/window` 目前只有 versioned wire contract、typed facade 与本地
   request validation。
8. Job lifecycle 的宿主实现、取消传播和跨 restart 恢复。

建议先实现一个真实 provider（优先 Kafka 或 Kubernetes）作为端到端样板，再补齐
上述宿主能力。
