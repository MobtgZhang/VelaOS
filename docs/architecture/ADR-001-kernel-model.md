# ADR-001 — The kernel model: a modular monolith

> **Status:** accepted
> **Date:** 2026-10-24
> **Scope:** the shape of the VelaOS kernel, the ring-3 boundary, and the
> message model that the Amiga-compatible API is built on.

---

## 1. Context

VelaOS has to do two things that pull in opposite directions:

* it must present an **AmigaOS 3.1-shaped** API to applications — tasks,
  message ports, signals, libraries, screens, windows, IDCMP events — and
* it must be a **modern 64-bit kernel** with UEFI boot, hardware page
  tables, a preemptive scheduler and a real privilege boundary.

The AmigaOS model is a **message-passing** model: a task talks to another
task by putting a message on a port and waiting for a reply. The classic
AmigaOS implementation is a **preemptive, uniprocessor, shared-memory**
kernel with a flat address space and no privilege separation: every task
runs in ring 0 and can read every other task's memory.

That flat model cannot be copied. A modern kernel that runs untrusted code
needs a privilege boundary, and a user program that can read the kernel's
memory is not a user program. So the question this ADR answers is: **how
much of the AmigaOS model survives the move to ring 3?**

---

## 2. Decision

**The kernel is a modular monolith.** Every subsystem — Exec, DOS, Graphics,
Layers, Intuition, the scheduler, the memory manager — is a separate crate
with a narrow, explicit interface, and they are compiled into **one**
kernel image with **direct function calls** between them. There is no
microkernel, no message-passing between kernel subsystems, and no
serialisation of internal interfaces.

**User programs are in ring 3.** They reach the kernel through a single
syscall gate, and every user-supplied pointer is validated before it is
dereferenced.

**The AmigaOS message model is preserved at the API boundary, and only
there.** Inside the kernel, subsystems call each other directly. Between a
user program and the kernel, the Exec message model — ports, messages,
replies, signals — is the *only* channel, exactly as it is on AmigaOS.

---

## 3. The modular monolith, specifically

### 3.1 — What "modular" means here

Each subsystem is a crate in the workspace:

| crate | responsibility |
|---|---|
| `kernel/exec` | tasks, message ports, messages, signals, the library registry |
| `kernel/vfs` | the mount table, the `Node` and `File` traits |
| `kernel/dos` | AmigaDOS path and assign resolution, the DOS file API |
| `kernel/graphics` | `RastPort`, `Bitmap`, pens, `Text`, `Move`, `Fill` |
| `kernel/layers` | layers, clip rectangles, z-order, compositing |
| `kernel/intuition` | screens, windows, gadgets, IDCMP |
| `kernel/input` | the PS/2 controller, the mouse packet decoder |
| `kernel/memory` | the frame allocator, the page mapper, the kernel heap |
| `kernel/scheduler` | tasks, context switching, the run queue, wait queues |
| `kernel/syscall` | the syscall numbers, the dispatch table, the user-copy helpers |

A crate boundary is a **compile-time** boundary, not a runtime one. The
crates are compiled together, the optimiser sees across them, and a call
from `kernel/intuition` to `kernel/exec` is an ordinary function call.

### 3.2 — Why not a microkernel

A microkernel would move the subsystem boundaries to runtime: every call
would be an IPC, every subsystem would live in its own protection domain,
and the kernel's "privileged" part would shrink to a scheduler and a memory
manager.

VelaOS does not do this, for three reasons.

**The subsystems are not separately trustworthy in the first place.** They
are all part of one operating system, written by one project, and they share
data structures — the window manager and the compositor share the layer
stack; the Intuition service and the input driver share the event queue.
A microkernel boundary between them would be a boundary without a threat
model behind it.

**The IPC cost lands on the hot paths.** Compositing a frame touches every
layer; delivering an IDCMP event touches the input queue, the event
allocator, the message port and the waiting task. In a microkernel, each of
those is a message round trip. In a modular monolith, each is a function
call.

**The privilege boundary that matters is the user boundary.** The property
that actually needs enforcing is "a user program cannot read or write the
kernel's memory, and cannot make the kernel fault". That boundary is
enforced by the page tables and by the syscall gate — not by how the kernel
is internally decomposed. A microkernel would not strengthen it; it would
only add a second, redundant one.

### 3.3 — What the modularity buys

* **The crates can be tested on the host.** `kernel/exec`, `kernel/dos`,
  `kernel/graphics`, `kernel/layers` and `kernel/intuition` are ordinary
  `std`-compatible libraries with no architecture dependency. The hosted
  harness in `platforms/hosted` runs the whole of Exec's message semantics
  under plain `cargo test`.
* **The dependency direction is enforced by cargo.** `kernel/intuition`
  depends on `kernel/exec`; `kernel/exec` does not depend on
  `kernel/intuition`. A cycle is a build error, not a runtime surprise.
* **The subsystems can be reasoned about separately.** The message model's
  ownership rules are entirely inside `kernel/exec`.

---

## 4. The ring-3 boundary

### 4.1 — One gate, one table

User programs reach the kernel through the `syscall` instruction and
`sysret`. `kernel/syscall` owns:

* the **number space** (`kernel/syscall/src/numbers.rs`);
* the **dispatch table**, a fixed-size array of handler slots with an
  `is_implemented` test per slot;
* the **user-copy helpers**, `probe_read` and `probe_write`.

The dispatch table is installed once, during boot, before the banner. A
number with no handler is not a crash: the dispatcher returns "not
implemented" as a value.

### 4.2 — The number space

The numbers are grouped so that a library's calls are contiguous:

| range | library |
|---|---|
| 0–19 | `exec.library` |
| 64–76 | `graphics.library` |
| 128–137 | `layers.library` |
| 192–198+ | `intuition.library` |

The DOS numbers are in `kernel/syscall/src/dos_handlers.rs`.

### 4.3 — Handles, never pointers

**A syscall never returns a kernel pointer.** It returns a **handle**: an
integer that names something in a kernel-side table. `OpenLibrary` returns a
library base handle; `CreateMsgPort` returns a port identifier;
`OpenWindow` returns a window identifier.

The reason is not abstraction for its own sake. A user program that held a
raw kernel pointer could read the kernel's memory through it, could pass it
back to a different syscall, and could — with the wrong argument — reach
another user's object. A handle has none of those properties: it is a number,
it is meaningless outside the calling process's own table, and using a
deleted handle is an error rather than a dangling dereference.

The `DeleteMsgPort` case is the concrete example. Deleting a port makes the
handle invalid; a subsequent operation on it reports `NO_OBJECT` rather
than following a freed pointer. This is asserted by
`tests/api-conformance/tests/exec_conformance.rs`.

### 4.4 — Every user pointer is probed

Every pointer a user program passes is validated by `probe_read` or
`probe_write` before it is dereferenced. The helpers reject:

* kernel addresses — anything at or above `0xFFFF_8000_0000_0000`, which is
  the kernel's half of the address space;
* unmapped user addresses;
* ranges that would cross out of the user's half of the address space.

A rejected pointer produces an **error code** — `BAD_POINTER` — never a
kernel fault. This is the property the plan calls out explicitly, and it is
tested: `tests/api-conformance/tests/hostile_pointers.rs` passes
deliberately invalid pointers and asserts a clean error, including the
subtle case that a failed `GetMsg` copy-out must **not** consume the message.

The subtlety is the point. A `GetMsg` that removes the message from the
queue and *then* fails to copy it out has destroyed the message: the sender
will wait for a reply forever, and the message is gone. The kernel copies
out first and only then removes the message from the queue.

### 4.5 — W^X

User text is mapped `user_rx` and user data `user_rw`, and the page mapper
enforces that a page is never both writable and executable. The kernel's own
image gets the same treatment: the loader computes the executable prefix of
the ELF, and the kernel maps that prefix read-execute and the rest
read-write.

---

## 5. The Exec message model

### 5.1 — Why the message model is preserved

The AmigaOS message model is not a general-purpose queue with a
convenience API. It is the **only** way a task talks to another task, and it
carries specific ownership semantics:

* a message carries a **reply port**, and the reply port's ownership is
  transferred to the receiver until it replies;
* `ReplyMsg` returns the reply to the *reply port*, not to the sender
  directly;
* a task that exits with unreplied messages has them **cleaned up**, so the
  sender is not left waiting on a port that will never answer;
* `Wait(mask)` and `Signal(task, mask)` are **bitmask** operations, not
  queue operations;
* `Forbid`/`Permit` are a **bounded** scheduler lock, not a global freeze.

Each of those is a semantic that a program depends on. A "message queue with
a `send` and a `receive`" would break them all.

### 5.2 — Where the model lives

The model lives entirely inside `kernel/exec`:

| concept | implementation |
|---|---|
| a task | `kernel/exec/src/task.rs` — a `TaskId`, a signal set, a signal mask |
| a port | `kernel/exec/src/port.rs` — a `PortId`, a FIFO of `Message`s, a signal pair |
| a message | `vela_abi_native64::Message` — the shared layout, with the reply port |
| the classic lock | `kernel/exec/src/lib.rs` — `ClassicLock`, with a depth counter |
| the library registry | `kernel/exec/src/library.rs` — name, version, reference count |

### 5.3 — The `Forbid`/`Permit` decision

On AmigaOS, `Forbid` prevents task switches for the duration of a critical
section. A naive implementation — a global "do not switch" flag — is a
correctness hazard in a preemptive kernel: a task that calls `Forbid` and
then blocks, or that holds the forbid across a long operation, stops the
whole machine.

VelaOS models it as a **bounded scheduler lock** with a depth counter:
`Forbid` increments the depth and returns the new depth; `Permit`
decrements it and returns the remaining depth. A depth of zero means the
scheduler may switch. This preserves the *observable* behaviour a program
relies on — the critical section is not interrupted — without giving any one
task the ability to freeze the machine.

`force_release` exists for the case where a task exits while holding the
lock: the lock is released rather than left held forever.

### 5.4 — IDCMP is delivered as an Exec message

This is the decision that ties the whole model together, and it is the one
the plan singles out.

Intuition's input events — mouse movement, mouse clicks, key presses,
gadget activation — are **not** delivered through a private channel, a
callback, or a shared ring buffer that the window manager and the
application both read. They are delivered as **ordinary Exec messages on the
window's message port**.

The consequences are the point:

* an application that opens a window gets a port, and receives events on it,
  exactly as it would receive a reply to a request it sent;
* the application's event loop is the same loop it would write for any
  other message source;
* a program that wants to forward events to another task forwards *messages*,
  which is a mechanism the Exec model already defines;
* there is no second event system to keep consistent with the first.

The acceptance test for this is in `platforms/hosted/tests/idcmp_acceptance.rs`
and in `userland/apps/guitest/tests/acceptance.rs`.

### 5.5 — The library registry

`OpenLibrary(name, min_version)` returns a **library base handle** — an
integer naming an entry in the kernel's registry — not a pointer to a jump
table. The registry holds the name, the version and a reference count;
`CloseLibrary` decrements it.

The reason a *handle* and not a jump table address is §4.3: the user
program's own library base lives in **user space** and is populated by the
SDK with the addresses of the SDK's own stubs, which marshal into syscalls.
The kernel's registry entry is the kernel-side state; the user-space jump
table is a different thing with a different lifetime. Conflating them would
mean handing the user a kernel address.

---

## 6. The hosted harness

`platforms/hosted` provides mock kernel services so that the Exec, DOS and
Intuition logic runs under plain `cargo test`, with no machine and no
kernel.

This is possible **because** of the modular-monolith decision. The
subsystems are ordinary libraries with no architecture dependency, so a test
can call `Exec::put_msg` directly, without a syscall gate and without a
ring-3 transition. The harness is not a re-implementation; it is the same
code, called differently.

The hosted harness is where most of the semantics in §5 are actually
verified:

| suite | what it covers |
|---|---|
| `platforms/hosted/tests/exec_two_task.rs` | two tasks exchanging messages, ordering, wake-on-enqueue, reply semantics, signal masks |
| `platforms/hosted/tests/dos_syscalls.rs` | the DOS file API over the VFS |
| `platforms/hosted/tests/idcmp_acceptance.rs` | the IDCMP-to-Exec-message path |
| `tests/api-conformance/tests/*.rs` | the per-API behaviour, with a status matrix |

---

## 7. Consequences

### 7.1 — What this makes easy

* **Testing.** The bulk of the semantics is testable on the host, in
  milliseconds, with no QEMU. The machine is needed for the boot path and
  for the framebuffer, and for nothing else.
* **Portability.** The subsystems do not know about x86-64. The other three
  architectures need a HAL, not a redesign.
* **A real privilege boundary.** The user's protection is enforced by the
  hardware, and it is the boundary that matters.
* **One place to look.** A programmer tracing "why did my window not
  appear" follows ordinary calls through the source, not a message trace.

### 7.2 — What this makes hard

* **A kernel bug takes down the kernel.** There is no per-subsystem
  isolation. A fault in the compositor is a fault in the machine. The
  mitigation is the same as it has always been in kernels: careful code,
  loud failures (the page-fault handler prints CR2 and the error code and
  halts), and tests.
* **The subsystems must agree on data structures.** Because they share
  memory directly, the shared structures' ownership rules have to be
  written down and obeyed. This is where the message model's ownership
  rules earn their keep: the message port's queue has exactly one owner.
* **The monolithic image grows.** Everything is in one binary. At v0.1.0's
  scale that is irrelevant; at a much larger scale it would need revisiting.
* **The syscall boundary is the only compatibility surface.** Anything that
  is not reachable through a syscall is unreachable from a user program. That
  is the intended shape, but it means a new user-visible feature needs a
  number, a handler and a pointer-validation story — not just a function.

### 7.3 — What this explicitly does not buy

* **No protection from a malicious kernel.** There is only one kernel.
* **No hot-swap.** A subsystem cannot be replaced at runtime.
* **No 68k support.** The 68k compatibility work (M6) will need a guest
  memory model and a bridge, which is a different design question.

---

## 8. Alternatives considered

| alternative | why not |
|---|---|
| **microkernel**, with each subsystem in its own address space | see §3.2: no threat model behind the boundary, IPC cost on the hot paths, and it does not strengthen the boundary that actually matters |
| **monolithic kernel with no crate structure** | rejected: it would make the subsystems untestable on the host, and the crate boundaries cost nothing at runtime |
| **keeping the AmigaOS flat model** — user programs in ring 0 | rejected: a user program that can read the kernel's memory is not a user program, and modern hardware and toolchains assume a privilege boundary |
| **a private event channel for IDCMP** instead of Exec messages | rejected: it would be a second event system, and the plan requires the message model to be the one channel |
| **returning kernel pointers** from syscalls | rejected: see §4.3 |
| **a global "do not switch" flag** for `Forbid` | rejected: see §5.3 |

---

## 9. References

* `reports/VelaOS.md` §6.1 (the modular-monolithic kernel), §6.2 (the two
  execution domains), §7.3 (how one API call runs), §7.5 (the Exec
  semantics), §7.6 (the API compatibility registry template).
* `.cursor/plans/velaos_v0.1.0_m0-m4_d71aa211.plan.md` — Phase 4 (ring 3,
  scheduler, Exec), Phase 6 (Intuition and IDCMP), Phase 8 (the tests that
  hold all of this in place).
* `docs/boot/entry-x86_64.md` — the boot sequence this ADR's ring-3
  boundary is reached by.
* `docs/amiga-api/*-conformance-matrix.md` — the per-API status, generated
  from the conformance tests.