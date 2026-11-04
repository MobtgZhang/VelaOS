# `icon.library` — API conformance matrix

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
| `GetDiskObject` | 384 | `verified` | `icon_disk_object_lifecycle_names_and_duplicates` | Finds an icon by its exact name; a missing name is `NOT_FOUND`, never a null handle. |
| `GetDiskObjectNew` | 385 | `verified` | `icon_disk_object_lifecycle_names_and_duplicates` | Finds an icon, or creates a default one under that name; the second call returns the same handle as the first. |
| `PutDiskObject` | 386 | `verified` | `icon_disk_object_lifecycle_names_and_duplicates` | Names an existing icon handle; the name is copied, not retained. |
| `FreeDiskObject` | 387 | `verified` | `icon_disk_object_lifecycle_names_and_duplicates` | Releases a handle; the slot stays reserved so other handles do not shift, and a second free is `NO_OBJECT`. |
| `DupDiskObject` | 388 | `verified` | `icon_disk_object_lifecycle_names_and_duplicates` | Copies an icon into a fresh handle that can be freed independently. |
| `DeleteDiskObject` | 389 | `verified` | `icon_disk_object_lifecycle_names_and_duplicates` | Removes the icon with the given name, then a lookup reports `NOT_FOUND`. |
| `GetDefDiskObject` | 390 | `verified` | `icon_defaults_and_tool_types` | Returns a copy of the default object for a kind, so editing it cannot mutate the default. |
| `PutDefDiskObject` | 391 | `verified` | `icon_defaults_and_tool_types` | Replaces the default object for a kind; a later `GetDefDiskObject` observes the replacement. |
| `NewDiskObject` | 392 | `verified` | `icon_defaults_and_tool_types` | A fresh default object of a kind, carrying `TYPE=<kind>` as a ToolType; an unknown kind is `INVALID_NAME`. |
| `FindToolType` | 393 | `verified` | `icon_defaults_and_tool_types` | Matches a ToolType name case-insensitively and copies the value plus its NUL terminator into the caller's buffer. |
| `MatchToolValue` | 394 | `verified` | `icon_defaults_and_tool_types` | Compares a ToolType value case-insensitively; a missing ToolType is `false`, not an error. |
| `GetIconTagList` | 395 | `verified` | `icon_tag_list_round_trip` | Copies up to `max_count` tags into the caller's buffer and returns how many it copied. |
| `PutIconTagList` | 396 | `verified` | `icon_tag_list_round_trip` | Replaces an icon's tag list; more than 64 tags is `TOO_LARGE`, never a silent truncation. |
| `BumpRevision` | 397 | `verified` | `icon_geometry_revision_and_state` | Increments and returns the icon's revision counter. |
| `LayoutIcon` | 398 | `verified` | `icon_geometry_revision_and_state` | Stores a rectangle read from a caller buffer and writes the one in force back; a zero width or height is `BAD_RECT`. |
| `GetIconRectangle` | 399 | `verified` | `icon_geometry_revision_and_state` | Writes the icon's rectangle into a caller-owned buffer; never a kernel pointer. |
| `IconControl` | 400 | `verified` | `icon_geometry_revision_and_state` | Selects, deselects, queries the selected state, and gets or sets the icon kind; an unknown operation is `RANGE`. |
| `DrawIconState` | 401 | `verified` | `icon_geometry_revision_and_state` | Records whether the icon is drawn selected; the desktop reads the state through `IconControl`. |
| `AddFreeList` | 402 | `verified` | `icon_free_list_and_selected_colour` | Remembers a live handle, ignoring a duplicate, and returns the list's length. |
| `FreeFreeList` | 403 | `verified` | `icon_free_list_and_selected_colour` | Clears the remembered handles and returns how many it dropped. |
| `ChangeToSelectedIconColor` | 404 | `verified` | `icon_free_list_and_selected_colour` | Sets the selected-icon colour word and returns the previous one. |

**Summary:** 21 of 21 entries are `verified`.
