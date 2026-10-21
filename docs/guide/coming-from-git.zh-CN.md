# 从 git 迁移

[English](coming-from-git.md) | 简体中文

`scn` 的参数沿用 git 的写法（`HEAD~1`、`A..B`、`-- PATH`、`-am`、`branch -d`），只有 Scion 的模型确实和 git 不同的地方才用自己的命令名。本文列出两者的对应关系；简明速查见 [scn 中的 git 命令](git-commands.zh-CN.md)。

## 模型上的不同

- **没有暂存区。** `scn commit` 提交所有本地改动。新增、删除和改名的文件通过扫描工作区发现，没有 `add`、`rm`、`mv`。
- **没有本地历史。** 工作副本只有一份快照。历史在服务端，所以 `log`、`blame`、`show` 和 `diff REV` 都是服务端查询，`cat`、`ls` 和 `archive` 不需要工作副本。
- **提交不等于发布。**
  - `scn commit` 在服务端创建一个提交，它还不在任何分支上。
  - `scn land` 发布它，相当于 `git push`。分支已经前进时，服务端会 rebase 这个提交，并且只合入一次。
- **只有一个待合入的提交。** 工作副本最多有一个已提交、未合入的提交，没有本地分支，也没有本地提交栈。
- **没有 remote。** 一个工作副本只属于一个服务端的一个仓库。

## 命令对照

| git | scn | 说明 |
|---|---|---|
| `git init` | `scn repo create NAME` | 仓库在服务端 |
| `git clone URL [DIR]` | `scn clone URL [DIR] [--ref B]`，或 `scn --endpoint SERVER clone REPO` | `URL` 为 `http(s)://host[:port]/REPO`。下载快照，不下载历史；进入工作副本后命令不用再写地址 |
| `git clone --sparse` + `git sparse-checkout set` | `scn clone REPO --sparse src --sparse docs`，`scn sparse set/add/rm/disable` | 按目录；也可以写 `--sparse a,b` |
| `git status` | `scn status` | git 的长格式；`-s` 显示 `A ` / `M ` / `D `（所有改动都算已暂存），冲突是 `UU` |
| `git add`、`git rm`、`git mv` | 不需要 | 直接改文件 |
| `git commit -m MSG`、`-am MSG` | `scn commit -m MSG`、`-am MSG` | 接受 `-a`，但没有作用。作者依次取 `--author`、`scn config`、git 的 `user.name` / `user.email` |
| `git commit --amend` | `scn commit --amend` | 只能修改待合入的提交 |
| `git reset --soft HEAD~1` | `scn uncommit` | 撤掉待合入的提交，保留它的改动 |
| `git restore PATH`、`git checkout -- PATH` | `scn restore PATH` | `.` 表示全部；新增的文件会保留（没有 `git clean`） |
| `git push` | `scn land` | 分支已经前进时由服务端 rebase |
| `git pull --rebase` | `scn sync` | 移到分支最新提交，并 rebase 待合入的提交；没有单独的 `fetch` |
| `git branch` | `scn branch`（或 `scn branch ls`） | |
| `git branch NAME [REV]` | `scn branch NAME [REV]`（或 `scn branch create`） | 分支是服务端的 ref。名为 `ls`、`create` 或 `rm` 的分支要用 `create` 的写法 |
| `git branch -d NAME` / `-D` | `scn branch -d NAME` / `-D`（或 `scn branch rm`） | |
| `git tag`、`git tag NAME [REV]`、`git tag -d NAME` | `scn tag`、`scn tag NAME [REV]`、`scn tag -d NAME` | 只有轻量标签（没有说明）。存成服务端永远不会移动的 ref `tags/NAME`。标签名可以当版本号用（`scn log v1.0`），除非有同名分支 |
| `git switch B`、`git switch -c B` | `scn switch B`、`scn switch -c B` | 有待合入的提交或改动过的文件时拒绝；新增的文件会带过去 |
| `git merge B` | `scn merge B` | 生成一个待合入的合并提交；`scn land` 发布它 |
| `git merge --abort` | `scn merge --abort` | |
| 解决冲突后的 `git add` | `scn resolve PATH`，`--ours` / `--theirs` | 还有冲突标记时拒绝，除非加 `--force`；不带路径时列出冲突 |
| `git rebase --continue`、`git merge --continue` | `scn commit` | `-m` 可以省略，沿用原来的提交说明 |
| `git log [REV] [-n N] [--oneline] [-- PATH]` | `scn log [REV] [-n N] [--oneline] [-- PATH]` | 与 git 格式相同（默认 20 个提交）；也可以用 `--path PATH` |
| `git show [REV] [-- PATH]` | `scn show [REV] [--name-only] [-- PATH]` | |
| `git diff`、`git diff A B`、`A..B`、`A...B` | `scn diff`、`scn diff A B`、`A..B`、`A...B` | `--name-only`、`-- PATH` |
| `git blame [REV] -- PATH` | `scn blame [REV:]PATH` | |
| `git show REV:PATH` | `scn cat REV:PATH` | 不需要工作副本 |
| `git ls-tree REV DIR` | `scn ls REV:DIR`，根目录用 `scn ls REV` | 不是版本号的单个参数按目录处理；`:DIR` 强制当作目录 |
| `git archive REV` | `scn archive REV`，`scn archive [REV:][DIR] --format tar.gz\|zip\|tar [--prefix P] [-o FILE]` | |
| `git config user.name` / `user.email` | `scn config set user.name` / `user.email` | 目前只有这两项设置 |
| `.gitignore` | `.gitignore` 和 `.scionignore` | 语法相同；同一目录下以 `.scionignore` 为准 |
| `git fast-import` | `scn import git PATH --repo R` | 保留原始的 git SHA |

## 过渡用的 git 写法

为了方便迁移，下面这些 git 命令也能用。每个都会执行右边的 scn 命令，并在 stderr 上提示对应的写法，例如：

```
hint: `scn push` is a transitional git-style spelling; use `scn land` instead
```

它们不出现在 `scn --help` 里，以后可能会去掉，脚本里请用 scn 的命令名。

| 你输入的 | scn 执行的 |
|---|---|
| `scn push` | `scn land` |
| `scn pull [--rebase]` | `scn sync` |
| `scn checkout BRANCH`、`scn checkout -b NAME` | `scn switch BRANCH`、`scn switch -c NAME` |
| `scn checkout -- PATH`、`scn checkout PATH`（已存在的路径） | `scn restore PATH` |
| `scn reset --soft [HEAD~1]` | `scn uncommit` |
| `scn reset --hard [HEAD]` | `scn restore .` |
| `scn rebase --continue`、`scn merge --continue` | `scn commit` |
| `scn add ...` | 什么也不做：没有暂存区 |
| `scn rm [-r] PATH...`、`scn mv FROM TO` | 删除 / 移动文件；下一次 `scn commit` 会带上 |

其它写法，例如 `reset --hard HEAD~3` 或 `rebase --abort`，会报错并说明 scn 里的做法。

## 版本号

| 写法 | 含义 |
|---|---|
| `main`、`feature/x` | 分支 |
| `v1.0`、`tags/v1.0` | 标签 |
| `c1…` | Scion 提交 id |
| 40 位十六进制 | git SHA。每个提交都有一个，见 [git-sha.zh-CN.md](git-sha.zh-CN.md) |
| `HEAD`、`@` | 工作副本的当前提交：有待合入的提交时是它，否则是同步到的提交 |
| `REV~N` | 沿第一父提交往上第 N 个祖先（单独的 `~` 表示 `~1`） |
| `REV^`、`REV^N` | 第一个或第 N 个父提交（`^0` 是提交本身） |

后缀可以像 git 一样组合，例如 `main~2^2`。分支名本身含有 `~` 或 `^` 时，优先按分支名解析。

## 尚未支持

- **工作副本**：检出旧版本、`revert`、`cherry-pick`、`stash`、`clean`、交互式 rebase、部分提交（`add -p`）。
- **查询**：附注标签、`grep`、`bisect`、`reflog`。
- **协作**：`remote`、`fetch`。git 客户端目前还不能直接访问 Scion 仓库，这是[路线图](../design/roadmap.md)里的 git 网关。
- **rebase 冲突**：停在冲突上的 sync 没有 `--abort`。它会撤掉待合入的提交，改动作为本地改动留在新的最新提交之上。
