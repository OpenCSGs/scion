# 指标与追踪

`scion server` 和 `scion cache` 提供 Prometheus 指标和 OpenTelemetry 追踪，两者都默认关闭。

```sh
scion server --metrics-listen 0.0.0.0:9464 --otlp-endpoint http://otel-collector:4317 ...
scion cache  --metrics-listen 0.0.0.0:9464 --otlp-endpoint http://otel-collector:4317 ...
```

| 参数 | 环境变量 | 说明 |
|---|---|---|
| `--metrics-listen ADDR` | `SCION_METRICS_LISTEN` | `GET /metrics` 的 HTTP 监听地址（OpenMetrics 文本格式）。独立端口，不经 gRPC 的认证与限流，不要暴露到公网。 |
| `--otlp-endpoint URL` | `SCION_OTLP_ENDPOINT` | OTLP gRPC 地址，把 trace 导出到 OpenTelemetry collector。 |
| `--log-format text\|json` | `SCION_LOG_FORMAT` | 日志格式，日志级别用 `SCION_LOG`（`EnvFilter` 语法，默认 `info`）。 |

## 指标

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `scion_rpc_requests_total` | counter | `service`, `method`, `code` | 每个 RPC 的请求数，按 gRPC 状态码（`OK`、`NOT_FOUND` …）。客户端中途断开记为 `CANCELLED`。协议之外的路径归入 `service="unknown", method="unknown"`，任意路径不会新增时间序列 |
| `scion_rpc_duration_seconds` | histogram | `service`, `method` | 从收到请求到响应结束。流式响应（例如 `WatchRefs`）到流结束为止 |
| `scion_rpc_in_flight` | gauge | | 正在处理的请求 |
| `scion_rejected_requests_total` | counter | `reason` | 被拒绝的请求：`overload`（全局在途上限）、`repo_rate`、`tenant_rate`、`tenant_concurrency`、`deadline` |
| `scion_land_batches_total` | counter | `outcome` | 合入批次：`landed`；`retried`（ref 被并发修改，CAS 失败后重做）；`failed` |
| `scion_land_batch_size` | histogram | | 每批的合入请求数 |
| `scion_land_duration_seconds` | histogram | | 一个合入请求从排队到得到结果 |
| `scion_object_cache_requests_total` | counter | `layer`, `result` | 服务端进程内对象缓存（`layer="l3"`）的命中（`hit`）与未命中（`miss`） |
| `scion_l2_lookups_total` | counter | `result` | 区域缓存的命中与未命中 |
| `scion_l2_fetched_objects_total` | counter | `from` | 区域缓存未命中时取到的对象：`peer`（同区域的其它缓存节点）或 `upstream`（服务端） |
| `scion_l2_upstream_requests_total` | counter | | 区域缓存发往服务端的请求 |
| `scion_l2_checks_total` | counter | `kind` | 区域缓存向服务端确认的可见性（`visibility`）与读权限（`access`） |

常用查询：

```promql
# 每层的命中率
sum(rate(scion_l2_lookups_total{result="hit"}[5m])) / sum(rate(scion_l2_lookups_total[5m]))
sum(rate(scion_object_cache_requests_total{result="hit"}[5m])) / sum(rate(scion_object_cache_requests_total[5m]))

# GetObjects 的 p99
histogram_quantile(0.99, sum by (le) (rate(scion_rpc_duration_seconds_bucket{method="GetObjects"}[5m])))

# 合入的 p99 与 CAS 失败率
histogram_quantile(0.99, sum by (le) (rate(scion_land_duration_seconds_bucket[5m])))
sum(rate(scion_land_batches_total{outcome="retried"}[5m])) / sum(rate(scion_land_batches_total[5m]))

# 错误率（不含业务上预期的 NOT_FOUND）
sum(rate(scion_rpc_requests_total{code!~"OK|NOT_FOUND"}[5m])) / sum(rate(scion_rpc_requests_total[5m]))
```

## 追踪

- **span**：每个 gRPC 请求一个 span（`rpc.service`、`rpc.method`），健康检查除外。日志事件带上所在的 span，JSON 日志中可以按请求关联。
- **接续 trace**：请求带 W3C Trace Context 的 `traceparent` 头时，接续调用方的 trace。
- **跨节点传递**：
  - 集群内转发给 leader 的合入，注入当前 span 的 `traceparent`，leader 接续同一个 trace；
  - 区域缓存回源时，同样注入当前 span 的 `traceparent`；
  - 区域缓存没有启用追踪时，原样转发调用方的 `traceparent`，trace 不会断开。
- **启用方式**：
  - 设置 `--otlp-endpoint`，或者只设置标准环境变量 `OTEL_EXPORTER_OTLP_ENDPOINT` / `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`；
  - 其它参数（采样率 `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG`、`OTEL_RESOURCE_ATTRIBUTES` 等）同样按 OpenTelemetry 的标准环境变量；
  - `service.name` 为 `scion-server` 或 `scion-cache`。
- **退出时**：导出尚未发送的 span。
- **尚未实现**：客户端 `scn` 和 SDK 不会自动注入 `traceparent`。
