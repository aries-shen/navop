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

Phase 1 没有伪装成完整产品，以下部分仍需后续实现：

1. 七个真实 provider binary 及其各自的认证、连接池、分页、重试和数据模型。
2. Navop GPUI Home/sidebar/tab 的生产级 panel activation、生命周期和错误呈现。
3. IPC process supervisor、崩溃重启、退避和 shutdown 状态机。
4. Blob/event stream 的宿主 backing store、订阅、背压和清理策略。
5. Secret store、capability sandbox、危险写操作的逐项确认与审计。
6. Terminal session / capability manager 到 declarative `<terminal>` 的 trusted
   host 注入链路。当前组件只允许引用宿主已批准 session，provider 不能声明
   shell、command、cwd、env 或连接参数。
7. `connections` 的强类型 schema；当前它只是供 catalog 展示的松散 metadata。
8. 真实 GPUI Dialog / Window activation manager。`ui/dialog` 与 `ui/window`
   目前只有 versioned wire contract、typed facade 与本地 request validation。

建议先实现一个真实 provider（优先 Kafka 或 Kubernetes）作为端到端样板，再补齐
上述宿主能力。
