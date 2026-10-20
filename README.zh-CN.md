# Scion

[English](README.md) | 简体中文

面向 monorepo、编程 agent 和 CI 的服务端优先的源码管理系统。

Scion 用 Rust 实现了 Anthropic 在 [Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control) 一文中提出的设计：历史、合并、rebase 都在服务端计算，客户端始终在线、按需获取不可变对象，检出时只拿任务需要的路径。项目提供服务端 `scion` 和客户端 `scn`，协议与语言无关，由 OpenCSG 为 [starhub-server](https://github.com/OpenCSGs/csghub-server) 和 CSGLite 开发。本项目不是 Anthropic 的实现，与 Anthropic 没有关联。

![Scion standalone architecture](docs/design/images/arch-01-standalone.en.png)


## 性能对比

[测试程序](bench/openscion/src/main.rs)在 [OpenScion](https://github.com/tinyhumansai/openscion/blob/main/benchmarks/README.md) 的四个合成负载上，对比 CSG Scion、Git，以及同一设计的另一个实现 [CarterT27/scion](https://github.com/CarterT27/scion)。三者在同一台机器上运行：Apple M5、16 GiB 内存、macOS 26.5.2、Git 2.54.0。CarterT27/scion 用当时 `main` 的代码构建，用它自己的测试程序运行。

每个文件是 4 KiB 伪随机内容，分布在 10 个目录里。1,000 个文件跑 7 次，10,000 个文件跑 5 次。单位是毫秒，取中位数，越小越好。

| 场景 | Git，1k 文件 | CarterT27，1k | CSG Scion，1k | Git，10k 文件 | CarterT27，10k | CSG Scion，10k |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 导入目录 | 365.0 | 32.8 | **30.6** | 3,364.4 | 413.5 | **354.9** |
| 提交一个改动的文件 | 28.4 | 29.2 | **13.9** | 250.8 | 142.5 | **35.5** |
| 完整克隆 / 检出 | 243.0 | 115.8 | **52.2** | 2,361.9 | 1,047.3 | **589.4** |
| 只克隆 / 检出一个目录 | 60.3 | 25.0 | **23.1** | 172.9 | 123.1 | **49.5** |

CSG Scion 在每一项都是三者中最快的：比 Git 快 2–12 倍；和 CarterT27/scion 相比，检出快 1.1–2.5 倍，提交快 2.1–4 倍，导入快 7–17%。

- **导入：** Git 执行 `init`、`add -A` 和 `commit`。CSG Scion 每批 256 个上传文件，再通过写入 API 创建一个没有父提交的 changeset 和一个 ref。
- **提交：** 在完整工作副本里改一个文件的一个字节，然后计时 Git 的 `commit -a`，或 CSG Scion 的工作副本提交（包括扫描状态和上传）。两边都不 push 也不 land。
- **完整检出：** Git 的 `clone --no-local`；CSG Scion 的工作副本克隆。
- **只检出一个目录：** Git 的 `clone --no-local --filter=blob:none --sparse` 加 `sparse-checkout set dir0000`；CSG Scion 只克隆 `dir0000`。

两个实现都在进程内启动服务端，元数据和对象都在内存里；CSG Scion 的对象和存到磁盘或 S3 时一样经过 zstd 压缩和打包。两边的服务端都要求令牌认证：客户端用长期令牌换取短期访问令牌，每个请求都带上它。CarterT27/scion 还开了 TLS，CSG Scion 的进程内服务端没有开（部署时由前面的代理终止 TLS）；关掉认证时 CSG Scion 的时间变化不到 4 ms。生成测试文件和校验检出结果不计入时间。

```sh
(cd bench/openscion && cargo build --release)
bench/openscion/target/release/openscion-bench 100 7 objects auth      # 1,000 个文件，跑 7 次
bench/openscion/target/release/openscion-bench 1000 5 objects auth     # 10,000 个文件，跑 5 次
```

### 真实仓库

[pingdotgg/t3code](https://github.com/pingdotgg/t3code)，用 CarterT27/scion README 里测的同一个快照：5,573 个提交、28,068 个文件。CSG Scion 在上面那台 M5 上运行，后端是 Docker 中的 PostgreSQL 和 MinIO，Git 用本地裸仓库。M4 Pro 两列是 CarterT27/scion README 里公布的数字（CockroachDB + MinIO），没有在这里重跑。

| 场景 | Git，M5 | CSG Scion，M5 | Git，M4 Pro | CarterT27，M4 Pro |
| --- | ---: | ---: | ---: | ---: |
| 完整检出（28,068 个文件） | 2.13 秒 | 2.20 秒 | 2.90 秒 | 14.96 秒 |
| 检出 `apps/web`（1,521 个文件） | 0.173 秒 | 0.178 秒 | 0.21 秒 | 0.97 秒 |
| 干净工作区的 status | 98 ms | **75 ms** | — | — |
| 完整历史：Git 克隆、Scion 导入 | 3.96 秒 | 103 秒 + repack 42 秒 | 3.56 秒 | 23 分 18 秒 |
| 历史占用的存储 | pack 557 MB | MinIO 247 MB + PostgreSQL 90 MB | — | — |

和同一台机器上的 Git 相比，CSG Scion 完整检出和检出 `apps/web` 的用时都是 Git 的 1.03 倍；CarterT27/scion 分别是 5.2 倍和 4.6 倍。CSG Scion 的干净工作区 status 用时是 Git 的 0.77 倍。Scion 的导入要在服务端构建并存下每一个提交，所以拿来和另一个实现的导入比，而不是和 Git 的克隆比；CarterT27/scion 没有公布 status 和存储的数字。每次检出都逐个文件和 Git 核对；Git 的 pack 是从 GitHub 直接克隆下来的，没有重新打包。

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

**熟悉 git？** 大多数 git 命令在 scn 里可以照原样输入，从 `scn clone http://host/REPO` 开始，完整列表见 [scn 中的 git 命令](docs/guide/git-commands.zh-CN.md)。

[scion 与 scn 使用导览](docs/guide/cli.zh-CN.md)介绍日常命令，[从 git 迁移](docs/guide/coming-from-git.zh-CN.md)说明两者模型上的区别。

## 部署

有三种运行方式：在本机运行 release 二进制、在单节点上运行容器镜像，或者多节点接 PostgreSQL 和 S3。

镜像 `opencsg-registry.cn-beijing.cr.aliyuncs.com/opencsghq/scion`（标签 `latest` 和 `0.1.0`）同时构建了 `linux/amd64` 和 `linux/arm64`，可以运行在 Linux 服务器上，也可以运行在 Apple 芯片和 Intel 芯片 Mac 的 Docker Desktop 里。镜像包含服务端 `scion` 和客户端 `scn`，以非特权用户运行，默认要求认证。

**本机部署（release 二进制，不用 Docker）。** 每个 [release](https://github.com/OpenCSGs/scion/releases) 都提供服务端 `scion-server-<版本>-<平台>.tar.gz` 和客户端 `scn-<版本>-<平台>.tar.gz`，平台有 `x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`（需要 glibc 2.35 及以上）、`aarch64-apple-darwin` 和 `x86_64-apple-darwin`，并附 `SHA256SUMS`。

```sh
V=v0.1.0 T=aarch64-apple-darwin     # 或 x86_64-apple-darwin、x86_64-unknown-linux-gnu、aarch64-unknown-linux-gnu
for p in scion-server scn; do
  curl -fsSLO https://github.com/OpenCSGs/scion/releases/download/$V/$p-$V-$T.tar.gz && tar -xzf $p-$V-$T.tar.gz
done
sudo install scion-server-$V-$T/scion scn-$V-$T/scn /usr/local/bin/

scion server --lite --data-dir ~/.scion-data &       # 监听 127.0.0.1:7080；回环地址上不需要令牌
scn repo create http://127.0.0.1:7080/mono
scn clone http://127.0.0.1:7080/mono && cd mono
```

要在这台机器上给团队提供服务，加上 `--listen 0.0.0.0:7080 --auth-required`，并按下文创建第一个管理员：`scion admin policy set --lite --data-dir ~/.scion-data acl.toml` 和 `scion admin token create --lite --data-dir ~/.scion-data --subject admin --group admins`。

**Docker 单节点（Lite 档：SQLite + 本地目录）。**

```sh
docker run -d --name scion -p 7080:7080 -v scion-data:/data opencsg-registry.cn-beijing.cr.aliyuncs.com/opencsghq/scion:latest

# 第一个管理员：先写入策略（admins 组管理一切；任何已登录的身份都可以读写、合入）
docker exec scion sh -c 'printf "%s\n" "[[rule]]" "subjects = [\"group:admins\"]" "repos = [\"*\"]" "actions = [\"admin\"]" \
  "[[rule]]" "subjects = [\"*\"]" "repos = [\"*\"]" "actions = [\"land\"]" > /data/acl.toml \
  && scion admin policy set --lite /data/acl.toml'
# 再签发令牌，令牌只显示这一次
docker exec scion scion admin token create --lite --subject admin --group admins

scn login http://localhost:7080 --token scn_pat_…    # 策略在 30 秒内生效
scn repo create http://localhost:7080/mono
scn clone http://localhost:7080/mono && cd mono     # 进入目录后不用再写地址
```

**生产环境（Standard 档：PostgreSQL + S3 兼容存储）。** 用环境变量指定两者；上面的管理命令去掉 `--lite` 即可：

```sh
docker run -d --name scion -p 7080:7080 \
  -e SCION_META=postgres://scion:PASSWORD@db:5432/scion \
  -e SCION_BLOBS=s3://scion-objects -e SCION_S3_ENDPOINT=https://s3.example.com \
  -e SCION_S3_ACCESS_KEY_ID=… -e SCION_S3_SECRET_ACCESS_KEY=… \
  opencsg-registry.cn-beijing.cr.aliyuncs.com/opencsghq/scion:latest --log-format json server --listen 0.0.0.0:7080 --auth-required
```

多个节点放在负载均衡后面，共用一个数据库和一个 bucket（见[多节点](docs/guide/cluster.zh-CN.md)）；区域缓存用同一个镜像运行 `scion cache`（见[缓存](docs/guide/cache.zh-CN.md)）。想在一台机器上试用全部组件，可以用 [`deploy/compose`](deploy/compose/docker-compose.yml)：它会启动 PostgreSQL、SeaweedFS 和不带认证的服务端。服务端监听在回环地址以外时，没有 `--auth-required` 就拒绝启动，除非显式加上 `--allow-anonymous`。端口、健康检查、OIDC 登录以及 starhub-server 等外部身份提供方，见[部署](docs/guide/deploy.zh-CN.md)和[认证](docs/guide/auth.zh-CN.md)。

## 文档

- **使用：** [部署](docs/guide/deploy.zh-CN.md) · [命令导览](docs/guide/cli.zh-CN.md) · [scn 中的 git 命令](docs/guide/git-commands.zh-CN.md) · [从 git 迁移](docs/guide/coming-from-git.zh-CN.md) · [从 git 导入](docs/guide/git-import.zh-CN.md) · [稀疏检出](docs/guide/sparse-checkout.zh-CN.md) · [仓库与分支](docs/guide/repos.zh-CN.md) · [打包下载](docs/guide/archive.zh-CN.md) · [文件列表页](docs/guide/browse.zh-CN.md) · [git SHA](docs/guide/git-sha.zh-CN.md)
- **运维：** [对象存储](docs/guide/object-storage.zh-CN.md) · [多节点与合入](docs/guide/cluster.zh-CN.md) · [缓存](docs/guide/cache.zh-CN.md) · [认证与鉴权](docs/guide/auth.zh-CN.md) · [redaction 与 fsck](docs/guide/redaction.zh-CN.md) · [ref 事件](docs/guide/events.zh-CN.md) · [指标与追踪](docs/guide/observability.zh-CN.md)
- **设计：** [服务端](docs/design/rust-server-architecture.zh-CN.md) · [客户端](docs/design/rust-client-architecture.zh-CN.md) · [接入 starhub / CSGLite](docs/design/starhub-integration.zh-CN.md) · 对象格式 [`SPEC.zh-CN.md`](SPEC.zh-CN.md) · 协议 [`proto/scion/v1/`](proto/scion/v1/)

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
