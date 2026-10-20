# Metrics and tracing

English | [简体中文](observability.zh-CN.md)

`scion server` and `scion cache` export Prometheus metrics and OpenTelemetry traces, both off by default.

```sh
scion server --metrics-listen 0.0.0.0:9464 --otlp-endpoint http://otel-collector:4317 ...
scion cache  --metrics-listen 0.0.0.0:9464 --otlp-endpoint http://otel-collector:4317 ...
```

| Option | Environment variable | What |
|---|---|---|
| `--metrics-listen ADDR` | `SCION_METRICS_LISTEN` | HTTP address for `GET /metrics` (OpenMetrics text). A separate port, outside gRPC's authentication and rate limits: do not expose it publicly. |
| `--otlp-endpoint URL` | `SCION_OTLP_ENDPOINT` | OTLP gRPC address to export traces to an OpenTelemetry collector. |
| `--log-format text\|json` | `SCION_LOG_FORMAT` | Log format; the level is set by `SCION_LOG` (`EnvFilter` syntax, `info` by default). |

## Metrics

| Metric | Type | Labels | What |
|---|---|---|---|
| `scion_rpc_requests_total` | counter | `service`, `method`, `code` | Requests per RPC, by gRPC status (`OK`, `NOT_FOUND` …). A client that disconnects midway counts as `CANCELLED`. Paths outside the protocol count as `service="unknown", method="unknown"`, so arbitrary paths add no time series |
| `scion_rpc_duration_seconds` | histogram | `service`, `method` | From receiving a request to the end of its response; for streaming responses (such as `WatchRefs`), to the end of the stream |
| `scion_rpc_in_flight` | gauge | | Requests being handled |
| `scion_rejected_requests_total` | counter | `reason` | Refused requests: `overload` (the global in-flight limit), `repo_rate`, `tenant_rate`, `tenant_concurrency`, `deadline` |
| `scion_land_batches_total` | counter | `outcome` | Land batches: `landed`; `retried` (the ref was changed concurrently and the CAS failed, so the batch was redone); `failed` |
| `scion_land_batch_size` | histogram | | Land requests per batch |
| `scion_land_duration_seconds` | histogram | | One land request, from queueing to its result |
| `scion_object_cache_requests_total` | counter | `layer`, `result` | Hits (`hit`) and misses (`miss`) of the server's in-process object cache (`layer="l3"`) |
| `scion_l2_lookups_total` | counter | `result` | Hits and misses of the regional cache |
| `scion_l2_fetched_objects_total` | counter | `from` | Objects the regional cache fetched on a miss: from a `peer` (another cache node in the region) or `upstream` (the server) |
| `scion_l2_upstream_requests_total` | counter | | Requests from the regional cache to the server |
| `scion_l2_checks_total` | counter | `kind` | Visibility (`visibility`) and read permission (`access`) checks the regional cache made with the server |

Common queries:

```promql
# Hit rate per layer
sum(rate(scion_l2_lookups_total{result="hit"}[5m])) / sum(rate(scion_l2_lookups_total[5m]))
sum(rate(scion_object_cache_requests_total{result="hit"}[5m])) / sum(rate(scion_object_cache_requests_total[5m]))

# p99 of GetObjects
histogram_quantile(0.99, sum by (le) (rate(scion_rpc_duration_seconds_bucket{method="GetObjects"}[5m])))

# p99 of lands and the CAS failure rate
histogram_quantile(0.99, sum by (le) (rate(scion_land_duration_seconds_bucket[5m])))
sum(rate(scion_land_batches_total{outcome="retried"}[5m])) / sum(rate(scion_land_batches_total[5m]))

# Error rate (without the expected NOT_FOUND)
sum(rate(scion_rpc_requests_total{code!~"OK|NOT_FOUND"}[5m])) / sum(rate(scion_rpc_requests_total[5m]))
```

## Tracing

- **Spans:** one per gRPC request (`rpc.service`, `rpc.method`), health checks excepted. Log events carry their span, so JSON logs can be correlated by request.
- **Continuing a trace:** a request with a W3C Trace Context `traceparent` header continues the caller's trace.
- **Across nodes:**
  - a land forwarded to the leader carries the current span's `traceparent`, and the leader continues the same trace;
  - the regional cache's requests to the server carry the current span's `traceparent` too;
  - a regional cache without tracing forwards the caller's `traceparent` as it is, so the trace is not broken.
- **Turning it on:**
  - set `--otlp-endpoint`, or only the standard environment variables `OTEL_EXPORTER_OTLP_ENDPOINT` / `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`;
  - everything else (sampling with `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG`, `OTEL_RESOURCE_ATTRIBUTES` and so on) follows OpenTelemetry's standard environment variables too;
  - `service.name` is `scion-server` or `scion-cache`.
- **On exit:** spans not sent yet are exported.
- **Not implemented yet:** the `scn` client and the SDK do not inject `traceparent` on their own.
