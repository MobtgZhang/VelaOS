# `graphics.library` — API conformance matrix

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
| `AllocPen` | 64 | `verified` | `graphics_pen_lifecycle` | Pen zero is a legitimate pen, so failure is `0xFFFF`. |
| `FreePen` | 65 | `verified` | `graphics_pen_lifecycle` | Releasing a pen twice reports `NO_OBJECT`. |
| `SetPen` | 66 | `verified` | `graphics_pen_lifecycle` | Returns the colour the pen resolves to, packed `0x00RRGGBB`. |
| `Rect` | 67 | `verified` | `graphics_rect_needs_a_valid_pointer` | The rectangle arrives behind a pointer, so it is validated. |
| `FillRect` | 68 | `verified` | `graphics_rect_needs_a_valid_pointer` | The rectangle arrives behind a pointer, so it is validated. |
| `Text` | 69 | `verified` | `graphics_text_draws_into_the_window` | Returns the advance width in pixels. |
| `Line` | 70 | `implemented` | — | Draws a segment into the calling task's window. |
| `SetClip` | 71 | `implemented` | — | Sets the window's clip rectangle. |
| `PenColor` | 72 | `implemented` | — | The colour a pen resolves to. |
| `WritePixel` | 73 | `implemented` | — | Plots one pixel in the calling task's window. |
| `Move` | 74 | `implemented` | — | Sets the drawing position and plots it. |
| `SetPalette` | 75 | `implemented` | — | Installs a whole palette. |
| `RasterInfo` | 76 | `implemented` | — | The raster's geometry. |

**Summary:** 6 of 13 entries are `verified`.
