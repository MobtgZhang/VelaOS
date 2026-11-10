# `exec.library` — API conformance matrix

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
| `CreateMsgPort` | 0 | `verified` | `exec_port_lifecycle_is_verified` | Returns a non-zero handle; deleting it makes the handle invalid. |
| `DeleteMsgPort` | 1 | `verified` | `exec_port_lifecycle_is_verified` | Deleting a port that does not exist reports `NO_OBJECT`. |
| `PutMsg` | 2 | `verified` | `exec_message_queue_is_fifo` | Messages are enqueued in order; a bad message pointer is refused. |
| `GetMsg` | 3 | `verified` | `exec_message_queue_is_fifo` | A failed copy-out must not consume the message. |
| `ReplyMsg` | 4 | `verified` | `exec_reply_goes_to_the_reply_port` | A reply with no reply port is refused with `NO_REPLY_PORT`. |
| `WaitPort` | 5 | `implemented` | — | Returns 0 with a message, 1 when the wait ended without one. |
| `Wait` | 6 | `implemented` | — | Returns the satisfied mask, or `u64::MAX` while still blocked. |
| `SetSignal` | 7 | `implemented` | — | Returns the task's new signal set. |
| `SetExcept` | 8 | `implemented` | — | Returns the task's new signal set. |
| `Signal` | 9 | `implemented` | — | Raises bits in another task's signal set. |
| `OpenLibrary` | 10 | `verified` | `exec_library_open_and_close` | An unknown library yields a null handle, not an error. |
| `CloseLibrary` | 11 | `verified` | `exec_library_open_and_close` | Closing a handle that was never opened reports `NO_OBJECT`. |
| `Forbid` | 12 | `verified` | `exec_forbid_and_permit_are_balanced` | Returns the new depth of the classic critical section. |
| `Permit` | 13 | `verified` | `exec_forbid_and_permit_are_balanced` | Returns the remaining depth. |
| `Yield` | 14 | `verified` | `exec_yield_requeues_the_task_after_its_body` | Records the request; the scheduler requeues the caller after its body returns. |
| `GetTaskID` | 15 | `verified` | `exec_get_task_id_returns_the_caller` | Returns the calling task's identifier. |
| `PutChar` | 16 | `implemented` | — | Writes one byte to the console. |
| `PowerOff` | 17 | `implemented` | — | Stops the machine through QEMU's debug-exit device. |
| `MapPage` | 18 | `verified` | `exec_map_page_maps_a_page_for_the_caller` | Maps one page into the calling task's own address space. |
| `Brk` | 19 | `verified` | `exec_brk_grows_and_shrinks_the_break` | Returns the break actually in force; growing maps pages and shrinking releases them. |
| `AddPort` | 20 | `verified` | `exec_add_port_reports_the_ports_signal_mask` | Returns the port's whole `mp_sig` mask; another task's port is `NOT_OWNER`. |
| `FindTask` | 43 | `verified` | `exec_find_task_finds_a_named_task` | A null name asks for the calling task; an unknown name answers `0`, never an error. |
| `SetTaskName` | 44 | `verified` | `exec_find_task_finds_a_named_task` | Names the calling task, the way a task sets `tc_Node.ln_Name`. |
| `InitSemaphore` | 45 | `verified` | `exec_semaphore_lifecycle_is_verified` | Creates a kernel-owned semaphore and returns its id; the classic `priority` argument orders the wait queue. |
| `ObtainSemaphore` | 46 | `verified` | `exec_semaphore_grant_reaches_the_waiter` | `1` when the lock is taken, `0` when the caller was queued; the grant arrives as `SIGF_SINGLE`. |
| `ConfirmGrant` | 47 | `verified` | `exec_semaphore_grant_reaches_the_waiter` | Consumes the pending grant wait and reports whether the hand-off named the caller; a grant cannot be confirmed twice. |
| `AttemptSemaphore` | 48 | `verified` | `exec_attempt_semaphore_reports_failure_without_blocking` | Never blocks: `0` says the lock was held, which is an answer rather than an error. |
| `ReleaseSemaphore` | 49 | `verified` | `exec_semaphore_lifecycle_is_verified` | Releases one nesting level and grants the queue onward; a release the caller does not own is `NOT_OWNER`. |
| `DeleteSemaphore` | 50 | `verified` | `exec_semaphore_lifecycle_is_verified` | Wakes the waiters so their next attempt reports `NO_OBJECT` rather than sleeping forever. |
| `Alert` | 51 | `verified` | `exec_alert_is_accepted_and_a_dead_alert_ends_the_task` | Logs the alert with its decoded name; the `AT_DeadEnd` type bit ends the calling task. |

**Summary:** 23 of 30 entries are `verified`.
