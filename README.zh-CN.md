# Scion

[English](README.md) | 简体中文

面向 monorepo、编程 agent 和 CI 的服务端优先的源码管理系统。

Scion 用 Rust 实现了 Anthropic 在 [Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control) 一文中提出的设计：历史、合并、rebase 都在服务端计算，客户端始终在线、按需获取不可变对象，检出时只拿任务需要的路径。项目提供服务端 `scion` 和客户端 `scn`，协议与语言无关，由 OpenCSG 为 [starhub-server](https://github.com/OpenCSGs/csghub-server) 和 CSGLite 开发。本项目不是 Anthropic 的实现，与 Anthropic 没有关联。

![Scion standalone architecture](docs/design/images/arch-01-standalone.en.png)

<sub>架构图源文件：[`arch-01-standalone.en.svg`](docs/design/images/arch-01-standalone.en.svg)（英文）· [`arch-01-standalone.svg`](docs/design/images/arch-01-standalone.svg)（中文）。图中包含规划中的组件（deriver、git-gateway 等），已实现的部分见[里程碑](docs/design/milestones.md)。</sub>

## 性能对比

[测试程序](bench/openscion/src/main.rs)在 [OpenScion](https://github.com/tinyhumansai/openscion/blob/main/benchmarks/README.md) 的四个合成负载上，对比 Scion、Git，以及同一设计的另一个实现 [CarterT27/scion](https://github.com/CarterT27/scion)。三者在同一台机器上运行：Apple M5、16 GiB 内存、macOS 26.5.2、Git 2.54.0。CarterT27/scion 用当时 `main` 的代码构建，用它自己的测试程序运行。

每个文件是 4 KiB 伪随机内容，分布在 10 个目录里。1,000 个文件跑 7 次，10,000 个文件跑 5 次。单位是毫秒，取中位数，越小越好。

| 场景 | Git，1k 文件 | CarterT27，1k | Scion，1k | Git，10k 文件 | CarterT27，10k | Scion，10k |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 导入目录 | 375.1 | 32.8 | **29.6** | 3,342.0 | 413.5 | **362.1** |
| 提交一个改动的文件 | 29.7 | 29.2 | **13.4** | 226.7 | 142.5 | **42.2** |
| 完整克隆 / 检出 | 251.3 | 115.8 | **50.7** | 2,316.6 | 1,047.3 | **568.3** |
| 只克隆 / 检出一个目录 | 65.0 | 25.0 | **19.4** | 181.7 | 123.1 | **48.6** |

Scion 在每一项都是三者中最快的：比 Git 快 2.2–13 倍；和 CarterT27/scion 相比，检出快 1.3–2.5 倍，提交快 2.2–3.4 倍，导入快 11–14%。

- **导入：** Git 执行 `init`、`add -A` 和 `commit`。Scion 每批 256 个上传文件，再通过写入 API 创建一个没有父提交的 changeset 和一个 ref。
- **提交：** 在完整工作副本里改一个文件的一个字节，然后计时 Git 的 `commit -a`，或 Scion 的工作副本提交（包括扫描状态和上传）。两边都不 push 也不 land。
- **完整检出：** Git 的 `clone --no-local`；Scion 的工作副本克隆。
- **只检出一个目录：** Git 的 `clone --no-local --filter=blob:none --sparse` 加 `sparse-checkout set dir0000`；Scion 只克隆 `dir0000`。

两个实现都在进程内启动服务端，元数据和对象都在内存里；Scion 的对象和存到磁盘或 S3 时一样经过 zstd 压缩和打包。CarterT27/scion 的服务端开启了 TLS 和令牌认证，Scion 两者都没开，这会让 CarterT27 的检出多花百分之几的时间。生成测试文件和校验检出结果不计入时间。

```sh
(cd bench/openscion && cargo build --release)
bench/openscion/target/release/openscion-bench 100 7      # 1,000 个文件，跑 7 次
bench/openscion/target/release/openscion-bench 1000 5     # 10,000 个文件，跑 5 次
```

### 真实仓库

在 [pingdotgg/t3code](https://github.com/pingdotgg/t3code) 上，用 CarterT27/scion README 里测的同一个快照（5,573 个提交、28,068 个文件）。Scion 运行在 Docker 中的 PostgreSQL 和 MinIO 上，Git 用本地裸仓库：

| | Git | Scion | Scion ÷ Git | CarterT27 ÷ Git（其 README） |
| --- | ---: | ---: | ---: | ---: |
| 完整检出 | 2.65 秒 | **2.28 秒** | **0.86** | 5.2 |
| 检出 `apps/web`（1,521 个文件） | 0.172 秒 | 0.178 秒 | 1.03 | 4.6 |
| 干净工作区的 status | 98 ms | 153 ms | 1.6 | — |
| 导入完整历史 | — | 103 秒 + repack 42 秒 | — | 23 分 18 秒 |

导入后，Scion 用 247 MB 的 MinIO 加 90 MB 的 PostgreSQL 存下这份历史；从 GitHub 克隆下来的 pack 是 557 MB。每次检出都逐个文件和 Git 核对。

- [报告：CarterT27/scion 的测试](docs/reports/2026-10-20-openscion-workloads.md)（英文）：方法、全部数据和复现方式。
- [报告：starhub-server](docs/reports/2026-10-19-starhub-server.md)（英文）：在 starhub-server 历史上的全量真实测试，以及与 starhub 的 Gitaly + MinIO 方案、libgit2 的对比。
- [组件基准测试](docs/performance.md)（英文）：缓存、合入、历史查询、10 万文件的检出。

## 工作方式

- **写入在服务端。** 提交由服务端根据改动的文件创建；`land` 把分支移到这个提交上，分支已经前进时由服务端 rebase，恰好一次，不需要全局队列。同时向一个分支写入的人都会成功。
- **按需获取。** 客户端按 id 请求需要的 tree 和文件，所以稀疏检出不需要任何特殊支持：范围外的目录根本不会被拉取。
- **不用克隆就能查历史。** log、diff、blame、祖先判断、merge base、每个条目的最后提交、打包下载都是服务端查询。
- **三档存储。** Lite（SQLite + 本地目录）、Standard（PostgreSQL + S3 兼容存储），以及测试用的内存档。对象按内容寻址、zstd 压缩并打包；`scion admin repack` 把旧版本存成相对新版本的 delta。
- **缓存。** 区域缓存 `scion cache` 和本机 daemon `scn daemon` 提供和服务端相同的协议，客户端不需要知道缓存拓扑。
- **兼容 Git。** `scn import git` 连同历史导入已有仓库；每个提交都保留 git SHA，任何需要版本号的地方都可以用。
- **认证与 redaction。** 令牌、OIDC 登录、外部签发方、按仓库、ref 和路径的访问控制，以及不改写历史地清除泄露内容。

## 快速开始

```sh
# 以 Lite 模式（SQLite + 本地目录）从源码启动服务端
cargo run -p scion -- server --lite --data-dir ./.scion-data
# 或者用 PostgreSQL + S3 兼容存储：docker compose -f deploy/compose/docker-compose.yml up -d --build

# 在另一个终端
cargo install --path client/cli
scn repo create mono
scn clone mono && cd mono
echo hello > README.md
scn commit -m "add readme"
scn land                          # 把 main 移到这个提交；main 已前进时由服务端 rebase
scn log
```

[scion 与 scn 使用导览](docs/guide/cli.md)（英文）介绍日常命令，[从 git 迁移](docs/guide/coming-from-git.md)（英文）给出 git 命令与 scn 的对照。

## 文档

- **使用：** [部署](docs/guide/deploy.md) · [命令导览](docs/guide/cli.md) · [从 git 迁移](docs/guide/coming-from-git.md) · [从 git 导入](docs/guide/git-import.md) · [稀疏检出](docs/guide/sparse-checkout.md) · [仓库与分支](docs/guide/repos.md) · [打包下载](docs/guide/archive.md) · [文件列表页](docs/guide/browse.md) · [git SHA](docs/guide/git-sha.md)
- **运维：** [对象存储](docs/guide/object-storage.md) · [多节点与合入](docs/guide/cluster.md) · [缓存](docs/guide/cache.md) · [认证与鉴权](docs/guide/auth.md) · [redaction 与 fsck](docs/guide/redaction.md) · [ref 事件](docs/guide/events.md) · [指标与追踪](docs/guide/observability.md)
- **设计：** [服务端](docs/design/rust-server-architecture.md) · [客户端](docs/design/rust-client-architecture.md) · [接入 starhub / CSGLite](docs/design/starhub-integration.md) · 对象格式 [`SPEC.md`](SPEC.md) · 协议 [`proto/scion/v1/`](proto/scion/v1/)

## 进度

M1–M4 已完成：写入路径与工作副本、历史查询、服务端 rebase 与合入、集群，以及区域缓存和本机 daemon。M5 生产化进行中：认证、redaction、可观测性、ref 事件、打包与 repack 已完成，还剩 YugabyteDB 档、Helm 和发布构建。详情见[里程碑](docs/design/milestones.md)（英文），后续的大任务见[路线图](docs/design/roadmap.md)（英文）。

## 开发

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

[开发说明](docs/development.md)（英文）介绍目录结构、crate 之间的依赖规则和测试要求（新代码覆盖率至少 90%）。

## 许可证

Apache-2.0
