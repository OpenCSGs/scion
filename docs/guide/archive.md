# Archives

English | [简体中文](archive.zh-CN.md)

Download a whole repository, or one of its directories, at some revision as a tar.gz, zip or tar, without cloning or checking out first. It suits a "download source" button on a web page and CI jobs that need only a snapshot.

## Command line

```sh
scn archive -o mono.tar.gz                          # the whole repository at the current ref
scn archive main:src --format zip -o src.zip         # the src directory on main
scn archive v1.2 --prefix mono-v1.2/ -o mono.tar.gz  # every path in the archive under mono-v1.2/
scn archive --format tar | tar -x -C /tmp/snap       # to standard output without -o
```

- **Revision and directory:** given as `[REV:][DIR]`, as for `scn cat` and `scn ls`.
- **Repository:** outside a working copy, pass `--repo`.
- **On failure:** with `-o`, a failure leaves no partial file behind.

## API

```
QueryService.Archive(repo, revision, format, path, prefix) → stream { data, changeset_id }
```

- **`format`:** `TAR_GZ` (the default), `ZIP` or `TAR`.
- **`path`:** archive only this directory, keeping its path in the repository: with `path = "src"`, entries are `src/...`.
- **`prefix`:** put before every path; empty or ending in `/`, for example `qwen-main/`.
- **Response:** the content streams in chunks of at most `max_chunk_bytes`. The first message carries the id of the archived commit.
- **Errors:** an error while generating (for example, a file whose content was purged) ends the stream with that error, and the client should discard what it received.
- **SDK:** `Client::archive(revision, path, format, prefix, &mut writer)`, which returns the archived commit and the number of bytes written.

The server declares the feature `query.archive` in `GetServerInfo`.

## Contents

- **Order:** entries are written depth first, by name, so one revision always produces the same bytes.
- **Modification times:** every entry has the commit's time, as with `git archive`.
- **Modes and types:**
  - files `0644`, executables `0755`;
  - symbolic links are stored as links;
  - directories, empty ones included, have entries too.
- **Large files:** files over 4 GiB in a zip use zip64.
- **Redacted files:** the note `This file has been redacted (redaction <id>).` stands in for the content, and the download goes on.

## Permissions

An archive covers a whole subtree, so it needs read access to all of it:
- with an empty `path`, read access to the whole repository;
- with a `path`, read access to that path.

Users who can read only some paths can archive the directories they can read.

## Cost

- **Memory:** files are read one at a time and sent as they are read, so memory is about the size of one file and does not grow with the repository.
- **Backpressure:** when the client disconnects or reads slowly, the server waits or stops generating.
- **Many large files:** large files are better downloaded straight from object storage through presigned URLs than through an archive.
