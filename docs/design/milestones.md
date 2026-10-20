# Milestones

What each milestone delivered, newest first. Large tasks planned for later are in the [roadmap](roadmap.md).

**M5 (production hardening) is in progress.** Done so far:
- **Authentication and access control:**
  - tokens: self-issued ES256 access tokens, PATs, and refresh tokens;
  - external identities, standards only: JWKS or HS256 issuers, RFC 7662 introspection and an OPA-compatible authorization webhook. starhub-server is supported through configuration alone;
  - ACL rules by subject, repo, action, ref and path;
  - `scn login` with the OIDC device flow, credentials stored in the keychain.
- **Redaction:**
  - redact leaked file contents: object ids and history stay the same, reads return `REDACTED`;
  - the regional cache and the local daemon stop serving them within 10 seconds;
  - land never merges redacted content into new commits;
  - purge deletes the bytes after a grace period, once every repository that holds the object has redacted it.
- **Operations:** `scion admin fsck` checks refs, commits, trees and files against the object index and object store; `scion admin lease ls` lists leases.
- **Sparse checkout:** `scn clone --sparse a/,b/` and `scn sparse add|rm|set|disable`. Subtrees outside the scope are never fetched; commits, lands and rebases leave them untouched on the server.

- **Observability and limits:**
  - Prometheus metrics on `--metrics-listen`: per-RPC counts, codes and latency; land batches and latency; cache hit rates at L2 and L3; rejected requests;
  - OpenTelemetry tracing over OTLP, continuing the caller's `traceparent` across land forwarding and cache fetches;
  - per-caller rate and concurrency limits (`--tenant-rate`, `--tenant-concurrency`);
  - deadlines: the smaller of the client's `grpc-timeout` and `--max-request-timeout`, passed on to the leader and to the servers.

- **Repositories and branches:** `RepoService` (create, list, update, soft-delete with a restore window, fork) and branch create / move / delete, with `scn repo` and `scn branch`.
- **Deployment:** a Docker image with `scion` and `scn`, and a docker compose setup with PostgreSQL and SeaweedFS.
- **Ref events:**
  - every ref change is written in the same transaction as an ordered event;
  - consumers poll `EventService.ListRefEvents` by sequence number;
  - or the server pushes the events, with commit and path-level details, to a webhook (HMAC-signed) or NATS JetStream, at least once and in order.
- **Pre-land check:** before a land or a branch write, the server can POST the commits it would introduce to an external service, which allows or rejects them (fail-closed by default).

Still to come in M5: YugabyteDB, Helm and chaos testing.

**M4 (cache layer) is complete**:
- **Regional cache `scion cache` (L2):**
  - storage: foyer memory plus NVMe;
  - rendezvous sharding across nodes, so the region fetches each object from the server once;
  - single-flight coalescing, plus hedged reads and circuit breakers toward the servers;
  - warm-up from WatchRefs that only walks changed trees.
- **Local daemon `scn daemon` (L1):**
  - a verified on-disk object cache;
  - all other RPCs forwarded transparently;
  - prefetch of refs the working copy follows;
  - started on demand by `scn`.
- **WatchRefs:** streams ref changes to subscribers (snapshot first, then each advance), resumable after disconnects.
- **Server read path:**
  - in-process object cache;
  - coalesced, concurrency-limited backend reads;
  - `--read-endpoint` advertises caches to clients.

The cache serves 99.5% of reads without the server and 1.53M objects/s per node; see [performance](../performance.md).

Earlier milestones:
- M3 added server-side rebase, exactly-once lands, batched landing (885 lands/s on one ref), clusters and overload protection.
- M2 added history queries (p99 under 20 ms on 100k commits).
- M1 added the write path, Lite mode, the `scn` working copy and git import.

The remaining M5 work: the YugabyteDB tier, Helm and release builds.
