# `dos.library` — API conformance matrix

> Generated from `tests/api-conformance`'s registry. Do not edit by hand:
> run `VELA_UPDATE_CONFORMANCE_DOCS=1 cargo test -p vela-test-api-conformance`
> to regenerate this file.

The status column uses the taxonomy from the design document (§7.6):

| status | meaning |
|---|---|
| `not-started` | the API is not reachable — no syscall slot, no service method |
| `stub` | the API exists but returns a fixed placeholder |
| `implemented` | the API exists and returns a plausible value, but no test asserts the behaviour |
| `verified` | a test in `tests/api-conformance` asserts the documented behaviour |

**A stub is not compatible.** A stub that returned success would be
indistinguishable from a working call.

| API | syscall | status | verified by | notes |
|---|---|---|---|---|
| `Open` | 256 | `verified` | `dos_open_write_read_close` | Opening for writing creates the file. |
| `Close` | 257 | `verified` | `dos_open_write_read_close` | Closing a handle that is not open reports `NO_OBJECT`. |
| `Read` | 258 | `verified` | `dos_open_write_read_close` | The data is read first and copied out second. |
| `Write` | 259 | `verified` | `dos_open_write_read_close` | The copy in is first: the write cannot be undone. |
| `Seek` | 260 | `verified` | `dos_seek_moves_the_position` | `SEEK_SET`, `SEEK_CUR` and `SEEK_END` are the AmigaOS modes. |
| `ReadDir` | 261 | `implemented` | — | Returns a fixed-size record per entry, at most 64 at a time. |
| `DeleteFile` | 262 | `implemented` | — | Refuses to remove a directory. |
| `MakeDir` | 263 | `implemented` | — | Creates a directory in an existing parent. |
| `Examine` | 264 | `implemented` | — | Fills in a `FileInfo` in user space. |
| `CurrentDir` | 265 | `implemented` | — | Returns the process's current directory as a NUL-terminated string. |
| `ExamineDir` | 266 | `verified` | `dos_examine_dir_walks_a_directory_from_an_anchor` | Walks a directory one entry per call, resuming from the anchor it wrote. |
| `SetCurrentDir` | 267 | `verified` | `dos_set_current_dir_resolves_relative_paths` | Sets the current directory from a path; a relative path is resolved against the current one. |

**Summary:** 7 of 12 entries are `verified`.
