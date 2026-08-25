# Protocol

## Resource lifecycle

公共协议只描述资源会话和调用形状，领域方法使用字符串命名空间：

```text
resource/open
resource/ping
resource/invoke
resource/close
```

`resource/open` 建立一个 provider-side resource handle。`resource/invoke` 携带：

```json
{
  "resource_id": "connection-1",
  "method": "kafka/topic/list",
  "params": {
    "include_internal": false
  }
}
```

provider 必须对未知 method 显式返回 unsupported/method-not-found，不能静默返回
空数组。这样可以在不修改公共协议的情况下增加新的领域 provider。

## Jobs and result references

长操作使用：

```text
job/start
job/status
job/cancel
job/result
job/close
```

典型方法：

- `kafka/message/consume`
- `docker/container/logs`
- `kubernetes/pod/logs`
- `elasticsearch/search`
- `api_test/request/send`

结果可以是：

```json
{ "kind": "inline", "value": { "items": [] } }
{ "kind": "blob", "id": "blob-123" }
{ "kind": "event_stream", "id": "stream-123" }
```

inline JSON 是便利路径，不是无限大的传输通道。宿主对 `resource/invoke` 与
`job/result` 的返回值应用统一边界：序列化后超过 4 MiB 的 inline 结果会被宿主
复制为 `host-blob-*` 并仍以 `ResultRef::Blob` 返回。provider 自己返回的
`blob-123` 保持 provider 所有权并路由回该 provider；`host-blob-*` 是宿主专用
命名空间，provider 不能探测、伪造或要求宿主透传。host blob 的读取授权绑定
exact runtime + generation，provider 进程重启或 deactivation 后自动失效。

`ProgressPercent` 限制在 `0..=100`。取消必须幂等；provider 应尽快停止网络、
文件和子进程工作。

## Event streams

持续日志、watch 或消息消费使用：

```text
event/open
event/read
event/close
```

`event/open` 返回 provider-local `stream_id`。provider 必须为每个 stream 维护
有界 buffer；宿主通过 `event/read` 拉取 batch：

```json
{
  "stream_id": "stream-123",
  "max_events": 128,
  "wait_ms": 250
}
```

`max_events` 未提供时默认 128，有效范围会被限制在 `1..=1024`。返回值携带事件
batch、`closed` 终态标记和 buffer overflow 时累计的 `dropped_count`：

```json
{
  "events": [],
  "closed": false,
  "dropped_count": 0
}
```

宿主把 `stream_id` 注册为 extension + runtime + generation + stream 的精确
所有权。同一 runtime generation 内 duplicate stream 会被拒绝，open stream 数量
有 per-runtime limit。旧 generation 的 client 不能读取或关闭 replacement
process 的同 ID stream；runtime deactivation、extension deactivation 或成功
重启会使旧 generation 的注册项失效。provider 返回 `closed` 或宿主执行
`event/close` 后，宿主注册项被释放；provider close 失败不能让已失效注册项
永久占用配额。

当前宿主完成的是 stream registry、owner 校验和 lifecycle cleanup。后台订阅
任务、UI bridge、跨 restart 恢复和统一 backpressure/cancellation 策略仍待实现。

## Declarative UI actions

模板使用 `action` 和 `data-*`：

```html
<button
    action="refresh-resources"
    data-provider="kafka"
    data-method="kafka/topic/list"
>
    Refresh
</button>
```

运行时把它序列化为：

```json
{
  "request_id": "ui-42",
  "action": "refresh-resources",
  "source_id": "topics",
  "source_path": [0, 2],
  "payload": {
    "provider": "kafka",
    "method": "kafka/topic/list"
  },
  "expected_revision": 7
}
```

provider 返回 `UiStatePatch` 后，宿主调用
`Runtime::apply_external_patch`。patch 带 `expected_revision` 和有序 operation：

```json
{
  "expected_revision": 7,
  "operations": [
    { "operation": "set", "key": "status", "value": "ready" },
    { "operation": "set", "key": "items", "value": "[...]" }
  ]
}
```

patch 应用是 atomic 的：revision conflict 会整批失败，成功批量更新只触发一次
状态事件和一次 reconcile。

## Declarative UI dialogs and windows

Provider 不能持有 native GPUI Window、Dialog 或 focus API。它只能发送版本化
request，由宿主 activation manager 检查权限、owner、panel 注册和生命周期后权威
执行。

### `ui/dialog`

```json
{
  "request_id": "request-1",
  "dialog_id": "delete-topic",
  "kind": "confirm",
  "title": "Delete topic",
  "message": "This operation cannot be undone.",
  "confirm_label": "Delete",
  "cancel_label": "Cancel",
  "danger": true,
  "expected_revision": 7
}
```

`kind` 是 `alert` / `confirm` / `prompt`。结果显式建模为终态：

```json
{ "result": "confirmed" }
{ "result": "cancelled" }
{ "result": "dismissed" }
{ "result": "prompt", "value": "orders" }
```

用户按 Esc、点击关闭按钮或点击 modal mask 都应返回 `dismissed`，不能被折叠成
`confirmed`。`expected_revision` 只是 UI state patch 语义；`request_id` 用于 host
去重，不承担 revision 冲突检测。

Reverse Host API 的宿主实现会把 request 放入 extension + runtime + generation +
`request_id` 命名空间。generation 已被替换或 request 尚未完成的重复 ID 会被
拒绝；同一 runtime 的 pending dialog 有数量上限。runtime deactivation、
extension deactivation 或成功重启会返回显式 `dismissed`。该结果表示宿主清理，
不是用户确认；provider 必须把它作为终态处理。

### `ui/window`

```json
{
  "request_id": "request-1",
  "window_id": "topic-detail",
  "operation": {
    "operation": "open",
    "title": "Topic detail",
    "width": 1024,
    "height": 768,
    "panel_id": "kafka.topic-detail",
    "modal": false
  }
}
```

`operation` 还支持：

```json
{ "operation": "close" }
{ "operation": "set_title", "title": "Topic detail" }
```

`panel_id` 必须引用当前 extension manifest 注册的 declarative panel。host 必须把
`window_id` 限制在 owner runtime / panel / extension 命名空间内，并在 owner
runtime、panel 或 extension 关闭 / 卸载时自动关闭相关 window 和 dialog。

### Wire contract limits

typed client 发送 RPC 前会拒绝：

- 空 ID、超过 128 bytes 或包含非 ASCII alphanumeric / `.` / `_` / `-` 的
  `request_id`、`dialog_id`、`window_id`、`panel_id`；
- 空 title、超过 512 bytes 或含 control character 的 title；
- 超过 8192 bytes 或含除 `\n` / `\r` / `\t` 外 control character 的 message；
- 空 label、超过 128 bytes 或含 control character 的 confirm / cancel label；
- 不在 `200..=16384` px 范围内的 window width / height。

这两个方法需要对应 UI permission：建议 gate 为 `ui:dialog` 与 declarative panel
所属的 `ui:window` permission。危险确认还必须复用 destructive operation
confirmation 与 audit 流程。当前 `ui/dialog` 已有宿主生命周期管理与生产排队
presenter，但真实 GPUI modal presenter、focus owner、permission gate 与 audit
仍未完成；`ui/window` 尚未实现 activation lifecycle。

## Domain method registry

领域方法不放进 Rust 公共 enum；provider 自己声明能力，宿主按 namespaced string
路由。参考命名：

| Provider | Methods |
| --- | --- |
| Nacos | `nacos/namespace/list`, `nacos/config/get`, `nacos/config/publish`, `nacos/service/instances` |
| Elasticsearch | `elasticsearch/index/list`, `elasticsearch/index/mappings`, `elasticsearch/search`, `elasticsearch/document/index` |
| RocketMQ | `rocketmq/topic/list`, `rocketmq/topic/route`, `rocketmq/message/send`, `rocketmq/message/query` |
| Kafka | `kafka/topic/list`, `kafka/topic/describe`, `kafka/message/produce`, `kafka/message/consume` |
| Docker | `docker/container/list`, `docker/container/inspect`, `docker/container/logs`, `docker/image/list` |
| Kubernetes | `kubernetes/resource/list`, `kubernetes/resource/apply`, `kubernetes/pod/logs`, `kubernetes/events/list` |
| API Test | `api_test/request/send`, `api_test/request/validate`, `api_test/environment/list`, `api_test/collection/save` |

建议 provider 在 `resource/open` 返回版本、capabilities 和可用方法，界面据此隐藏
不支持的操作；宿主仍需执行 capability 和 confirmation 检查。
