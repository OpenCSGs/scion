# Scion object model and canonical encoding (v1)

English | [简体中文](SPEC.zh-CN.md)

This document is the **only authoritative definition** of Scion's object format. An implementation in any language that follows it and passes every golden test vector under `testdata/vectors/` computes exactly the same object ids as the official implementation.

- Ordinary clients need only the ObjectId of §1 and the check after download of §1.2, not the full encoding and decoding.
- The canonical encoding is **unique**: a logical object has exactly one valid byte sequence, and decoders must reject any other (§6).

---

## 1. ObjectId

The binary form is always 34 bytes:

```
algo (u8) | kind (u8) | digest (32 bytes)
```

| algo | Name | Digest |
|---|---|---|
| `0x01` | BLAKE3-256 | BLAKE3's default mode, 32 bytes of output |

| kind | Name | Text prefix | Digest input |
|---|---|---|---|
| `0x01` | Blob | `b` | The file's raw bytes (no header of any kind) |
| `0x02` | Tree | `t` | The tree's canonical encoding (§4) |
| `0x03` | Changeset | `c` | The changeset's canonical encoding (§5) |
| `0x05` | ChunkList | `l` | The ChunkList's canonical encoding (§7); used only inside server storage |

Other values are reserved, and decoders must reject them.

### 1.1 Text form

```
<prefix letter><algo in decimal><64 lowercase hex digits of the digest>
```

For example, the Blob id of the empty file is `b1af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262`.

Parsers must reject uppercase hex, wrong lengths and unknown prefixes or algos.

### 1.2 Checking after download

For every kind, `digest = BLAKE3(object bytes)`, where the object bytes are the file's content for a Blob and the canonical encoding for the other kinds. Clients **must** check this on every object they receive; a mismatch means corruption, and no cache layer is to be trusted.

---

## 2. Basic types

| Notation | Encoding |
|---|---|
| `u8` | 1 byte |
| `uvarint` | Unsigned LEB128, at most 10 bytes, values in u64 |
| `svarint` | A signed integer ZigZag-encoded (`(n << 1) ^ (n >> 63)`), then as a `uvarint` |
| `bytes` | A `uvarint` length and the raw bytes |
| `string` | As `bytes`, and the content must be valid UTF-8 |
| `id` | The 34-byte binary ObjectId, without a length prefix |
| `list<T>` | A `uvarint` count, then the elements one after another |

**Varints must be minimal:** apart from 0, encoded as the single byte `0x00`, the last byte cannot be `0x00` (`0x80 0x00` is invalid, for example). More than 10 bytes, or a 10th byte above `0x01` (overflowing u64), are invalid.

---

## 3. Object header

The encodings of Tree, Changeset and ChunkList start with a 2-byte header:

```
kind (u8) | format_version (u8)
```

In v1, `format_version` is always `0x01`. The header's kind must be the kind the decoder expects.

---

## 4. Tree

```
header        = 0x02 0x01
entries       = list<Entry>

Entry:
  name        string
  mode        u8
  id          id
  size        uvarint
```

| mode | Meaning | Kind of id | size |
|---|---|---|---|
| `0x01` | Regular file | Blob | The file's size in bytes |
| `0x02` | Executable file | Blob | The file's size in bytes |
| `0x03` | Symbolic link (the content is the target) | Blob | The target's size in bytes |
| `0x04` | Subdirectory | Tree | Must be 0 |

Constraints:
- Entries are in **strictly ascending byte order** of `name`, with no duplicate names.
- `name` must follow the file name rules of §8.
- An empty tree (0 entries) is valid; it is the root of an empty repository.

---

## 5. Changeset

```
header        = 0x03 0x01
parents       list<id>            # ordered, the first is the first parent; kind must be Changeset
author        Signature
committer     Signature
message       string
change_id     16 bytes (no length prefix)
delta         list<FileChange>
extensions    list<Extension>

Signature:
  name        string
  email       string
  time_ms     svarint             # Unix milliseconds
  tz_offset   svarint             # minutes from UTC, in [-1440, 1440]

FileChange:
  path        string
  op          u8
  if op = 0x01 (Put):
    mode      u8                  # only 0x01 / 0x02 / 0x03, never 0x04
    id        id                  # kind must be Blob
    size      uvarint
  if op = 0x02 (Delete):
    (no further fields)

Extension:
  key         string              # not empty
  value       bytes
```

Constraints:
- `parents` has no duplicate ids. No parents means a root commit.
- `delta` is in strictly ascending byte order of `path`, with no duplicate paths; `path` must follow the path rules of §8.
- `delta` describes only **the changes against the first parent**. Directories are implicit: putting a file creates its parent directories, and a directory that becomes empty disappears. Conflicting paths in one delta (putting both `a` and `a/b`, say) are a semantic error reported when trees are built, not checked by the encoding.
- `extensions` are in strictly ascending byte order of `key`, with no duplicates. Unknown keys must be kept as they are.
- Reserved extension key: `git.sha1`, whose value is the 20-byte git commit SHA, written by the git importer. The server maps git SHAs to commit ids with it, so every API that takes a revision accepts a 40-digit lowercase hex git SHA.
- Reserved extension key: `scion.rebased-from`, whose value is the id of the commit as it was made (the 34-byte binary ObjectId, §1), written by the server on a commit it rebased while landing it.
- A changeset's id does **not include** the root tree. The root tree is derived data; see §4.3 of the server design document.

---

## 6. Inputs decoders must reject

In any of these cases, decoding fails with the matching error code (the codes are used by the golden vectors; implementations in other languages map them to equivalent errors):

| Error code | Case |
|---|---|
| `unexpected_eof` | The input ends in the middle of a field |
| `trailing_bytes` | Bytes remain after the object is decoded |
| `varint_overflow` | A varint is longer than 10 bytes or overflows u64 |
| `varint_non_minimal` | A varint is not minimal |
| `length_out_of_range` | A `bytes` length exceeds the remaining input |
| `wrong_kind` | The header's kind is not the expected one |
| `unsupported_version` | `format_version` is not 1 |
| `unknown_algo` / `unknown_kind` | An ObjectId's algo / kind is not defined |
| `unknown_mode` / `unknown_op` | A tree entry's or FileChange's mode / op is not defined; mode `0x04` in a FileChange is `unknown_mode` too |
| `invalid_utf8` | A `string` is not valid UTF-8 |
| `invalid_name` / `invalid_path` | §8 is violated |
| `not_sorted` | Entries, delta or extensions are not strictly ascending (duplicates included) |
| `id_kind_mismatch` | An id's kind does not match its mode or field |
| `nonzero_tree_size` | A subdirectory entry's size is not 0 |
| `duplicate_parent` | parents has duplicates |
| `invalid_tz_offset` | A time zone offset is out of range |
| `invalid_extension_key` | An extension key is empty |
| `invalid_chunk_list` | A ChunkList violates the constraints of §7 |

---

## 7. ChunkList (server-internal)

Files over 8 MiB are stored on the server in FastCDC chunks, and a ChunkList lists the chunks. Trees and changesets **always use the whole file's Blob id**; the ChunkList is a storage detail that clients never see.

```
header        = 0x05 0x01
total_size    uvarint
chunks        list<Chunk>

Chunk:
  id          id                  # kind must be Blob
  size        uvarint             # must be above 0
```

Constraints: at least one chunk, and the chunks' sizes add up to `total_size`.

---

## 8. File name and path rules

**File names** (a tree entry's `name`, and each component of a path):
- not empty, at most 255 bytes;
- not `.` or `..`;
- without `/` (0x2F) or NUL (0x00).

**Paths** (a FileChange's `path`):
- one or more valid file names joined by `/`, at most 4,096 bytes;
- not starting or ending with `/`, and without consecutive `/`.

Case and Unicode normalization are **not** handled: different bytes are different names.

---

## 9. Golden test vectors

In `testdata/vectors/`, as JSON:

```jsonc
{
  "valid": [
    {
      "name": "tree_single_file",
      "kind": "tree",                // blob | tree | changeset | chunk_list
      "input": { ... },             // structured input, with the field names of this document
      "encoding": "0201...",        // canonical encoding (lowercase hex); the raw content for a blob
      "id": "t1..."                 // the expected ObjectId in text form
    }
  ],
  "invalid": [
    { "name": "...", "kind": "tree", "encoding": "...", "error": "not_sorted" }
  ]
}
```

In the structured input, ids are in text form, `change_id` and extension `value`s are lowercase hex, and a blob's content is the hex field `data_hex`.

An implementation passes when, for every `valid` vector, its encoding is byte-for-byte `encoding`, the id matches and decoding gives back the same structure; and, for every `invalid` vector, decoding fails with the same error code.

---

## 10. Evolution

- New fields always go in `extensions`, without changing v1's byte layout.
- When the layout really must change, `format_version` goes up. Objects of a new version have different bytes and so different ids; decoders handle each version on its own, and objects of old versions stay readable forever.
