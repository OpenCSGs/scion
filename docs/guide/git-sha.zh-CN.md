# git 兼容的提交 sha

[English](git-sha.md) | 简体中文

Scion 的提交 id 是 64 位十六进制（`c1…`）。很多外部系统默认提交 id 是 40 位十六进制的 git sha，例如 starhub 的前端路由、缓存 key 和 HF SDK 的 `revision` 参数。为了让这些系统不用改，服务端给**每个提交**提供一个 git 兼容的 sha，并且接受它作为版本号。

## 怎么得到

| 方式 | 说明 |
|---|---|
| `QueryService.GetGitShas(repo, changeset_ids)` | 批量映射，一次最多 1000 个，结果与请求一一对应 |
| `QueryService.GetDerived(kind = GIT_SHA1)` | 单个版本（ref 名、提交 id 或 git sha） |
| 事件推送与合入前检查 | `old_git_sha`、`new_git_sha`，以及每个提交的 `git_sha`（见 [ref 变化事件](events.zh-CN.md)） |
| SDK | `Client::git_shas(&ids)`、`Client::git_sha(revision)` |

反过来，所有接受版本号的接口（`ResolveRevision`、`Log`、`Diff`、`ReadFile` 等）都可以直接传 40 位 git sha。

服务端在 `GetServerInfo` 中声明特性 `git.sha1`。旧版本服务端只给从 git 导入的提交提供 sha。

## 怎么算

- **从 git 导入的提交**：使用导入时记录的原始 sha（扩展字段 `git.sha1`），与原仓库完全一致。只有仓库 admin 可以创建带这个字段的提交，普通写者收到 `PERMISSION_DENIED`；服务端 rebase 时会去掉这个字段。
- **其余提交**：按 git 的对象格式逐层计算：
  - 文件：`blob`。权限对应 `100644`，可执行文件对应 `100755`，符号链接对应 `120000`。
  - 目录：`tree`，条目按 git 的规则排序。
  - 提交：`commit`，包含目录树、父提交的 git sha、作者和提交者（时间精确到秒，时区为 `±hhmm`）以及提交说明。
  - commit 对象在 committer 之后多一行 `scion-id <提交 id>` 头。
    - git 允许这种额外的头（jj 的 `change-id` 也是这样），`git fsck --strict` 可以通过。
    - 有了这行头，不同的 Scion 提交不会得到相同的 sha。git 的时间只精确到秒，也不包含 change id，没有这行头时，同一秒内内容相同的两个提交会撞上同一个 sha。
  - 名字和邮箱中的 `<`、`>` 和换行会被去掉，git 不允许出现这些字符。

## 缓存与开销

- 结果写入派生数据：提交 → sha、sha → 提交、目录树 → sha、文件 → sha。之后再查询只需一次批量读取，fork 出来的仓库也会一并复制这些数据。
- 第一次查询某个提交时才计算，会先算出它还没算过的祖先。
  - 未变化的子目录 id 相同，直接命中缓存。所以一个新提交只需计算改动路径上的对象。
  - 仓库的第一个提交需要读取全部文件内容一次。
- 配置了事件推送或合入前检查时，每次 ref 变化都会顺带算出新提交的 sha，查询时通常已经命中缓存。

## 限制

- 文件内容被清除（`scion admin purge`）之后，还没算过的提交无法再计算：
  - `GetGitShas` 和 `GetDerived` 返回 `FAILED_PRECONDITION`；
  - 事件和合入前检查中对应的字段为 `null`，事件照常推送。
- 已经算过的提交不受影响。
- redact 不影响计算：sha 按原始内容计算，与 redact 之前保持一致。
