# `layers.library` — API conformance matrix

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
| `CreateLayer` | 128 | `verified` | `layers_create_and_destroy` | Returns a handle for the new layer. |
| `DestroyLayer` | 129 | `verified` | `layers_create_and_destroy` | Destroying a layer that does not exist reports `NO_OBJECT`. |
| `SetClip` | 130 | `implemented` | — | Sets the layer's clip rectangle. |
| `Front` | 131 | `implemented` | — | Brings a layer to the front of the stack. |
| `Back` | 132 | `implemented` | — | Sends a layer to the back of the stack. |
| `SetVisible` | 133 | `implemented` | — | Shows or hides a layer. |
| `MoveLayer` | 134 | `implemented` | — | Moves or resizes a layer. |
| `Composite` | 135 | `implemented` | — | Composites the stack into the framebuffer. |
| `Order` | 136 | `implemented` | — | The z-order, back to front. |
| `Topmost` | 137 | `implemented` | — | The topmost layer at a point. |

**Summary:** 2 of 10 entries are `verified`.
