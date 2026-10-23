# 用 git 访问 Scion 仓库

[English](git-clients.md) | 简体中文

服务端在 `scn` 的同一个地址和 URL 上提供 git 的 smart HTTP 协议，所以 git 命令行、CI 系统和 IDE 都可以克隆和拉取 Scion 仓库。git 推送稍后支持，在此之前请用 `scn` 推送。网关的工作原理见[设计文档](../design/git-gateway.zh-CN.md)。

- [克隆和拉取](#克隆和拉取)
- [凭据](#凭据)
- [SHA](#sha)
- [暂不支持](#暂不支持)
- [关闭](#关闭)

## 克隆和拉取

仓库的 git URL 就是它的 `scn` URL，加 `.git` 后缀也可以：

```sh
git clone http://scion.example.com:7080/agentlab/app
cd app
git pull                        # 快进到之后合入的提交
git fetch origin feature        # 任意分支
git ls-remote origin            # HEAD、refs/heads/*、refs/tags/*
```

- 分支显示为 `refs/heads/<name>`，标签（Scion 中的 `tags/<name>`）显示为 `refs/tags/<name>`，`HEAD` 指向仓库的默认分支。
- 支持 git 2.x 客户端，协议 v2（git 2.26 起默认）和较老的 v0 都可以。
- 克隆会拿到所拉取分支的全部提交，而不只是 `scn clone` 下载的快照。

## 凭据

网关接受和 `scn` 相同的 token，作为 HTTP Basic 认证的密码，用户名任意：

```sh
git clone http://git:$SCION_TOKEN@scion.example.com:7080/agentlab/app
```

也可以让凭据助手保存 token（`git config --global credential.helper store`，git 询问时输入任意用户名和 token）。能发送请求头的工具也可以用 `Authorization: Bearer <token>`（`git -c http.extraHeader="Authorization: Bearer $SCION_TOKEN" clone …`）。

- 仓库不允许匿名读取时，没有凭据 git 会提示输入。
- token 无权读取仓库时返回 `403`。
- 权限策略和 `scn` 相同（见[认证](auth.zh-CN.md)）：拉取需要仓库的 `read` 权限。

## SHA

每个提交只有一个 git SHA，就是 `GetGitShas` 和 SDK 的 `git_shas` 返回的那个（见 [git SHA](git-sha.zh-CN.md)）：

- **从 git 导入的提交**保留原来的 SHA，克隆得到的对象和源仓库完全一致。带签名（`gpgsig`）或子模块的导入提交暂时无法重建（见下文）。
- **用 `scn` 创建的提交**的 SHA 由内容计算得出，commit 对象中带一行 `scion-id` 头，`git fsck --strict` 可以通过。

## 暂不支持

| 功能 | 现象 |
|---|---|
| `git push` | 拒绝，提示 `pushing with git is not supported yet; push with scn` |
| 浅克隆（`--depth`）和部分克隆（`--filter`） | 服务端没有提供这两种能力，git 自己会拒绝 |
| 从 git 导入的签名提交或含子模块的提交 | 拉取失败，提示 `commit … was imported from git without its raw object and cannot be rebuilt; re-import it or use scn` |
| 历史中包含被 [redact](redaction.zh-CN.md) 的文件 | 拉取失败：文件的 SHA 覆盖的内容已不允许任何人读取 |
| SSH URL | 请使用 HTTP(S) |

## 关闭

`scion server --no-git`（或 `SCION_NO_GIT=1`）停止提供 git 服务，gRPC 接口不受影响。
