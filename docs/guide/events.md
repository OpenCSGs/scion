# ref 变化事件

每次 ref 变化（创建、前进、强制移动、删除，包括合入和 fork）都会产生一条事件。事件与 ref 的修改在**同一个事务**里写入元数据库，带一个全局递增的序号 `seq`，序号顺序与提交顺序一致。消费方记下处理到的 `seq`，下次从它之后读取，不会漏掉变化。

有两种消费方式：

| 方式 | 适合 | 说明 |
|---|---|---|
| 拉取：`EventService.ListRefEvents` | 自己管理进度的服务 | 按 `seq` 分页读取，可以按仓库过滤 |
| 推送：webhook / NATS JetStream | 已有消息系统的部署（例如 starhub） | 服务端按 `seq` 顺序推送，至少送达一次 |

事件保留 `--event-retention`（默认 7 天），由 janitor 清理。

## 拉取

```
ListRefEvents(after_seq, limit, repo) → { events, next_seq, truncated }
```

- **权限**：指定 `repo` 时需要该仓库的 `read`；不指定时（全部仓库）需要系统管理员。
- **事件内容**：每条包含 `seq`、`repo`、`ref`、变化前后的提交（`old_changeset_id` 为空表示新建，`new_changeset_id` 为空表示删除）、变化后的版本和时间。
- **`truncated` 为 true**：`after_seq` 之后的部分事件已经超过保留期被清理，消费方需要用 `ListRefs` 重新同步全部 ref。
- **SDK**：`Client::list_ref_events(after_seq, limit, repo)`。

## 推送

```sh
scion server ... \
  --event-webhook https://csghub.example.com/api/v1/scion/events \
  --event-webhook-secret "$SECRET" \
  --event-nats nats://nats:4222 --event-nats-subject scion
```

两个目标可以同时配置，各自独立记录进度。

- **起点**：第一次配置某个目标时，从当时的位置开始推送，不重放历史事件。
- **集群**：每个目标由持有租约（`events/webhook`、`events/nats`）的一个节点负责，节点故障时其它节点在 15 秒内接手。
- **进度**：保存在元数据库中，节点重启或切换都不会丢失。
- **失败**：按 `seq` 顺序重试（间隔从 1 秒退避到 60 秒），不跳过事件。所以同一事件可能送达多次，消费方按 `seq` 去重。
- **积压超过保留期**：落后的事件被清理后无法再推送，服务端日志记录一条 error。

### 内容

```json
{
  "type": "ref.updated",
  "seq": 1042,
  "repo": "opencsg/qwen",
  "ref": "main",
  "old": "c1…",
  "new": "c1…",
  "version": 17,
  "time_ms": 1760000000000,
  "commits": [
    {
      "id": "c1…",
      "parents": ["c1…"],
      "author": {"name": "Ada", "email": "ada@example.com", "time_ms": 1760000000000},
      "message": "update weights",
      "changes": [{"path": "model.safetensors", "op": "put", "size": 4718592}],
      "changes_truncated": false
    }
  ],
  "commits_truncated": false
}
```

- **`type`**：`ref.created`、`ref.updated` 或 `ref.deleted`。
- **`commits`**：本次变化引入的提交，即从 `new` 可达、从 `old` 不可达的提交，从新到旧。
  - 最多 100 个，超过时 `commits_truncated` 为 true；
  - 删除时为空；
  - 强制回退到旧提交时也为空。
- **`changes`**：每个提交相对第一个父提交的改动路径，`op` 为 `put` 或 `delete`。
  - 最多 1000 条，超过时 `changes_truncated` 为 true；
  - 只有路径和大小，不包含文件内容。
- **历史已不可读**（例如仓库已被清除）时，仍然推送事件本身，`commits` 为空，`commits_truncated` 为 true。

### webhook

`POST` JSON，请求头：

| 头 | 内容 |
|---|---|
| `X-Scion-Event` | 事件类型 |
| `X-Scion-Delivery` | `seq`（用于去重） |
| `X-Scion-Signature-256` | `sha256=<hex>`：以 `--event-webhook-secret` 为密钥、对请求体计算的 HMAC-SHA256，格式与 GitHub 相同。验证时对原始请求体计算并做常数时间比较 |

响应 2xx 视为送达，其它状态码和超时（10 秒）都会重试。

### NATS JetStream

- **subject**：`<前缀>.ref.created`、`<前缀>.ref.updated`、`<前缀>.ref.deleted`，前缀默认 `scion`。
- **stream**：需要事先创建一个覆盖 `<前缀>.ref.>` 的 stream。仓库名放在消息内容里，不放进 subject，因为仓库名可以包含 `.`。
- **去重**：每条消息带 `Nats-Msg-Id: scion-<seq>`，在 stream 的去重窗口内重复发布会被 JetStream 丢弃。
- **送达确认**：收到 JetStream 的确认才算送达。

## 指标

| 指标 | 说明 |
|---|---|
| `scion_events_delivered_total{sink}` | 成功推送的事件数 |
| `scion_event_delivery_failures_total{sink}` | 失败的推送尝试（之后会重试） |
| `scion_event_lag{sink}` | 尚未推送的事件数 |
