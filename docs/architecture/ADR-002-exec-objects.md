# ADR-002 — The Exec objects, and how AROS's semantics survive ring 3

> **Status:** accepted
> **Date:** 2026-11-10
> **Scope:** the Exec service's objects — message ports, signals,
> semaphores, the task table and the alert path — and the decisions taken
> where AROS's model and a ring-3 kernel disagree.

---

## 1. Context

AROS's `exec.library` is the AmigaOS task and message system. It owns:

| Object | AROS source | What it is |
|---|---|---|
| message ports | `rom/exec/createmsgport.c`, `putmsg.c` | a queue plus the owner's signal bit |
| signals | `rom/exec/setsignal.c`, `signal.c`, `wait.c` | a 32-bit mask in the task |
| semaphores | `rom/exec/initsemaphore.c`, `obtainsemaphore.c`, `releasesemaphore.c` | a nested, owner-tracked lock with a wait queue |
| the public semaphore list | `rom/exec/addsemaphore.c`, `findsemaphore.c`, `remsemaphore.c` | named semaphores, findable by any task |
| the task table | `rom/exec/findtask.c`, `findtaskbypid.c` | tasks findable by name |
| alerts | `rom/exec/alert.c`, `compiler/include/exec/alerts.h` | the `AN_*`/`AG_*` numbers and their decoding |

Two of those are *structures in the caller's memory*: a
`SignalSemaphore` and a `struct Task`. A caller initialises them, keeps
them, and the kernel reaches into them.

VelaOS runs programs at ring 3 with an MMU between them and the kernel
(ADR-001). A structure in caller memory is therefore not an
implementation detail but a hole in the boundary: a program that writes a
forged `ss_Owner` or a forged `tc_Node.ln_Name` is writing into the
kernel's data structures.

This ADR records how the Exec objects are built so that AROS's *semantics*
survive while its *memory layout* does not.

---

## 2. Decision

### 2.1 Every Exec object is a kernel object

Message ports were already kernel objects, and the semaphores follow the
same rule:

* `InitSemaphore` in AROS initialises a structure the caller owns. Here
  `CREATE_SEMAPHORE` creates a semaphore in the kernel's table and hands
  back an opaque id. The classic `priority` argument — which ordered the
  wait queue — is kept for the ABI's shape; grants are made in arrival
  order.
* `AddSemaphore` in AROS initialises a semaphore and enqueues it in the
  system's *public* semaphore list, where `FindSemaphore` finds it by name.
  Here `ADD_SEMAPHORE` creates a named semaphore in the same table, and
  `FIND_SEMAPHORE` searches it. `REMOVE_SEMAPHORE` unlists it without
  destroying it, exactly as AROS does: the semaphore belongs to its owner,
  who may still be holding it.

The trade-off is deliberate: a program's semaphore id cannot be forged
into a lock it does not own, and a name that is already taken is refused
rather than silently shadowing another semaphore.

### 2.2 The grant is atomic, and the retry confirms it

AROS's `ReleaseSemaphore` performs the hand-off *inside* the release: the
owner changes, the queue is popped, and the waiter is woken with
`Signal(SIGF_SINGLE)`. A third task cannot take the lock in between,
because the release holds the list lock while it grants.

The same rule holds here. When the nest count reaches zero, the lock is
granted to every leading *shared* waiter, or to the first *exclusive*
waiter alone — AROS's rule, from `releasesemaphore.c`.

The one difference is how a granted waiter learns that it was granted.
AROS's waiter tests its own request node after the wake; here the
granted waiter would have to call `obtain` again, and that call would
*nest* the lock one level deeper than the first request asked for. So the
retry is a **confirmation** (`CONFIRM_GRANT`): it consumes the pending
`SIGF_SINGLE` wait and reports whether the hand-off named the caller.
A grant cannot be confirmed twice, and a wake that was not a grant leaves
the decision to the caller — the same answer `WaitPort` gives when it
returns `NULL`.

### 2.3 `SIGF_SINGLE` is AmigaOS's bit 16, not AROS's bit 4

The two systems disagree, and the difference matters here:

* AmigaOS numbers `SIGB_SINGLE` **16** and masks it with `1 << 16`.
* AROS's `compiler/include/exec/tasks.h` numbers it **4** — the same bit
  as `SIGB_BLIT`.

VelaOS follows **AmigaOS's 16**, because the low sixteen bits of a task's
signal set are the Exec reply space here: bit 0 is the "a message is
waiting" bit and bits 1–15 are the fifteen reply slots. A grant raised at
AROS's bit 4 would land on reply slot 3, and a program waiting for a
reply would wake up holding a semaphore.

That decision forced one correction elsewhere: `REPLY_MASK` read
`0xFFFF_FFFE`, which contradicted its own documentation and would have
put the reply slots on top of the classic signal space. It is now the
classic `0xFFFE` — fifteen slots, with everything above bit 15 free for
the classic signals.

### 2.4 `FindTask` answers with an id, and zero means "not here"

AROS's `FindTask(NULL)` answers with the calling task, and
`FindTask("name")` answers with `NULL` when nobody carries the name. Both
answers are preserved: a null name asks for the caller, and an unknown
name answers `0` rather than an error, because a program probing for a
task expects to be told "not here" rather than to receive a failure.

The task's name is set by the task itself (`SET_TASK_NAME`), the way an
AmigaOS task sets `tc_Node.ln_Name` at start-up.

### 2.5 An alert is logged, and a dead alert ends the task

An AmigaOS alert is a dialog on the screen, and an alert carrying the
`AT_DeadEnd` type bit halts the machine. There is no screen behind the
kernel yet, so the alert is written to the console — where the person at
the keyboard, or the boot log, sees it — with its number decoded into its
name, subsystem, class and code using AROS's own `alerts.h` layout.

A dead alert also ends the calling task. That part is not cosmetic: the
alert numbers exist to report a broken invariant, and a task that has
just reported one cannot be trusted to continue.

---

## 3. Consequences

* The Exec service owns two tables — semaphores and task names — that
  grow with the system. Both are small, and both are bounded by the
  number of live objects rather than by memory.
* A program that wants a *shared* lock asks for it explicitly: the
  `shared` flag on the obtain and attempt calls is AROS's
  `ObtainSemaphoreShared` and `AttemptSemaphoreShared`.
* The memory flags are policy, not mechanism: `kernel/exec/src/memory.rs`
  holds AROS's `MEMF_*` values and what they mean on a machine with no
  Chip RAM, while the allocation itself is per-task and page-granular.
* Nothing here changes the port-based message model, which is the part of
  Exec that the rest of the system is built on.

---

## 4. References

* AROS `rom/exec/`: `initsemaphore.c`, `obtainsemaphore.c`,
  `obtainsemaphoreshared.c`, `releasesemaphore.c`, `attemptsemaphore.c`,
  `addsemaphore.c`, `findsemaphore.c`, `remsemaphore.c`, `findtask.c`,
  `alert.c`
* AROS `compiler/include/exec/`: `semaphores.h`, `tasks.h` (the signal
  numbers), `alerts.h`, `memory.h`
* ADR-001 — the kernel model: a modular monolith
