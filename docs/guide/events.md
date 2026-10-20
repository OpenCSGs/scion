# Ref events

English | [简体中文](events.zh-CN.md)

Every ref change (created, moved forward, force-moved or deleted, lands and forks included) produces an event. The event is written to the metadata database in **the same transaction** as the ref change, with a globally increasing sequence number `seq` whose order is the order of the commits. A consumer records the last `seq` it handled and reads after it next time, missing no change.

There are two ways to consume them:

| How | Suits | What |
|---|---|---|
| Pull: `EventService.ListRefEvents` | Services that track their own progress | Pages by `seq`, optionally filtered by repository |
| Push: webhook / NATS JetStream | Deployments with a messaging system already (such as starhub) | The server pushes in `seq` order, at least once |

Events are kept for `--event-retention` (7 days by default) and cleaned up by the janitor.

## Pulling

```
ListRefEvents(after_seq, limit, repo) → { events, next_seq, truncated }
```

- **Permissions:** with `repo`, read on that repository; without it (all repositories), system administration.
- **Reused names:** after a repository is purged, its events are kept for the retention period. When its name is reused, pulling by repository returns only events after the new repository was created; an administrator's pull without `repo` still sees the old one's history. Skipped old events count in `next_seq` too.
- **Contents:** each event has `seq`, `repo`, `ref`, the commits before and after (an empty `old_changeset_id` means created, an empty `new_changeset_id` deleted), the version after and the time.
- **`truncated` is true:** some events after `after_seq` were already cleaned up after the retention period, so the consumer must resync every ref with `ListRefs`.
- **SDK:** `Client::list_ref_events(after_seq, limit, repo)`.

## Pushing

```sh
scion server ... \
  --event-webhook https://csghub.example.com/api/v1/scion/events \
  --event-webhook-secret "$SECRET" \
  --event-nats nats://nats:4222 --event-nats-subject scion
```

Both targets can be configured at once, each with its own progress.

- **Starting point:** a newly configured target gets events from that point on; history is not replayed.
- **Clusters:** each target is handled by the node holding its lease (`events/webhook`, `events/nats`); when that node fails, another takes over within 15 seconds.
- **Progress:** kept in the metadata database, so restarts and failovers lose none.
- **Failures:** retried in `seq` order (backing off from 1 to 60 seconds) without skipping events, so an event may arrive more than once; consumers deduplicate by `seq`.
- **A backlog older than the retention period:** events cleaned up before they were pushed cannot be pushed any more, and the server logs an error.

### Contents

```json
{
  "type": "ref.updated",
  "seq": 1042,
  "repo": "opencsg/qwen",
  "ref": "main",
  "old": "c1…",
  "new": "c1…",
  "old_git_sha": "3f7a…",
  "new_git_sha": "9c21…",
  "version": 17,
  "time_ms": 1760000000000,
  "commits": [
    {
      "id": "c1…",
      "git_sha": "9c21…",
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

- **`type`:** `ref.created`, `ref.updated` or `ref.deleted`.
- **`old_git_sha`, `new_git_sha`, `git_sha`:** the git-compatible SHAs of those commits (40 hex digits; see [git-compatible commit SHAs](git-sha.md)), or `null` when they cannot be computed (file contents purged).
- **`commits`:** the commits the change brings in, reachable from `new` and not from `old`, newest first.
  - At most 100; beyond that, `commits_truncated` is true.
  - Empty for a deletion.
  - Empty for a force-move back to an older commit too.
- **`changes`:** each commit's changed paths against its first parent, with `op` `put` or `delete`.
  - At most 1,000; beyond that, `changes_truncated` is true.
  - Paths and sizes only, never file contents.
- **When history can no longer be read** (the repository was purged, say), the event itself is still pushed, with empty `commits` and `commits_truncated` true.

### Webhook

A JSON `POST` with these headers:

| Header | Contents |
|---|---|
| `X-Scion-Event` | The event type |
| `X-Scion-Delivery` | `seq` (for deduplication) |
| `X-Scion-Signature-256` | `sha256=<hex>`: the HMAC-SHA256 of the body keyed with `--event-webhook-secret`, in GitHub's format. To verify, compute it over the raw body and compare in constant time |

A 2xx response means delivered; any other status, or a timeout (10 seconds), is retried.

### NATS JetStream

- **Subjects:** `<prefix>.ref.created`, `<prefix>.ref.updated` and `<prefix>.ref.deleted`, with the prefix `scion` by default.
- **Stream:** create a stream covering `<prefix>.ref.>` beforehand. Repository names are in the message, not the subject, since they may contain `.`.
- **Deduplication:** each message carries `Nats-Msg-Id: scion-<seq>`, so JetStream drops a repeated publish within the stream's deduplication window.
- **Delivery:** an event counts as delivered once JetStream acknowledges it.

## Metrics

| Metric | What |
|---|---|
| `scion_events_delivered_total{sink}` | Events pushed successfully |
| `scion_event_delivery_failures_total{sink}` | Failed push attempts (retried later) |
| `scion_event_lag{sink}` | Events not pushed yet |

## Pre-land checks

Before a land (`scn land`, `LandService.Land`), a branch creation or a branch move (`CreateRef` / `UpdateRef`), the server can hand the changes coming in to an external service to check: content review, or limits on file types or sizes, for example.

```sh
scion server ... \
  --pre-land-webhook https://csghub.example.com/api/v1/scion/pre-land \
  --pre-land-secret "$SECRET" \
  [--pre-land-timeout 5s] [--pre-land-fail-open]
```

### Request

A JSON `POST` with the header `X-Scion-Event: pre-land`, and `X-Scion-Signature-256` when a secret is configured (the same HMAC format as event pushes).

```json
{
  "type": "pre-land",
  "repo": "opencsg/qwen",
  "ref": "main",
  "old": "c1…",
  "new": "c1…",
  "old_git_sha": "3f7a…",
  "new_git_sha": "9c21…",
  "pusher": {"subject": "alice", "groups": ["dev"], "authenticated": true},
  "commits": [ … ],
  "commits_truncated": false
}
```

- **`old` and `new`:** `old` is the ref's current commit (empty when the ref does not exist), `new` the version the client submitted; `old_git_sha` and `new_git_sha` are their git-compatible SHAs.
- **`commits`:** the commits coming in, in the format of event pushes (path-level changes, no file contents).
  - Their range: reachable from `new` and not from `old`.
  - A new branch is compared with the default branch, whose contents were checked when they landed.
  - Without new commits (a repeated land, a move back, a move to existing content), the check service is not called.

Lands rebase on the server, and the check sees the version the client submitted; a rebase does not change what each commit changes.

### Response

| Response | Outcome |
|---|---|
| 2xx | Allowed; denied when the body is `{"allowed": false, "message": "…"}` |
| 2xx for a truncated request | Denied, asking the client to split the change and retry. Truncated means over 100 commits, or over 1,000 paths in one commit, with `commits_truncated` or `changes_truncated` true in the request. Only an explicit `{"allowed": true, "accept_truncated": true}` lets it through, meaning the check service accepts having seen only part of it |
| 4xx (but 408 and 429) | Denied, with the body as the reason |
| Any other status, a timeout, a connection failure | The check service is unavailable: denied by default (`UNAVAILABLE`, which clients can retry), allowed with `--pre-land-fail-open` |

- **When denied:** the client gets `FAILED_PRECONDITION` / `PRE_LAND_REJECTED` with the reason in `metadata.message`, which `scn` shows.
- **Clusters:** a land forwarded to the leader is checked on the first node; with `--cluster-secret` configured, the leader does not check it again.
- **Metric:** `scion_pre_land_checks_total{outcome="allowed|denied|error"}`.
