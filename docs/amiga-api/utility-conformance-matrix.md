# `utility.library` — API conformance matrix

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
| `FindTag` | 320 | `verified` | `utility_find_tag_returns_the_matching_items_data` | Returns the matching item's `ti_Data`, never a pointer; the first match wins and `TAG_END` ends the list. |
| `StrNCpy` | 321 | `verified` | `utility_strncpy_copies_at_most_n_bytes` | Writes at most `n` bytes — text then a NUL — and returns how many it wrote; the SDK refuses an `n` its buffer cannot hold. |
| `StrCmp` | 322 | `verified` | `utility_strcmp_orders_two_strings` | Byte-wise and unsigned; the kernel folds `Ordering` to a 64-bit two's-complement result (`-1` / `0` / `1`), which the `Packed` jump-table ABI carries without truncation, and the SDK maps `0 => Equal`, `1 => Greater`, else `Less`. |

**Summary:** 3 of 3 entries are `verified`.
