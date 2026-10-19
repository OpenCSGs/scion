# Scion

[English](README.md) | 简体中文

服务端优先的源码管理系统：历史、合并、rebase 都在服务端完成，客户端始终在线、按需取数。协议与语言无关，官方提供 Rust 服务端 `scion` 和 Rust 客户端 `scn`。

![Scion standalone architecture](docs/design/images/arch-01-standalone.en.png)

<sub>架构图源文件：[`arch-01-standalone.en.svg`](docs/design/images/arch-01-standalone.en.svg)（英文）· [`arch-01-standalone.svg`](docs/design/images/arch-01-standalone.svg)（中文）。图中包含规划中的组件（L2 缓存、deriver、git-gateway 等），当前进度见下文。</sub>

## 文档

**使用指南**
- [部署](docs/guide/deploy.md)：Docker 镜像，以及 PostgreSQL + S3 兼容存储的 docker compose
- [从 git 迁移](docs/guide/coming-from-git.md)（英文）：git 命令与 scn 的对照、版本号写法（`HEAD~1`、`A..B`）
- [仓库与分支管理](docs/guide/repos.md)：创建、列出、fork、删除与恢复仓库，管理分支
- [ref 变化事件](docs/guide/events.md)：按序号、不遗漏的 ref 变化事件流，推送到 webhook 与 NATS JetStream
- [打包下载](docs/guide/archive.md)：用 `scn archive` 把某个版本或目录打包成 tar.gz、zip 或 tar
- [文件列表页](docs/guide/browse.md)：目录中每个条目的最后提交、文件数与大小
- [git 兼容的提交 sha](docs/guide/git-sha.md)：每个提交都有 40 位 git sha，可以直接当作版本号使用
- [从 git 导入](docs/guide/git-import.md)：把已有的 git 仓库连同全部历史导入 Scion
- [稀疏检出](docs/guide/sparse-checkout.md)：只检出需要的目录，其余内容不会被拉取
- [对象存储](docs/guide/object-storage.md)：SeaweedFS、MinIO 与其他 S3 兼容服务；服务识别与启动检查
- [多节点部署与合入](docs/guide/cluster.md)：合入语义（rebase、恰好一次）、`--advertise-url` 集群模式、故障切换与过载保护
- [指标与追踪](docs/guide/observability.md)：Prometheus 指标、OpenTelemetry 追踪与常用查询
- [缓存](docs/guide/cache.md)：区域缓存 `scion cache`（分片、对冲、预热）与本机 daemon `scn daemon`
- [认证与鉴权](docs/guide/auth.md)：令牌、访问控制策略、OIDC 登录、外部签发方、OPA 兼容的鉴权回调、接入 starhub-server
- [Redaction 与数据校验](docs/guide/redaction.md)：清除泄露的内容（redact、purge）、`scion admin fsck`、`scion admin lease ls`

**设计与规范**
- 设计文档：[服务端](docs/design/rust-server-architecture.md) · [客户端](docs/design/rust-client-architecture.md) · [接入 starhub / CSGLite](docs/design/starhub-integration.md) · [路线图](docs/design/roadmap.md)（英文；已规划、暂不做的大任务，例如虚拟化检出）
- 对象格式规范：[`SPEC.md`](SPEC.md)，golden 测试向量在 [`testdata/vectors/`](testdata/vectors/)
- 协议：[`proto/scion/v1/`](proto/scion/v1/)

当前进度：**M5 生产化**进行中，已完成：
- **认证与鉴权**：
  - 令牌：自签 ES256 访问令牌、PAT、刷新令牌；
  - 外部身份（只用标准协议）：JWKS 或 HS256 签发方、RFC 7662 内省、OPA 兼容的鉴权回调。接入 starhub-server 只需配置；
  - ACL：按主体、仓库、动作、ref、路径授予权限；
  - `scn login`：OIDC device flow，凭据存钥匙串。
- **Redaction**：
  - redact 泄露的文件内容：对象 id 与历史不变，读取返回 `REDACTED`；
  - 区域缓存和本机 daemon 10 秒内停止返回；
  - 合入时不会把 redact 过的内容合并进新提交；
  - purge 在宽限期后物理删除字节，前提是所有含该对象的仓库都已 redact。
- **运维**：`scion admin fsck` 对照对象索引与对象存储校验 ref、提交、目录树和文件；`scion admin lease ls` 列出租约。
- **稀疏检出**：`scn clone --sparse a/,b/` 与 `scn sparse add|rm|set|disable`。范围外的子树不会被拉取，提交、合入和 rebase 都不影响服务端上范围外的内容。

- **可观测性与限流**：
  - `--metrics-listen` 提供 Prometheus 指标：各 RPC 的请求数、状态码与延迟，合入批次与延迟，L2 和 L3 的缓存命中率，被拒绝的请求；
  - OpenTelemetry 追踪（OTLP），合入转发和缓存回源时接续调用方的 `traceparent`；
  - 按调用者的速率与在途请求上限（`--tenant-rate`、`--tenant-concurrency`）；
  - deadline：取客户端 `grpc-timeout` 与 `--max-request-timeout` 中较小的一个，并传给 leader 和服务端。

- **仓库与分支管理**：`RepoService`（创建、列出、修改、软删除并可在宽限期内恢复、fork），以及分支的创建、移动与删除；命令行为 `scn repo`、`scn branch`。
- **部署**：包含 `scion` 和 `scn` 的 Docker 镜像，以及 PostgreSQL + SeaweedFS 的 docker compose。
- **ref 变化事件**：
  - 每次 ref 变化都与修改同一事务写入一条按序号排列的事件；
  - 消费方可以用 `EventService.ListRefEvents` 按序号拉取；
  - 也可以由服务端把带提交与路径级改动的事件推送到 webhook（HMAC 签名）或 NATS JetStream，按顺序、至少送达一次。
- **合入前检查**：合入或写分支之前，服务端可以把要引入的提交交给外部服务，由它允许或拒绝（检查服务不可用时默认拒绝）。

M5 后续还有 YugabyteDB、Helm 与混沌测试。

**M4 缓存层**已完成：
- **区域缓存 `scion cache`（L2）**：
  - 存储：foyer 内存加 NVMe；
  - 多节点按 rendezvous 哈希分片，整个区域对同一对象只回源一次；
  - 并发未命中合并（single-flight），回源时对冲与熔断；
  - 按 WatchRefs 预热，只沿变化的目录树下钻。
- **本机 daemon `scn daemon`（L1）**：
  - 校验过的本机对象缓存；
  - 其余 RPC 透明转发；
  - 预取工作副本跟随的 ref；
  - `scn` 按需自动拉起。
- **WatchRefs**：订阅 ref 变化（先快照，之后每次前进），断开后可续上。
- **服务端读路径**：
  - 进程内对象缓存；
  - 合并并发读并限制后端并发；
  - `--read-endpoint` 向客户端推荐缓存。

缓存 99.5% 的读取不回源，单节点每秒 153 万个对象，详见下文「性能」。

之前的里程碑：
- M3 实现了服务端 rebase、恰好一次合入、批量合入（单 ref 每秒 885 次）、集群与过载保护；
- M2 实现了历史查询（10 万提交 p99 低于 20 ms）；
- M1 实现了写入路径、Lite 模式、`scn` 工作副本与 git 导入。

M5 剩余工作：YugabyteDB 档、Helm 与发布构建。

## 性能

基准在一台机器上运行（Apple M5，10 核），对象放在内存里，客户端、缓存和服务端在同一台机器。每一行都链接了对应的基准测试，可以自行复现（`cargo test --release -p <crate> --test bench -- --ignored --nocapture`）。

| 方面 | 负载 | 结果 | 目标 |
|---|---|---|---|
| 缓存命中率（[`server/cache/tests/bench.rs`](server/cache/tests/bench.rs)） | 冷缓存，200 个客户端检出同一组 20000 个对象（1–4 KiB） | 99.5% 的读取不回源 L3；每个对象只从 L3 取一次 | ≥ 99% |
| 缓存吞吐（同上） | 热缓存，64 个客户端，gRPC 每批 64 个对象 | 单节点每秒 153 万个对象 | ≥ 每秒 20 万 |
| 缓存吞吐（同上） | 热缓存，64 个客户端，每次请求 1 个对象 | 单节点每秒 16.4 万次请求 | — |
| 检出（[`client/daemon/tests/bench.rs`](client/daemon/tests/bench.rs)） | `scn clone` 1000 个目录、10 万个文件 | 经热 daemon 5.3 秒；本机磁盘上不经网络直接创建这些文件需要 5.4 秒，daemon 没有可测出的额外开销 | < 5 秒（受磁盘限制） |
| 合入（[`server/land/tests/bench.rs`](server/land/tests/bench.rs)） | 200 个并发客户端向同一个 ref 合入 2000 次 | PostgreSQL 每秒 885 次（p99 379 ms）；SQLite 每秒 1921 次（p99 173 ms） | 每秒 ≥ 200 次，p99 < 500 ms |
| 历史查询（[`server/query/tests/bench.rs`](server/query/tests/bench.rs)） | 10 万提交：log、diff、祖先判断、merge-base、blame | SQLite 与 PostgreSQL 下 p99 均低于 20 ms（冷 blame 13.6 ms，`log --path` 15.6 ms） | p99 < 100 ms |
| 文件列表页（同一基准） | 10 万提交：根目录 100 个条目各自的最后提交、仓库统计 | PostgreSQL：冷查询 p50 33 ms（p99 93 ms），合入后父提交已缓存时 p99 2.4 ms，命中缓存 p99 < 5 ms；仓库统计冷查询 131 ms，下一个提交 3.1 ms | p99 < 100 ms |

## 目录结构

```
proto/scion/v1/      协议定义（.proto），语言无关，buf lint / breaking 检查
SPEC.md              对象模型与规范编码
testdata/vectors/    golden 测试向量，任何语言的实现都应通过

common/              服务端和客户端共用（只能依赖 common）
  protocol/          scion-proto   由 proto 生成的 Rust 代码
  object/            scion-object  ObjectId、规范编码、Tree / Changeset（纯计算，无 IO）
  flight/            scion-flight  合并并发请求（single-flight），服务端缓存与 daemon 共用

server/              服务端（可依赖 common 与更低层的 server crate）
  obs/               scion-obs         Prometheus 指标、OpenTelemetry 追踪、gRPC deadline
  store/             scion-store       MetaStore / BlobStore trait、内存实现、后端一致性测试套件
  store-sql/         scion-store-sql   SQLite / PostgreSQL 元数据存储
  store-blob/        scion-store-blob  本地目录 / S3 兼容对象存储
  auth/              scion-auth        令牌、外部签发方（JWKS / HS256）、内省、OIDC、ACL
  tree/              scion-tree        增量建树
  merge/             scion-merge       服务端 rebase：把 delta 重放到更新的目录树上，三方合并
  land/              scion-land        创建提交、提交栈 rebase、LandCoordinator、租约
  query/             scion-query       历史查询（log / diff / blame / merge-base）与重建索引
  cache/             scion-cache       区域对象缓存（scion cache）：foyer、rendezvous、对冲、预热
  api/               scion-api         gRPC 服务实现与启动入口
  scion/             scion             服务端二进制（scion server / scion cache / scion admin）

client/              客户端（只能依赖 common 与 client，禁止依赖 server）
  sdk/               scion-client  Rust SDK
  wc/                scion-wc      工作副本（clone / status / commit / land / sync）
  daemon/            scion-daemon  本机 L1 缓存（scn daemon）：对象缓存、透明转发、预取
  gitimport/         scion-gitimport  git 历史导入
  cli/               scion-cli     命令行二进制 scn

xtask/               仓库维护任务（cargo xtask layers：依赖方向检查）
deploy/dev/          本地开发依赖（SeaweedFS、PostgreSQL）
docs/guide/          使用指南
docs/design/         设计文档与架构图
```

目录名表示位置和职责；crate 名统一带 `scion-` 前缀，避免与 crates.io 上的同名库（如 `object`）冲突。依赖方向由 `cargo xtask layers` 在 CI 中强制检查。

## 快速开始

```sh
# 方式一：docker compose，PostgreSQL + S3 兼容存储（见 docs/guide/deploy.md；国内可加 REGISTRY=docker.m.daocloud.io）
docker compose -f deploy/compose/docker-compose.yml up -d --build

# 方式二：从源码启动 Lite 档（SQLite + 本地目录，数据落盘）
cargo run -p scion -- server --lite --data-dir ./.scion-data

# 其它后端：--meta mem|sqlite://PATH|postgres://…  --blobs mem|DIR|s3://bucket/prefix
# S3 连接参数：SCION_S3_ENDPOINT、SCION_S3_ACCESS_KEY_ID、SCION_S3_SECRET_ACCESS_KEY、SCION_S3_REGION、
# SCION_S3_PROVIDER=auto|minio|seaweedfs|aws|generic（CI 中测试 SeaweedFS 与 MinIO，详见 docs/guide/object-storage.md）

# 另一个终端：安装 scn 并检查连接
cargo install --path client/cli
scn doctor
scn config set user.name "Ada Lovelace"       # 默认的提交作者（未设置时用 git 的 user.name / user.email）
scn config set user.email ada@example.com
scn repo create mono                # 仓库与分支：scn repo / scn branch（见 docs/guide/repos.md）

# 日常流程
scn clone mono && cd mono          # 检出（默认 ref main，空仓库也可以）
echo 'hello' > README.md
echo 'target/' > .gitignore         # 支持 .gitignore 与 .scionignore（gitignore 语法）
scn status                          # A .gitignore, A README.md
scn commit -m "add readme"          # 或 --author "Ada <ada@example.com>"
scn restore src/main.rs             # 放弃已跟踪文件的本地改动（`.` 表示全部）；新增的文件保留
scn commit --amend                  # 把更多本地改动并入未合入的提交（保留说明与作者）
scn uncommit                        # 撤销未合入的提交，改动保留为本地改动
scn land                            # 合入 main；main 已前进时由服务端 rebase（三方合并）
scn sync                            # 拉取别人合入的改动；有未合入的提交时自动做文件级 rebase
scn tag v1.0                        # 给当前提交打标签；`scn tag` 列出，`scn log v1.0` 直接使用
scn switch -c feature               # 在当前快照创建分支并切换过去；`scn switch main` 切回
scn merge feature                   # 把分支合并为一个未合入的合并提交（按行三方合并），之后 `scn land`
# 冲突时（scn sync 或 scn merge）文件里写入 <<<<<<< / >>>>>>> 标记，`scn status` 显示 C：
scn resolve src/main.rs             # 去掉标记后标记为已解决；或用 --ours / --theirs 取某一方
scn commit                          # 完成，沿用原来的提交说明；`scn merge --abort` 放弃合并
scn cat main:README.md              # 不需要工作副本也能读文件（工作副本外加 --repo）
scn ls main:                        # 列目录；所有命令都支持 --json
scn log -n 10 -- src                # 历史，从新到旧，可按路径过滤（git 写法：HEAD~1、A..B）
scn diff                            # 本地未提交的改动（unified diff）
scn diff main                       # main 最新提交引入的改动；`scn diff A B` 比较两个版本
scn blame src/lib.rs                # 每一行最后被哪个提交改动
scn clone mono part --sparse src/   # 稀疏检出：只检出 src/ 和根目录下的文件；之后可以 `scn sparse add docs/`
scn archive -o mono.tar.gz          # 打包下载 tar.gz / zip / tar（见 docs/guide/archive.md）

# scn 经本机 daemon（scn daemon）连接服务端，按需自动拉起；SCION_NO_DAEMON=1 关闭
scn daemon status

# 在一个或多个服务端前部署区域缓存，并用 scion server --read-endpoint 推荐给客户端（详见 docs/guide/cache.md）
cargo run -p scion -- cache --upstream http://127.0.0.1:7080 --listen 127.0.0.1:7090 --warm mono:main

# 导入已有的 git 仓库（保留全部历史、作者与时间），详见 docs/guide/git-import.md
scn import git ~/src/my-repo --repo mono

# 升级已有数据的服务端后、启动之前：修复谱系并补建派生索引（可重复执行）
cargo run -p scion -- admin reindex --lite --data-dir ./.scion-data
```

## 开发

```sh
cargo test --workspace                          # 全部测试
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask layers                              # 依赖方向检查
cargo llvm-cov --workspace                      # 覆盖率（需 cargo install cargo-llvm-cov）
SCION_BLESS=1 cargo test -p scion-object --test vectors   # 新增 golden 向量后重新生成
docker compose -f deploy/dev/docker-compose.yml up -d     # SeaweedFS（S3）+ PostgreSQL
```

**测试要求**（[服务端设计文档 §12.1](docs/design/rust-server-architecture.md)）：每个含逻辑的函数都要有单元测试，覆盖正常路径、边界条件和每一个错误分支。核心 crate 行覆盖率不低于 90%，PR 新增代码不低于 90%，由 CI 强制执行。

## 许可证

Apache-2.0
