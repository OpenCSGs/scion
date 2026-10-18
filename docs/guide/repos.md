# 仓库与分支管理

仓库和分支可以在线创建与管理，不需要重启服务端。接口为 `RepoService`，以及 `RefService` 的 `CreateRef` / `UpdateRef` / `DeleteRef`，见 [`proto/scion/v1/repo.proto`](../../proto/scion/v1/repo.proto)。命令行为 `scn repo` 与 `scn branch`，所有命令都支持 `--json`。

## 仓库

```sh
scn repo create opencsg/qwen --description "Qwen 微调" [--default-branch main]
scn repo ls [opencsg/] [--limit 100]       # 只列出有读权限的仓库，按名字排序
scn repo info opencsg/qwen
scn repo set opencsg/qwen --default-branch dev --description "…"
scn repo fork opencsg/qwen alice/qwen      # 复制全部历史与分支
scn repo rm opencsg/qwen                   # 删除，宽限期内可以恢复
scn repo ls --deleted                      # 已删除、尚未清除的仓库
scn repo restore opencsg/qwen
```

仓库名是一段或多段 `[A-Za-z0-9._-]+`，用 `/` 分隔（与 CSGHub 的 `namespace/name` 一致），最长 256 字节。`scion server --repo NAME` 仍然可以在启动时预建仓库。

### 权限

| 操作 | 需要的权限 |
|---|---|
| 创建、修改、删除、恢复 | 该仓库名上的 `admin`（ACL 的仓库通配同样适用，例如 `repos = ["alice/*"]` 允许 alice 在自己的命名空间下建仓库） |
| 查看属性、列出 | `read`；列出时只返回有读权限的仓库 |
| 查看已删除的仓库 | `admin` |
| fork | 来源仓库的 `read`，加上目标仓库名上的 `admin` |

### 删除与清除

- **软删除**：删除后按名字找不到这个仓库，读写都返回 `REPO_NOT_FOUND`，数据保留。
- **宽限期**：由 `scion server --repo-delete-grace` 设置（默认 24 小时，最少 2 小时），期间可以用 `scn repo restore` 恢复。
- **名字暂不可用**：宽限期内用同一个名字创建仓库，返回 `ALREADY_EXISTS` / `REPO_DELETED`。
- **清除**：宽限期过后，服务端的 janitor 分批删除这个仓库的全部元数据（ref、提交记录、对象索引、派生数据、redaction 等），之后名字才能重新使用。
- **对象字节**：在仓库之间共享，清除仓库时不删除。
- **宽限期下限的原因**：服务端的提交记录缓存、区域缓存与 daemon 的可见集都按仓库名缓存，最长 1 小时。名字在这之后才重新可用，所以重新创建的同名仓库看不到旧仓库的数据。

### fork

- **复制的内容**：在一个事务里复制来源仓库的属性、对象索引、提交记录、派生数据、路径历史、全部分支，以及有效的 redaction（fork 里 redact 过的文件同样读不到）。
- **不复制的内容**：合入记录和请求结果不复制，fork 中的合入从头开始计。
- **耗时**：与来源仓库的提交数和文件数成正比，都在数据库内完成，对象字节不复制。超大仓库的异步 fork 尚未实现。

## 分支

```sh
scn branch ls                 # * 标出默认分支
scn branch create dev [REV]   # REV 默认为当前跟踪的 ref
scn branch rm dev
```

在工作副本之外用 `--repo`。

- **权限**：创建、移动、删除分支都需要该 ref 的 `land` 权限，与合入相同。ACL 中按 ref 限制的规则同样生效。
- **`UpdateRef`**：直接把分支指向某个提交，可以不是快进，相当于强制推送。平时前进分支用 `scn land`（服务端 rebase、批处理、恰好一次）。
- **默认分支**：不能删除；先用 `scn repo set --default-branch` 换一个默认分支。
- **版本号**：删除后重新创建的分支从版本 1 开始，这个分支此前的版本历史（`ref_log` 审计记录）随删除一起清除。
- **WatchRefs**：不推送分支删除。
