#!/usr/bin/env bash
#
# VelaOS — split the pending working tree into scripted feature commits.
#
# What this script does:
#
#   The current working tree carries a large body of work (graphics alpha
#   blending, the syscall surface growth, DOS assigns/Info, input and
#   Intuition changes, the new GPU/USB/audio drivers, the 68k emulator,
#   README screenshots and the golden references). Instead of one lump
#   commit, this script creates one commit per feature, in dependency
#   order, each with a message that spells out which files changed and
#   what was added or fixed in them.
#
# Rules it obeys:
#
#   * It only ever `git add`s the exact paths listed below. Nothing else
#     is staged, so temporary scripts and build output can never slip in.
#   * The `reports/` folder is deliberately never committed.
#   * It never pushes. Review the log, then push yourself:
#       git push origin HEAD:main
#
# Usage:
#
#   ./push.sh     # create the commits
#
# Note: this tree is a git worktree on a detached HEAD. After running the
# script, `git log --oneline -20` shows the new commits; merge or reset
# your branch onto them as you prefer.

set -euo pipefail

cd "$(dirname "$0")"

say() { printf '%s\n' "$*"; }
fail() {
  say "$1" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Safety checks before touching anything.
# ---------------------------------------------------------------------------

git rev-parse --is-inside-work-tree >/dev/null 2>&1 \
  || fail "Not inside a git repository."

# The index must be clean to start with, or the per-feature staging below
# would sweep pre-staged files into the wrong commit.
if ! git diff --cached --quiet; then
  fail "The index already holds staged changes. Unstage them first:
  git reset"
fi

# ---------------------------------------------------------------------------
# The commits, oldest first. Each `commit_group` stages exactly the paths
# given and commits them with the message that follows.
# ---------------------------------------------------------------------------

commit_group() {
  local msg
  msg="$1"
  shift
  git add -- "$@"
  git commit -m "$msg"
  say "committed: $msg"
}

say "Creating the feature commits."

# 1 — the compositor learns per-pixel alpha.
commit_group "layers: blend layers with per-pixel alpha

layers: implement alpha-aware compositing.
- kernel/layers/src/lib.rs: add blend_over() and pack_alpha(); the top
  byte of a pixel is now coverage (0x00 = legacy fully-opaque pixel,
  0x01-0xFF = blend fraction) so overlapping layers no longer punch
  hard-edged black rectangles through each other. Unit tests cover the
  blend math." \
  kernel/layers/src/lib.rs

# 2 — the syscall surface honours alpha when writing pixels, and can
#     measure text for label layout.
commit_group "syscall/graphics: alpha-aware write_pixels and text_width

graphics: teach the framebuffer writer the alpha convention and add a
text measurement call.
- kernel/syscall/src/graphics_handlers.rs: write_pixels() now skips
  pixels whose alpha byte is 0x00 (legacy opaque) and blends the rest,
  matching the compositor; add the text_width handler so callers can
  measure a string in font cells instead of guessing len*8." \
  kernel/syscall/src/graphics_handlers.rs

# 3 — keyboard LEDs across the Intuition boundary.
commit_group "syscall/intuition: expose keyboard LED state

intuition: add the KeyboardLeds call.
- kernel/syscall/src/intuition_handlers.rs: report the caps/num/scroll
  lock bits maintained by the input decoder so the desktop can show
  keyboard LEDs." \
  kernel/syscall/src/intuition_handlers.rs

# 4 — number the new calls, grow the SDK, keep the parity tests green.
commit_group "syscall/sdk: number and publish the new graphics, intuition and dos calls

syscall numbering and the SDK surface move together so the kernel and
the SDK can never drift apart.
- kernel/syscall/src/numbers.rs: add GRAPHICS_TEXT_WIDTH (78),
  INTUITION_KEYBOARD_LEDS (205) and DOS_INFO (272); bump the DOS group
  count to 17.
- kernel/syscall/src/dispatch.rs: install the three new handlers in the
  syscall table.
- sdk/vela-sys/src/numbers.rs: mirror the new numbers in the SDK.
- sdk/vela-sys/src/api.rs: add graphics::text_width() and
  intuition::keyboard_leds() wrappers.
- sdk/vela-sys/idl/graphics.yaml and idl/intuition.yaml: declare the two
  new functions with their LVOs (first listed function carries the
  deepest offset, per the IDL convention).
- tests/api-conformance/tests/numbers_parity.rs: extend the parity list
  with the new pairs.
- platforms/hosted/tests/exec_two_task.rs: the reserved-number probe
  moves to a slot that is still unimplemented." \
  kernel/syscall/src/numbers.rs \
  kernel/syscall/src/dispatch.rs \
  sdk/vela-sys/src/numbers.rs \
  sdk/vela-sys/src/api.rs \
  sdk/vela-sys/idl/graphics.yaml \
  sdk/vela-sys/idl/intuition.yaml \
  tests/api-conformance/tests/numbers_parity.rs \
  platforms/hosted/tests/exec_two_task.rs

# 5 — DOS: multi-target assigns and volume information.
commit_group "dos: multi-target assigns and the Info call

dos.library: finish the assign model and add volume information.
- kernel/syscall/src/dos_handlers.rs: AddAssign now accepts several
  space-separated targets, building the ordered search list that
  'Assign NAME: SYS:a SYS:b' means on AmigaOS; add the Info handler
  which copies an InfoData record out to the caller.
- kernel/dos/src/lib.rs: add Dos::info(), resolving a path to its mount
  and reporting the volume name, total blocks, free blocks and the
  block size.
- amiga/abi-native64/src/lib.rs: add the InfoData structure (disk type,
  state, size, free, bytes-per-block, volume name) with an
  InfoData::for_volume() constructor." \
  kernel/syscall/src/dos_handlers.rs \
  kernel/dos/src/lib.rs \
  amiga/abi-native64/src/lib.rs

# 6 — input plumbing: LED tracking, pointer visibility, event flow.
commit_group "input/intuition/console: LED state, visible pointer, event pumping

wire the input pipeline from the decoder to Intuition.
- kernel/input/src/lib.rs: replace the single caps flag with a full LED
  bitmask (caps/num/scroll) tracked by the keyboard decoder and
  published on every KeyEvent.
- kernel/console/src/keyboard.rs: add pump_input() and queue_scancode();
  the console tick now drains raw input into the shared EventQueue so
  key and mouse events reach Intuition without a busy-waiting caller.
- kernel/intuition/src/lib.rs: dispatch_mouse() syncs the pointer via
  pointer_move(); the cursor now starts at the screen centre with
  pointer_seen set, so the pointer is visible before the first mouse
  event; KeyEventLike carries the new leds field." \
  kernel/input/src/lib.rs \
  kernel/console/src/keyboard.rs \
  kernel/intuition/src/lib.rs

# 7 — a real virtio-gpu display driver.
commit_group "drivers/gpu: a virtio-gpu 2D display driver

new crate vela-driver-gpu: a real virtio-gpu driver replacing the
pure-fallback path.
- kernel/drivers/gpu/src/lib.rs: implement the virtio device probe, the
  split transmit queue, and the control pipeline RESOURCE_CREATE_2D ->
  ATTACH_BACKING -> SET_SCANOUT -> TRANSFER_TO_HOST_2D -> FLUSH, with a
  GOP-style fallback when the device is absent." \
  kernel/drivers/gpu

# 8 — the USB stack: xHCI, HID devices and mass storage.
commit_group "drivers/usb: xHCI stack with HID and mass-storage device classes

new crate vela-driver-usb: the xHCI host controller and the first two
device classes.
- kernel/drivers/usb/src/lib.rs: xHCI initialisation (HCRST, RUN, the
  command ring, the event ring, the device context base address array)
  and device enumeration; HID keyboard and mouse report parsing with a
  usage-to-set-1 scancode table, polled on the 250 Hz tick.
- kernel/drivers/usb/src/registers.rs, trb.rs, dma.rs, descriptor.rs:
  register layouts, TRB rings, DMA-safe buffers and the descriptor
  parser the enumeration walks.
- kernel/drivers/usb/src/hid.rs: HID report decoding shared by the
  keyboard and mouse paths.
- kernel/drivers/usb/src/storage.rs: USB Mass Storage Bulk-Only
  Transport (CBW/CSW and SCSI READ(10)/WRITE(10)), registered as the
  usb.storage device." \
  kernel/drivers/usb

# 9 — the user-facing PCM device.
commit_group "drivers/audio: the audio.device PCM device

audio: give programs a device to play through.
- kernel/drivers/audio/src/lib.rs: add AudioDevice, the generic
  'audio.device' a program opens with OpenDevice(\"audio.device\",
  unit) and writes s16 LE stereo PCM frames to; samples route through
  the global mixer. init_audio_mixer() installs it at boot." \
  kernel/drivers/audio/src/lib.rs

# 10 — boot brings the new drivers up.
commit_group "entry: bring the GPU, USB and audio drivers up at boot

kernel/entry: wire the new drivers into the boot sequence.
- kernel/entry/src/lib.rs: PCI enumeration now finds the virtio-gpu and
  xHCI devices; the GPU driver is initialised with the GOP fallback,
  the xHCI controller is brought up with HID and USB mass-storage
  class drivers attached, and the audio mixer is installed; the present
  tick ends with a vela_driver_gpu::flush().
- kernel/entry/Cargo.toml: depend on the new driver crates." \
  kernel/entry/src/lib.rs \
  kernel/entry/Cargo.toml

# 11 — loader and boot menu robustness.
commit_group "boot: GOP mode fallback and Escape-to-default in the boot menu

- boot/uefi-loader/src/main.rs: when the firmware GOP is BLT-only (the
  QEMU virtio-gpu UEFI path), fall back gracefully instead of failing
  the boot; the std VGA mode gives an addressable 1280x800 BGR
  framebuffer.
- kernel/boot_menu/src/lib.rs: Escape (0x1B) selects the default boot
  entry." \
  boot/uefi-loader/src/main.rs \
  kernel/boot_menu/src/lib.rs

# 12 — the QEMU device model matches the drivers.
commit_group "xtask/qemu: attach the xHCI, USB mouse and AC97 devices

tools/xtask/src/qemu.rs: update the QEMU device configuration to the
devices the drivers actually implement: the default PS/2 keyboard,
'-device qemu-xhci' with a usb-mouse on its bus, and '-device ac97'
(ich9-ac97 is not supported by this QEMU). The VGA stays std so the
guest framebuffer is addressable." \
  tools/xtask/src/qemu.rs

# 13 — icon transparency at the source.
commit_group "icons: honour an explicit no-transparent-index marker

graphics/icons: fix the transparency decode.
- graphics/icons/src/lib.rs: color_at() treated 'no explicit
  transparent index' (raw[10] == 0xFF) as 'index 0 is transparent',
  which turned near-black palette entries into holes. The marker now
  maps to 'no transparent index' and desktop icons render without
  black boxes." \
  graphics/icons/src/lib.rs

# 14 — the desktop fixes: transparent icons, centred labels.
commit_group "workbench: transparent icons and centred, ellipsised labels

userland/workbench: the desktop painting fixes.
- userland/workbench/src/lib.rs: paint_art() treats near-black pixels
  (luminance below 120) as transparent so icons carry no black border;
  paint_label() centres the label within the icon-sized cell using the
  new text_width measurement instead of len*8, and truncates with an
  ellipsis when the label is wider than the cell." \
  userland/workbench/src/lib.rs

# 15 — the golden references for the fixed desktop.
commit_group "test-gui-golden: refresh the desktop reference frames

tests/gui-golden: regenerate the reference captures for the desktop
after the transparency, label-centring and pointer fixes, so the
golden test compares against the corrected rendering.
- tests/gui-golden/reference/desktop.ppm
- tests/gui-golden/reference/desktop-after-mouse.ppm" \
  tests/gui-golden/reference/desktop.ppm \
  tests/gui-golden/reference/desktop-after-mouse.ppm

# 16 — the 68k emulator and HUNK loader.
commit_group "emu68k: a 68k CPU emulator and HUNK executable loader

new crate vela-emu68k: the foundation of the 68k compatibility runtime.
- amiga/emu68k/src/lib.rs: a big-endian guest Memory with byte/word/
  long accesses and bulk copy in/out; the Cpu with the 68000 register
  file (D0-D7, A0-A7, PC, SR/CCR) and reset from the vector table; an
  instruction interpreter covering MOVE.B/W/L and MOVEA, MOVEQ, the
  arithmetic and logic family (ADD/SUB/CMP/AND/OR/EOR with register,
  memory and immediate forms, ADDQ/SUBQ, ADDA/SUBA/CMPA, MULU/MULS,
  DIVU/DIVS), single-operand CLR/NEG/NOT/TST, SWAP and EXT, LEA/PEA,
  JSR/JMP/RTS/RTR/LINK/UNLK, conditional branches BRA/Bcc/BSR with
  8- and 16-bit displacements, DBcc loops, Scc, BTST/BSET/BCLR/BCHG,
  and a TRAP #n bridge (TrapBridge) a host can implement to translate
  68k calls into VelaOS services.
- amiga/emu68k/src/lib.rs: an Amiga HUNK executable parser for
  HUNK_HEADER/HUNK_CODE/HUNK_DATA/HUNK_BSS/HUNK_RELOC32/HUNK_END with
  even-length padding, producing loadable segments and an entry point.
- amiga/emu68k/Cargo.toml: the crate manifest.
- amiga/emu68k tests cover the big-endian accessors, CPU reset,
  MOVEQ, RTS, and HUNK parsing." \
  amiga/emu68k

# 17 — the workspace knows the new crate.
commit_group "workspace: register emu68k in the workspace

- Cargo.toml: add amiga/emu68k to the workspace member list.
- Cargo.lock: the lockfile update that follows." \
  Cargo.toml \
  Cargo.lock

# 18 — documentation and the screenshots it references.
commit_group "docs: desktop screenshots and an honest desktop section

README: document what the desktop actually does and show it.
- README.md: add a Screenshots section (one row, two columns) with the
  Workbench desktop and the boot-menu/shell captures; add 'The Desktop
  (Vela Workbench)' describing the 1280x800 screen, transparent
  icons with centred ellipsised labels, the menu bar, the tool dock,
  the alpha compositor, PS/2 and USB-HID input, the boot menu and the
  AmigaDOS-style Safe Terminal; update the project status note.
- assets/image1.png, assets/image2.png: the two screenshots the README
  references." \
  README.md \
  assets

# 19 — this script, so the split travels with the tree.
commit_group "push: script the feature-commit split for the pending work

- push.sh: replace the old verify-only helper with a script that
  creates the per-feature commits for the current body of work
  (drivers, alpha compositing, DOS, emulator, docs), staging only the
  paths it names. It never pushes and never touches reports/." \
  push.sh

# ---------------------------------------------------------------------------
# Post-condition: nothing unlisted may be left behind. reports/ is the
# project's own analysis folder and is deliberately not committed; target/
# is ignored by git and does not show up here.
# ---------------------------------------------------------------------------

say ""
leftover="$(git status --porcelain --untracked-files=no || true)"
if [ -n "$leftover" ]; then
  say "WARNING — tracked changes remain uncommitted:"
  printf '%s\n' "$leftover" | sed 's/^/  /'
else
  say "All tracked changes are committed."
fi

say ""
say "Done. Review the history with 'git log --oneline', then push yourself:"
say "  git push origin HEAD:main"
say "(this script never pushes.)"
