# `intuition.library` — API conformance matrix

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
| `OpenScreen` | 192 | `verified` | `intuition_screen_and_window_lifecycle` | A zero-sized screen is refused with `BAD_RECT`. |
| `CloseScreen` | 193 | `verified` | `intuition_screen_and_window_lifecycle` | Closing a screen closes the windows on it and reports how many. |
| `OpenWindow` | 194 | `verified` | `intuition_screen_and_window_lifecycle` | The geometry arrives behind a pointer and is validated. |
| `CloseWindow` | 195 | `verified` | `intuition_screen_and_window_lifecycle` | Closing another task's window reports `NOT_OWNER`. |
| `SetTitle` | 196 | `verified` | `intuition_window_title_and_gadgets` | The title is copied out of user space before it is stored. |
| `Refresh` | 197 | `implemented` | — | Repaints every window; a non-zero flag also composites. |
| `SetAttrs` | 198 | `verified` | `intuition_window_title_and_gadgets` | Sets the window's flags and IDCMP mask. |
| `AddGadget` | 199 | `verified` | `intuition_window_title_and_gadgets` | The gadget's rectangle arrives behind a pointer and is validated. |
| `RemoveGadget` | 200 | `implemented` | — | Detaches a gadget from its window. |
| `SetBounds` | 201 | `implemented` | — | Moves or resizes a window; zero keeps the current dimension. |
| `Front` | 202 | `implemented` | — | Brings a window to the front of the layer stack. |
| `Back` | 203 | `implemented` | — | Sends a window to the back of the layer stack. |
| `Repaint` | 204 | `implemented` | — | Repaints a window's furniture. |

**Summary:** 7 of 13 entries are `verified`.
