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
| `FindTag` | 320 | `not-started` | — | The syscall number is reserved and reports `NOT_IMPLEMENTED`. |
| `StrNCpy` | 321 | `not-started` | — | The syscall number is reserved and reports `NOT_IMPLEMENTED`. |
| `StrCmp` | 322 | `not-started` | — | The syscall number is reserved and reports `NOT_IMPLEMENTED`. |

**Summary:** 0 of 3 entries are `verified`.
