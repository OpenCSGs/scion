# 对象存储：SeaweedFS、MinIO 与其他 S3 兼容服务

Scion 服务端把文件内容、目录树和提交对象存放在对象存储中（`--blobs`）。可选后端：

| `--blobs` | 用途 |
|---|---|
| `mem` | 测试，进程退出即丢失 |
| 目录路径（或 `--lite --data-dir`） | 单机 Lite 档 |
| `s3://bucket[/prefix]` | 生产：SeaweedFS、MinIO、AWS S3，或其他兼容 S3 的服务 |

数据读写只用 S3 核心子集（PUT / GET（含 Range）/ HEAD / LIST），不依赖条件写或版本控制。SeaweedFS 和 MinIO 每次 CI 都会跑完整的存储一致性测试。

## Compression

The server stores objects zstd-compressed (level 3) when that saves at least 10%. Content that is already compressed, such as model weights, archives or images, is stored as it is.

- **Transparent**: object ids, sizes in the metadata and everything clients see are those of the original bytes.
- **Old objects**: objects written before compression, or with compression turned off, read back unchanged, and a later upload of the same object keeps the existing copy.
- **Effect**: importing starhub-server (6,312 commits) stores 227 MB of object bytes instead of 1,007 MB; git's pack for the same history is 392 MB.
- **Turning it off**: `SCION_BLOB_COMPRESSION=off` stores new objects uncompressed, for example when CPU is scarce and storage is cheap. Reads handle both forms either way.

## Packs

Objects written together are stored together: a commit's new trees and its changeset, and each batch of an upload (up to 16 MB or 4,096 objects), go into one *pack* under `packs/` instead of one stored object each. The metadata store records where each packed object is (`pack_locations`), and reads fetch just its byte range.

- **Why**: most objects are small (the trees and commits of starhub-server are around 1 KB). On MinIO every stored object is a directory plus an `xl.meta` file of at least a disk block each, and every write is a request. Importing starhub-server stored 67,636 objects taking 546 MB of MinIO disk; with packs it stores 10,065 taking 282 MB, and the import takes about 95 s instead of 154 s.
- **Reads**: the first read from a pack also fetches the pack's index from its end (and the whole pack when it is under 64 KB), so the other objects of the same commit need no further lookups.
- **What stays loose**: objects over 1 MB, and an object written on its own. Objects written before packs keep being read where they are.
- **Cost**: a row per packed object in the metadata store (13 MB of PostgreSQL for starhub-server), and range reads (`GET` with `Range`), which every S3-compatible service supports.
- **Redaction**: purging a packed object rewrites its pack without it.
- **Turning it off**: `SCION_BLOB_PACKS=off` writes every new object loose. Packed objects stay readable.

## Repacking

`scion admin repack` rewrites all reachable objects into a few large packs, storing older versions of each file and directory as deltas against the next newer version, then removes the packs and loose objects this superseded. It is Scion's `git gc`: run it now and then (after a large import, or nightly). It is safe while the server runs.

```sh
scion admin repack --meta postgres://… --blobs s3://scion-objects   # --dry-run to only count
```

- **Deltas**: an older version is stored as zstd with the newer version as a reference (what `zstd --patch-from` does). The newest versions, which are read most, stay whole. Chains are at most 50 long, as in git; reads follow them and cache the bases they use.
- **Effect** on starhub-server (6,338 commits, PostgreSQL + MinIO): 67,847 objects in 1 pack of 27 MB, 51,262 of them deltas, in 24 s. MinIO goes from 283 MB to 86 MB (the rest is three committed binaries over 8 MB, which stay loose); Gitaly's repository for the same history is 82 MB.
- **Removing what it superseded**: packs none of whose objects live there any more, once older than `--prune-after` (default 1 hour, so that packs being written are not touched), and loose objects that are also packed.
- **Reads** fetch large packs in 1 MiB blocks and keep 128 MiB of them, so a commit's objects and the versions of a delta chain mostly come from memory.
- **Metadata**: the repack rewrites every pack location. On PostgreSQL it then compacts the metadata. It rewrites the pack locations table in place with `VACUUM FULL` when the table is under 256 MB, which locks it for about a second at most and is skipped if the lock is not granted within 200 ms; larger tables are only vacuumed, so later writes reuse their space. It also rebuilds the indexes over object ids and paths with `REINDEX CONCURRENTLY`, which does not block reads or writes. For starhub-server this takes the database from 68 MB to 54 MB.
- **Redaction**: purging an object that others are stored against first rewrites those others whole.

## 连接参数

| 环境变量 | 说明 | 默认 |
|---|---|---|
| `SCION_S3_ENDPOINT` | 服务地址，如 `http://minio:9000`。访问 AWS S3 时留空 | 无 |
| `SCION_S3_PROVIDER` | `auto`、`minio`、`seaweedfs`、`aws`、`generic` | `auto` |
| `SCION_S3_ACCESS_KEY_ID` / `SCION_S3_SECRET_ACCESS_KEY` | 访问密钥；未设置时读 `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` | 无 |
| `SCION_S3_REGION` | 区域；未设置时读 `AWS_REGION` | `us-east-1` |
| `SCION_S3_VIRTUAL_HOSTED` | `true` 时用 `bucket.host` 风格 | 按服务决定，只有 AWS 默认为 `true` |
| `SCION_S3_MAX_RETRIES` | 单个请求的最大重试次数 | 10 |

`s3://` 后面的 `/prefix` 用来让多个部署共用一个 bucket。

## 服务适配

启动时服务端会先检查对象存储，任何一步失败都会拒绝启动，并说明原因：

1. **识别服务**：读 endpoint 根路径返回的 `Server` 响应头，`MinIO` 识别为 MinIO，`SeaweedFS …` 识别为 SeaweedFS，`AmazonS3` 识别为 AWS。
   - `SCION_S3_PROVIDER=auto` 时直接采用识别结果。
   - 显式配置的服务与识别结果不一致时报错，多半是 endpoint 写错了。
   - 前面有反向代理、识别不出时，以配置为准。
2. **健康检查**：MinIO 检查 `/minio/health/ready`，SeaweedFS 检查 `/healthz`。服务还没就绪时，报错中会带上返回的状态码。
3. **bucket 检查**：bucket 不存在时，报错中给出该服务的创建方法。
4. **读写自检**：写入、读回并校验一个探测对象。用于发现密钥错误、权限不足等问题。

启动日志会记录识别结果，例如：

```
object store ready backend=s3 (minio, Server: MinIO) bucket=scion-objects
```

| | SeaweedFS | MinIO |
|---|---|---|
| 识别依据（`Server` 响应头） | `SeaweedFS <版本>` | `MinIO` |
| 健康检查 | `/healthz` | `/minio/health/ready` |
| 创建 bucket | `echo 's3.bucket.create -name B' \| weed shell` | `mc mb <alias>/B`，或容器环境变量 `MINIO_DEFAULT_BUCKETS=B` |
| 请求风格 | path | path |
| 已测试版本 | 4.48 | 2023.12.23（bitnami 镜像，与 starhub / CSGHub 部署一致） |

## SeaweedFS

开发环境默认使用 SeaweedFS。MinIO 开源版已归档、停止维护，SeaweedFS 是仍在维护的 Apache 2.0 项目。

```sh
docker compose -f deploy/dev/docker-compose.yml up -d s3 s3-init
SCION_S3_ENDPOINT=http://localhost:8333 \
SCION_S3_ACCESS_KEY_ID=scion SCION_S3_SECRET_ACCESS_KEY=scion-dev-secret \
SCION_S3_PROVIDER=seaweedfs \
scion server --meta postgres://scion:scion-dev-secret@localhost:5432/scion --blobs s3://scion-objects
```

## MinIO

接入 starhub / CSGHub 时，可以直接使用它们现有的 MinIO：

```sh
docker compose -f deploy/dev/docker-compose.yml --profile minio up -d minio
SCION_S3_ENDPOINT=http://localhost:9000 \
SCION_S3_ACCESS_KEY_ID=scion SCION_S3_SECRET_ACCESS_KEY=scion-dev-secret \
SCION_S3_PROVIDER=minio \
scion server --meta postgres://scion:scion-dev-secret@localhost:5432/scion --blobs s3://scion-objects/scion
```

与其他服务共用 MinIO 时，建议给 Scion 单独的 bucket 或前缀，并只授予该范围的读写权限。

## 代理

请求遵循系统代理设置（`HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY`）。对象存储在内网或本机时，记得把它的地址加入 `NO_PROXY`，例如 `NO_PROXY=127.0.0.1,localhost,minio`。

## 测试

对象存储测试通过环境变量指向真实服务，未设置时跳过：

```sh
SCION_TEST_S3_ENDPOINT=http://127.0.0.1:9000 SCION_TEST_S3_BUCKET=scion-test \
SCION_TEST_S3_ACCESS_KEY=scion SCION_TEST_S3_SECRET_KEY=scion-dev-secret \
SCION_TEST_S3_PROVIDER=minio \
cargo test -p scion-store-blob -p scion
```

`SCION_TEST_S3_PROVIDER` 用来校验识别结果，可以不设置。
