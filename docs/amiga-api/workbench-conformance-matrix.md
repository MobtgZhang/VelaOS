# `workbench.library` — API conformance matrix

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
| `AddAppIcon` | 448 | `verified` | `workbench_app_icons_walk_and_remove` | Registers an application icon; an empty name or path, or an unknown kind, is `INVALID_NAME`. |
| `AddAppMenuItem` | 449 | `verified` | `workbench_menu_items_lifecycle` | Registers a menu item with its action code; an empty label is `INVALID_NAME`. |
| `AddAppWindow` | 450 | `verified` | `workbench_app_windows_drop_zones_and_messages` | Registers a window owned by the calling task; an unknown window is `NO_OBJECT`. |
| `AddAppWindowDropZone` | 451 | `verified` | `workbench_app_windows_drop_zones_and_messages` | Registers a drop zone on a live window; a zero-area rectangle is `BAD_RECT`. |
| `CloseWorkbenchObject` | 452 | `verified` | `workbench_objects_open_update_close` | Marks an object closed and invisible; the record stays so its handle remains a stable name. |
| `GetNextAppIcon` | 453 | `verified` | `workbench_app_icons_walk_and_remove` | Walks the icon table after a cursor; zero is a normal end of iteration, not an error. |
| `MakeWorkbenchObjectVisible` | 454 | `verified` | `workbench_objects_open_update_close` | Sets an object's visibility; an unknown handle is `NO_OBJECT`. |
| `NotifyWorkbench` | 455 | `verified` | `workbench_register_start_and_notify` | Puts an action/data message on the registered Exec port; no registered port is `NO_PORT`. |
| `OpenWorkbenchObject` | 456 | `verified` | `workbench_objects_open_update_close` | Creates an open object record; an empty name or path is `INVALID_NAME`. |
| `QuoteWorkbench` | 457 | `verified` | `workbench_register_start_and_notify` | Toggles path quoting and returns the previous state. |
| `RegisterWorkbench` | 458 | `verified` | `workbench_register_start_and_notify` | Binds the calling task's port; a second registration is `OBJECT_IN_USE`, and another task's port is `NOT_OWNER`. |
| `RemoveAppIcon` | 459 | `verified` | `workbench_app_icons_walk_and_remove` | Removes an application icon, and a second removal is `NO_OBJECT`. |
| `RemoveAppMenuItem` | 460 | `verified` | `workbench_menu_items_lifecycle` | Removes a menu item, and a second removal is `NO_OBJECT`. |
| `RemoveAppWindow` | 461 | `verified` | `workbench_app_windows_drop_zones_and_messages` | Removes a window record the caller owns; another task's record is `NOT_OWNER`. |
| `RemoveAppWindowDropZone` | 462 | `verified` | `workbench_app_windows_drop_zones_and_messages` | Removes a drop zone, and a second removal is `NO_OBJECT`. |
| `SendAppWindowMessage` | 463 | `verified` | `workbench_app_windows_drop_zones_and_messages` | Puts an action/data message on the window's own port; another task's record is `NOT_OWNER`. |
| `StartWorkbench` | 464 | `verified` | `workbench_register_start_and_notify` | Marks the desktop running; an unregistered Workbench is `NO_OBJECT`. |
| `UnregisterWorkbench` | 465 | `verified` | `workbench_register_start_and_notify` | Drops the registration and clears the running flag; a second call is `NO_OBJECT`. |
| `UpdateWorkbenchObject` | 466 | `verified` | `workbench_objects_open_update_close` | Replaces an object's path and kind; an empty path is `INVALID_NAME`. |
| `WBConfig` | 467 | `verified` | `workbench_register_start_and_notify` | Copies the configuration line, terminator included, into the caller's buffer; a short buffer is `RANGE`. |
| `WBInfo` | 468 | `verified` | `workbench_register_start_and_notify` | Copies the information line, terminator included, into the caller's buffer; a short buffer is `RANGE`. |
| `WorkbenchControl` | 469 | `verified` | `workbench_register_start_and_notify` | Reports running state, registration, and the live object counts; an unknown operation is `NO_OBJECT`. |

**Summary:** 22 of 22 entries are `verified`.
