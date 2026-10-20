# 打包下载

[English](archive.md) | 简体中文

把某个版本的整个仓库或其中一个目录打包成 tar.gz、zip 或 tar 下载，不需要先 clone 或检出。适合 Web 页面上的「下载源码」按钮和 CI 中只需要一份快照的场景。

## 命令行

```sh
scn archive -o mono.tar.gz                          # 当前 ref 的整个仓库
scn archive main:src --format zip -o src.zip         # main 上的 src 目录
scn archive v1.2 --prefix mono-v1.2/ -o mono.tar.gz  # 归档中的路径都在 mono-v1.2/ 之下
scn archive --format tar | tar -x -C /tmp/snap       # 不指定 -o 时写到标准输出
```

- **版本和目录**：用 `[REV:][DIR]` 指定，与 `scn cat` / `scn ls` 相同。
- **仓库**：在工作副本外使用时需要 `--repo`。
- **失败时**：使用 `-o` 时，失败不会留下不完整的文件。

## 接口

```
QueryService.Archive(repo, revision, format, path, prefix) → stream { data, changeset_id }
```

- **`format`**：`TAR_GZ`（默认）、`ZIP` 或 `TAR`。
- **`path`**：只打包这个目录，归档中保留它在仓库中的路径。例如 `path = "src"` 时，条目是 `src/...`。
- **`prefix`**：加在每个路径前面，必须为空或以 `/` 结尾，例如 `qwen-main/`。
- **返回**：内容分块流式返回，每块不超过 `max_chunk_bytes`。第一条消息带打包的提交 id。
- **出错时**：生成过程中出错（例如文件内容已被清除）时，流以错误结束，客户端应丢弃已收到的内容。
- **SDK**：`Client::archive(revision, path, format, prefix, &mut writer)`，返回打包的提交和写入的字节数。

服务端在 `GetServerInfo` 中声明特性 `query.archive`。

## 内容

- **顺序**：条目按目录深度优先、名字顺序写入，同一版本每次生成的内容相同。
- **修改时间**：所有条目都取提交时间，与 `git archive` 相同。
- **权限和类型**：
  - 普通文件 `0644`，可执行文件 `0755`；
  - 符号链接保存为链接；
  - 目录（包括空目录）也有条目。
- **大文件**：zip 中超过 4 GiB 的文件使用 zip64。
- **被 redact 的文件**：写入一段说明文字 `This file has been redacted (redaction <id>).` 代替原内容，不会中断下载。

## 权限

打包覆盖整棵子树，所以需要能读其中的全部内容：
- `path` 为空时，需要整个仓库的读权限；
- 指定 `path` 时，需要该路径的读权限。

只能读部分路径的用户，可以打包自己能读的目录。

## 开销

- **内存**：文件内容逐个读取，读到的数据边写边发送。内存占用与单个文件的大小相当，不随仓库大小增长。
- **背压**：客户端断开或读得慢时，服务端会停下来等待，或者停止生成。
- **下载大量大文件**：大文件更适合用预签名 URL 直接从对象存储下载，而不是经过归档。
