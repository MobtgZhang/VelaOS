# The framebuffer golden test — how it works and why

> **Scope:** `tests/gui-golden`. This document explains the approach, the
> decision that was made about the reference image, and the properties the
> test does and does not assert.

---

## 1. What the test does

The plan's Phase 8 asks for a test that "boots with a scripted synthetic
mouse move, captures the framebuffer via QEMU `screendump`, and compares
against a stored reference". The test does exactly that, in five steps:

1. build a bootable disk image;
2. boot it under QEMU with a hard timeout;
3. wait for the kernel's ready banner, then let the desktop settle;
4. capture the framebuffer with QEMU's `screendump` monitor command;
5. compare the capture against the reference — then do the same again after
   a scripted mouse move.

The two captures are compared against two references:

| capture | reference |
|---|---|
| the desktop as it settles | `tests/gui-golden/reference/desktop.ppm` |
| the desktop after the mouse move | `tests/gui-golden/reference/desktop-after-mouse.ppm` |

The mouse move is injected through QEMU's monitor `mouse_move` command,
which produces the same PS/2 packets a real mouse would. The kernel's whole
input path — the PS/2 controller, the packet decoder, the event queue and
the IDCMP delivery — is therefore exercised end to end, without a human at
the keyboard. The move is far enough to cross the desktop's gadget areas and
back, and no button is held, so the desktop treats it as a movement rather
than a click.

---

## 2. The decision: a committed reference, with a documented tolerance

**The reference is meant to be committed to the repository.** It is not
generated on first run.

### 2.1 — Why not generate on first run

A golden test that generates its own reference on the first run asserts
only that the framebuffer is *stable between two runs*. That is a real
property, but it is a much weaker one, and it has a specific failure mode:
it accepts **whatever the framebuffer happens to be**, including a black
screen, a boot splash, or a half-drawn desktop. The test would be green and
would be proving nothing about the desktop at all.

A committed reference inverts that. The desktop's appearance becomes a
*specified* property: a change to it is a failing test with a picture
attached, and a human decides whether the change was intended.

The cost is that the reference has to be regenerated whenever the desktop
legitimately changes. That is a deliberate trade: a desktop change should
require a human to look at it.

### 2.2 — Why the reference is committed

The reference is now committed, so the test compares the live desktop against
the specified image rather than regenerating it. A missing or corrupted
reference is still treated as an explicit skip rather than a silent
regeneration:

```text
SKIP: the golden reference tests/gui-golden/reference/desktop.ppm does not
exist, so there is nothing to compare against. Generate it with
`VELA_UPDATE_GOLDEN=1 cargo test -p vela-test-gui-golden`.
```

This is the correct behaviour, and it is the behaviour the plan asks for:
"make the test tolerant of a missing reference (skip with a clear message)
rather than failing spuriously."

### 2.3 — How the reference is generated

```bash
VELA_UPDATE_GOLDEN=1 cargo test -p vela-test-gui-golden
```

The environment variable is checked **explicitly**, rather than inferred
from the presence or absence of the reference file. The difference matters:
inferring it would mean the *first* run against a missing reference
silently generated one, which is precisely the vacuous behaviour §2.1
rejects. With the explicit flag, a missing reference is always a visible
skip, and regenerating is always a deliberate act.

The flag also has to be checked *before* the image is built, because
generating a reference is expensive and a developer running the test
normally does not want to pay for a QEMU boot.

---

## 3. The image format

QEMU's `screendump` writes a **PPM** (`P6`, binary RGB) when the filename
ends in `.ppm`. That format is trivial to parse — a short ASCII header
followed by raw bytes — so `tests/gui-golden/src/ppm.rs` implements a
decoder and an encoder for it directly.

No image-crate dependency was added. The reasoning:

* PPM is header-plus-raw-bytes; there is nothing to decompress and nothing
  to unfilter;
* the decoder is 40 lines and the encoder is 20;
* the test needs to *compare* pixels, not to draw anything, so it needs
  exactly one thing from an image library: a decoded pixel buffer;
* adding `image` would pull in `png`, `jpeg` and their decoders for a
  format that has none of that.

The decoder handles the parts that actually vary: the `P6` magic, the
width, the height, the maximum value, **comments** (`#` to end of line,
which some writers emit between header fields), and **arbitrary whitespace**
between the header tokens. A truncated file, a missing header, a
non-`P6` magic, a maximum value above 255, and a pixel buffer shorter than
`width * height * 3` are all errors rather than panics — the framebuffer
comes from a machine, and a machine's output is untrusted input.

The `P6` subset is exactly what QEMU emits. The encoder is used only to
write the reference back out.

The PPM decoder and encoder have their own test file,
`tests/gui-golden/tests/ppm.rs`, with 14 tests covering the header forms,
the comments, the whitespace, the truncation cases and the round trip.

---

## 4. The comparison, and the tolerance

The comparison is **not** "every pixel must match". The capture depends on
things that are not part of the kernel's behaviour:

* the exact framebuffer the firmware hands over — its stride, its pixel
  format, its physical base;
* the moment the capture happened relative to the desktop's repaints.

A strict comparison would fail on a pixel of animation timing, which is
noise, not a regression.

So the test compares a **fraction of differing pixels** against a threshold,
`DEFAULT_TOLERANCE = 0.01` — one percent. On a 1280x800 screen that is
about 10,000 pixels.

The threshold is not arbitrary. A real change to the desktop — a moved
window, a different palette, a missing gadget — alters tens of thousands of
pixels, so it is caught by a threshold two orders of magnitude smaller than
the change it needs to detect. The number of differing pixels is printed
whether the comparison passes or fails, so a change that stays under the
threshold is still visible in the test output.

A **geometry change** is a hard failure, not a tolerance question: if the
capture is 1280x800 and the reference is 1024x768, the display geometry
changed and the reference must be regenerated. The error message says so.

---

## 5. What the test guarantees

### 5.1 — QEMU is always terminated

The same guarantee as the boot smoke test, and for the same reason: this
project has a history of leaked QEMU processes.

`xtask::qemu::Vm` owns the child process, and its `Drop` implementation
kills **and reaps** it. `Drop` runs on the normal return, on an early `?`
that propagates an error, and while a panic unwinds the stack. The capture
function holds the guard in a local variable, so the teardown happens on
every path out of it — including the path where the kernel never reaches
its banner and the capture is abandoned.

The explicit timeout is the second, independent bound: even if the guard
somehow did not run, the run cannot last longer than the deadline.

### 5.2 — The test skips when it cannot run

A machine without `qemu-system-x86_64`, without an OVMF firmware image, or
without the `mtools`/`mkfs.vfat`/`sgdisk` trio cannot run the test, and
must not fail because of it. The precondition check names the missing
prerequisite in plain words and the test returns successfully with a
`SKIP:` line.

---

## 6. Files

| file | what it holds |
|---|---|
| `tests/gui-golden/src/lib.rs` | the capture and comparison machinery, and the skip logic |
| `tests/gui-golden/src/ppm.rs` | the PPM decoder and encoder |
| `tests/gui-golden/tests/golden.rs` | the test itself |
| `tests/gui-golden/tests/ppm.rs` | the decoder's and encoder's own tests |
| `tests/gui-golden/reference/` | the committed references |

---

## 7. References

* `.cursor/plans/velaos_v0.1.0_m0-m4_d71aa211.plan.md` — Phase 8.
* `tests/boot-smoke/src/lib.rs` — the same QEMU-termination guarantee,
  explained in full.
