# 路线图：已规划、暂不做的大任务

这份文档收集**工作量大、但目前不是必需**的任务，避免它们散落在各个设计文档和 PR 讨论里。

- **进入条件**：每一项都写明做什么、为什么现在不做、什么时候该做，以及已知的设计要点。
- **开始实现时**：从这里移到对应的设计文档，开 PR，完成后把这里的条目删掉或标为已完成。

当前正在做的是**本地开发闭环**：忽略文件、撤销与修改提交、切换分支与合并、本地解决冲突。这些不在本文档里，完成情况见 README 的 Status。

图例：🔵 已有初步设计，⚪ 只有想法。

---

## 1. 客户端

### 1.1 虚拟化检出（`scion-vfs`）🔵
- **做什么**：工作副本以虚拟文件系统挂载，目录和文件元数据立即可见，文件内容在第一次读取时才下载（`cat`、`sed`、编辑器打开时自动拉取）。
  - Linux 用 FUSE（`fuser` crate）；
  - macOS 用 NFSv3 loopback（与 EdenFS 相同，不需要内核扩展），以后可以评估 FSKit；
  - Windows 用 ProjFS。
- **现状**：clone 时下载检出范围内的全部文件。范围之外的文件可以用稀疏检出（`scn sparse add`）或 `scn cat` 获取。
- **为什么现在不做**：工作量最大的单项（每个平台一套实现，加上缓存一致性、写入时转为真实文件、崩溃恢复）。稀疏检出已经覆盖“只要一部分目录”的主要场景。
- **什么时候做**：出现稀疏检出也解决不了的超大单仓库（百万级文件、开发者不确定要哪些目录），或者 Agent 需要秒级打开任意版本。
- **设计要点**：
  - 读取路径复用 daemon 的 L1 缓存和 `GetObjects`；
  - 写入时把文件转为本地实体文件（copy-on-write），dirstate 只追踪这些文件；
  - `status` 只需比较实体文件；
  - 与稀疏检出共用范围定义；
  - 见[客户端设计文档](rust-client-architecture.md) M7。

### 1.2 文件监听模式的 status 🔵
- **做什么**：daemon 监听工作副本（Linux inotify / fanotify，macOS FSEvents），`scn status` 直接读变化列表，不再遍历目录。
- **为什么现在不做**：stat 缓存在几万个文件以内已经足够快（本仓库的 status 几乎不耗时）。
- **什么时候做**：检出范围内文件超过约 10 万个，或 status 超过 200 ms。
- **设计要点**：监听丢事件（溢出、daemon 重启）时退回全量扫描；见客户端设计文档 M6。

### 1.3 daemon 托管与指标 ⚪
- **做什么**：
  - `scn daemon install` 注册为 systemd user service 或 launchd agent；
  - daemon 提供 `/metrics`（命中率、缓存大小、回源延迟）。
- **什么时候做**：发布各平台安装包时一起做（见 3.2）。

### 1.4 SDK 传递 traceparent ⚪
- **做什么**：SDK 和 CLI 在请求中带上 W3C `traceparent`，把客户端操作和服务端追踪串起来。
- **什么时候做**：排查客户端到服务端的延迟问题时。

---

## 2. 服务端

### 2.1 M5d：YugabyteDB 与档位迁移 🔵
- **做什么**：
  - Distributed 档（YugabyteDB）通过同一套 sqlx 代码运行，事务冲突的重试按 Dialect 统一处理；
  - `scion admin migrate-meta --from … --to …`：SQLite → PostgreSQL → YugabyteDB；
  - `scion admin blob-sync`：本地文件系统到 S3；
  - 每晚在 YugabyteDB 上跑 conformance 测试。
- **为什么现在不做**：单个 PostgreSQL 主库足以承受目前的写入规模（服务端设计文档 §「容量」）。
- **什么时候做**：需要多 AZ 写入，或单 PG 主库接近写入上限；接入 starhub 的规模变大时。

### 2.2 M5e：生产部署与验证 🔵
- **做什么**：
  - Helm chart；
  - 各平台发布构建（服务端镜像、`scn` 安装包）；
  - 跨实现的 conformance 套件；
  - 混沌测试：杀 L2、PG 主备切换、YugabyteDB leader 迁移、AZ 断网、S3 延迟注入；
  - AZ 故障演练。
- **什么时候做**：第一次生产部署之前。

### 2.3 通用 Deriver 框架与独立的 deriver 进程 🔵
- **做什么**：
  - 把现在分散在合入路径和查询路径里的派生数据（代数、跳表、PathHistory、Blame、GitSha、LastCommitPerEntry、TreeStats）统一成一个框架：声明依赖、按版本失效、批量回填；
  - 由独立进程跟随合入日志提前计算。
- **为什么现在不做**：按需计算加缓存已经够快（见 README Performance）。
- **什么时候做**：派生种类继续增加（LfsPointerIndex、Sha256、PathIndex），或者冷查询的延迟成为问题。

### 2.4 更多派生数据与查询 ⚪
- **`PathIndex`**：路径前缀搜索，供“跳转到文件”使用。
- **`LfsPointerIndex`**：仓库中的 LFS 指针文件（starhub 的 GetRepoLfsPointers）。
- **`Sha256`**：blob 的 sha256，供 LFS 和 HF 协议使用。
- **`GetTrees`**：一次返回多层目录树（递归列目录）。
- **按路径 log 跟踪重命名，blame 跨合并追溯**：现在都只沿第一父提交链，不识别重命名。
- **LastCommits 的路径游标**：同一代的索引行超过一页时，现在退回扫描（结果正确，只是较慢）。

### 2.5 git 兼容（git-gateway）🔵
- **做什么**：新角色 `scion git-gateway`，实现 git smart HTTP / SSH（info-refs、upload-pack、receive-pack）。
  - git 对象由 GitSha 派生数据按需生成；
  - push 时把 packfile 解析成 changeset，再执行合入。
- **为什么现在不做**：`scn` 已经覆盖读写；这是工作量最大的服务端任务。
- **什么时候做**：需要让 git 客户端透明访问 Scion 仓库时（starhub S3 阶段）。见 [starhub 接入设计](starhub-integration.md)。

### 2.6 服务端导入（ImportService）⚪
- **做什么**：服务端从 git 远端或本地路径增量导入，用于 mirror 和批量迁移。现在的导入在客户端（`scn import git`）。
- **什么时候做**：starhub S2（存量仓库迁移）。

### 2.7 原生大文件 ⚪
- **做什么**：
  - 大文件改为 Scion 原生的分块 blob（按需拉取、全局去重）；
  - `ExternalBlob` 类型直接引用对象存储中已有的 LFS 对象，迁移时不需要复制数据。
- **什么时候做**：starhub S4。

### 2.8 仓库管理的扩展 ⚪
- **仓库配额**：大小、文件数，基于 TreeStats。
- **仓库级事件**：创建、删除、fork 也进入事件流。
- **异步 fork**：超大仓库的 fork 改为后台任务。

### 2.9 缓存与多区域 ⚪
- **冷 ref 自动识别**：长期不访问的 ref 不再预热和预取。
- **敏感前缀的缓存隔离**：带路径权限的内容不进入共享缓存。
- **多区域部署**：写入集中到主区域，读取走本区域缓存；需要 Distributed 档。

---

## 3. 集成

### 3.1 接入 starhub-server 🔵
- **Scion 侧（S1）**：已完成，包括仓库管理、外部身份与鉴权、合入前检查、事件、GitSha、LastCommitPerEntry、TreeStats、Archive。
- **starhub 侧，还没开始**：
  - `storage_backend` 字段与 `RoutingGitServer`；
  - Go 适配层；
  - NATS 消费者；
  - `pre_land` / `introspect` / `authz` 内部接口；
  - 两边一起的端到端联调。
- 分阶段计划见 [starhub 接入设计](starhub-integration.md)。

### 3.2 CSGLite ⚪
- **做什么**：CSGLite 内嵌 Lite 档（SQLite + 本地对象）。
- 见 [starhub 接入设计 §8](starhub-integration.md)。
