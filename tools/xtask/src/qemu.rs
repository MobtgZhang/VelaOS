//! QEMU runner.
//!
//! Two lessons from the probe are baked in here:
//!
//! * A run **must** be bounded. The probe leaked QEMU processes when a script
//!   exited without tearing the VM down, and leaked VMs hold resources and eat
//!   CPU. [`Vm`] always terminates the child, on every path.
//! * The `isa-debug-exit` device lets the guest power the machine off cleanly
//!   on success. CI then sees a fast, deterministic exit instead of waiting for
//!   the timeout.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Firmware images, tried in order. Distributions disagree on the filename and
/// on whether OVMF ships combined or split.
///
/// # Why this is public
///
/// The list is a property of the environments the project supports, and the
/// test that pins the two naming conventions lives in `tests/qemu.rs`. The
/// project bans `#[cfg]` outside `hal/` and `boot/`, so the test cannot be an
/// in-source `#[cfg(test)]` module.
pub const FIRMWARE_CANDIDATES: &[&str] = &[
    "/usr/share/OVMF/OVMF.fd",
    "/usr/share/ovmf/OVMF.fd",
    "/usr/share/ovmf/OVMF_CODE.fd",
    "/usr/share/edk2/ovmf/OVMF.fd",
];

/// Serial log lines the boot smoke test looks for.
///
/// The kernel's own banner is the authoritative signal that the machine
/// reached a stable idle, so the markers are the exact strings
/// `kernel/entry` prints.
///
/// The loader's own marker is *not* here: OVMF owns COM1 while Boot Services
/// run, so the loader's log goes to the debug console instead of the serial
/// port. A marker that lives in a different file cannot be looked for in this
/// one, and searching for it here would make every check fail forever. The
/// loader's marker is checked separately, against the debug console log.
pub const BOOT_MARKERS: &[&str] = &[
    "VELA-KERNEL: entering",
    "VELA-KERNEL: boot info accepted",
    "VELA-KERNEL: VelaOS 0.1.0 ready",
];

/// Marker the UEFI loader prints to the debug console.
pub const LOADER_MARKER: &str = "VELA-BOOT";

/// True when the loader's debug-console log shows it started.
///
/// This is a weaker signal than [`BOOT_MARKERS`] — it only proves the loader
/// ran, not that the kernel came up — so it is used to tell "the loader ran
/// and the machine was still alive" from "nothing happened at all".
///
/// It is deliberately *not* used to decide whether a run succeeded: a loader
/// that ran and then failed to hand off would satisfy it, and treating that
/// as success would turn a broken handoff into a green run.
pub fn loader_marker_present(loader_log: &str) -> bool {
    strip_ansi(loader_log).contains(LOADER_MARKER)
}

/// The OVMF firmware image this machine has, if any.
///
/// `None` means the machine cannot boot a VelaOS image at all: the tests
/// treat that as a reason to skip, not to fail.
pub fn find_firmware() -> Option<&'static str> {
    FIRMWARE_CANDIDATES
        .iter()
        .copied()
        .find(|p| Path::new(p).exists())
}

/// True when `qemu-system-x86_64` is on `PATH`.
pub fn qemu_available() -> bool {
    which("qemu-system-x86_64").is_some()
}

/// Locate an executable on `PATH`.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Where a run's logs and monitor socket live.
///
/// # Why this is a struct and not a directory prefix
///
/// Several agents share this machine, and they all want a QEMU run. A fixed
/// path would mean one agent's run could overwrite another's logs *while the
/// other run is still reading them*, which produces failures that look like
/// kernel bugs. Every field is therefore derived from a per-agent prefix, so
/// two runs never touch the same file.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The serial port capture, where the kernel's log lands.
    pub serial: PathBuf,
    /// The debug console capture, where the loader's log lands.
    pub debugcon: PathBuf,
    /// The QEMU monitor socket.
    pub monitor: PathBuf,
}

impl Paths {
    /// Build the paths for a run identified by `tag`.
    ///
    /// `tag` is normally the agent's own letter, so the files land next to
    /// everybody else's without colliding.
    pub fn tagged(root: &Path, tag: &str) -> Self {
        Self {
            serial: root.join(format!("target/serial-{tag}.log")),
            debugcon: root.join(format!("target/debugcon-{tag}.log")),
            monitor: root.join(format!("target/monitor-{tag}.sock")),
        }
    }

    /// Remove any stale log files from a previous run.
    ///
    /// A leftover log from an earlier run would otherwise be appended to, and
    /// the smoke test would "pass" on the previous run's markers.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.serial);
        let _ = std::fs::remove_file(&self.debugcon);
        let _ = std::fs::remove_file(&self.monitor);
    }

    /// Remove the monitor socket file.
    ///
    /// QEMU unlinks the socket when it exits cleanly, but a killed VM never
    /// gets that far -- the socket file outlives the process. The boot smoke
    /// test uses the socket's *presence* as its "no VM leaked" signal, so
    /// the teardown has to remove it explicitly.
    pub fn remove_monitor_socket(&self) {
        let _ = std::fs::remove_file(&self.monitor);
    }
}

/// A running QEMU machine that is guaranteed to be terminated.
///
/// # The guarantee
///
/// The `Drop` implementation kills the child and reaps it. That covers every
/// path out of the function that created it: the normal return, an early
/// `?` that propagates an error, and the unwinding that a `panic!` causes.
/// The probe that produced this design leaked QEMU processes because the
/// script that started them had no teardown on its error paths, and a leaked
/// VM keeps a core busy and holds the image file open.
///
/// The explicit timeout in [`Vm::wait_for`] is a *second* line of defence: it
/// bounds the wall-clock time of a run even if the guard somehow did not run.
pub struct Vm {
    child: Child,
    paths: Paths,
    /// Set once the child has been reaped, so `Drop` does not try again.
    reaped: bool,
}

impl Vm {
    /// Start QEMU on `image`.
    ///
    /// `timeout_secs` is a hard ceiling enforced by [`Vm::wait_for`], and the
    /// [`Drop`] guard is a backstop.
    pub fn start(image: &Path, paths: Paths, display: bool) -> Result<Self> {
        if !image.exists() {
            bail!(
                "image {} not found; run `cargo xtask image` first",
                image.display()
            );
        }
        let firmware = find_firmware().with_context(|| {
            format!(
                "no OVMF firmware found; looked in: {}",
                FIRMWARE_CANDIDATES.join(", ")
            )
        })?;

        paths.clear();

        let mut cmd = Command::new("qemu-system-x86_64");
        cmd.args(["-machine", "q35"])
            .args(["-m", "512M"])
            .arg("-no-reboot")
            // # Why the CPU model is pinned, and why this is only a convenience
            //
            // The kernel detects `RDTSCP` through `CPUID` and falls back to
            // plain `RDTSC` when the CPU does not implement it, so it boots
            // correctly on QEMU's default `qemu64` model -- which does *not*
            // implement `RDTSCP` and used to triple-fault the machine on the
            // first clock read.
            //
            // Asking for `+rdtscp` anyway is a convenience rather than a
            // requirement: it gives the guest the extra auxiliary clock
            // value and the stronger ordering guarantee, so the development
            // machine exercises the same code path as a real CPU.
            //
            // It is deliberately *not* relied upon. A user running an older
            // QEMU, or a hypervisor whose model cannot be extended this way,
            // must still get a working kernel -- so the fallback is the
            // thing that matters, and the flag only removes the need for it
            // in the common case.
            .args(["-cpu", "qemu64,+rdtscp"])
            .arg("-serial")
            .arg(format!("file:{}", paths.serial.display()))
            .arg("-debugcon")
            .arg(format!("file:{}", paths.debugcon.display()))
            // The monitor socket is how the framebuffer is captured. It is
            // created in server mode so the test can connect after the
            // machine is already running.
            .arg("-monitor")
            .arg(format!("unix:{},server,nowait", paths.monitor.display()))
            // Guest writes 0x10 to port 0xf4 to power off; QEMU exits with that
            // value as the exit code.
            .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
            .arg("-display")
            .arg(if display { "sdl" } else { "none" })
            .arg("-drive")
            .arg(format!("if=pflash,format=raw,file={firmware},readonly=on"))
            .arg("-drive")
            .arg(format!("format=raw,file={}", image.display()));

        let child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start qemu-system-x86_64")?;

        Ok(Self {
            child,
            paths,
            reaped: false,
        })
    }

    /// The paths this VM writes to.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Wait for the guest to stop on its own, up to `timeout_secs`.
    ///
    /// Returns `true` if the machine stopped by itself, `false` if the
    /// timeout expired — in which case the machine is still running and the
    /// caller decides what that means.
    pub fn wait_for(&mut self, timeout_secs: u64) -> Result<bool> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            match self.child.try_wait().context("polling qemu")? {
                Some(_) => {
                    self.reaped = true;
                    return Ok(true);
                }
                None => {
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }

    /// The serial log captured so far.
    pub fn serial_log(&self) -> String {
        std::fs::read_to_string(&self.paths.serial).unwrap_or_default()
    }

    /// Wait until the serial log contains every marker in `markers`.
    ///
    /// Returns `true` if the markers appeared before the deadline. The kernel
    /// writes its banner to the serial port as it boots, so this is how a
    /// caller knows the machine has reached a stable state and a framebuffer
    /// capture will show something meaningful.
    ///
    /// The wait is bounded, and the machine is still running when it returns
    /// either way — the caller's `Drop` guard is what terminates it.
    pub fn wait_for_markers(&self, markers: &[&str], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let log = strip_ansi(&self.serial_log());
            if markers.iter().all(|m| log.contains(m)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The loader's debug-console log captured so far.
    pub fn debugcon_log(&self) -> String {
        std::fs::read_to_string(&self.paths.debugcon).unwrap_or_default()
    }

    /// Send a command to the QEMU monitor and read the reply.
    ///
    /// The monitor is the only way to capture the framebuffer, because the
    /// guest has no framebuffer-read service: `screendump` asks QEMU to
    /// copy the display device's surface out.
    pub fn monitor_command(&self, command: &str) -> Result<String> {
        let stream = UnixStream::connect(&self.paths.monitor).with_context(|| {
            format!(
                "connecting to the QEMU monitor at {}",
                self.paths.monitor.display()
            )
        })?;
        let mut writer = stream.try_clone().context("cloning the monitor socket")?;
        writer
            .write_all(command.as_bytes())
            .context("writing to the QEMU monitor")?;
        writer
            .write_all(b"\n")
            .context("writing to the QEMU monitor")?;
        writer.flush().context("flushing the QEMU monitor")?;

        let mut reader = stream;
        let mut reply = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut chunk = [0u8; 4096];
            let received = reader
                .read(&mut chunk)
                .context("reading from the QEMU monitor")?;
            if received == 0 {
                break;
            }
            reply.push_str(&String::from_utf8_lossy(&chunk[..received]));
            if reply.trim_end().ends_with("(qemu)") {
                break;
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for the QEMU monitor prompt: {reply}");
            }
        }
        Ok(reply)
    }

    /// Capture the display device's surface into `out` as a PPM image.
    ///
    /// PPM rather than PNG on purpose: QEMU's PPM writer is a raw pixel dump
    /// with a text header, so it needs no decompressor to read back, while the
    /// PNG writer compresses with zlib. A golden test that wants to compare
    /// pixels should not have to implement an inflate.
    pub fn screendump_ppm(&self, out: &Path) -> Result<()> {
        let cmd = format!("screendump {} -f ppm", out.display());
        let reply = self.monitor_command(&cmd)?;
        if reply.contains("Error") || reply.contains("error") {
            bail!("the QEMU monitor rejected the screendump: {reply}");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !out.exists() {
                if Instant::now() >= deadline {
                    bail!("the QEMU monitor did not produce {}", out.display());
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let bytes = std::fs::read(out).with_context(|| format!("reading {}", out.display()))?;
            if let Some(expected) = ppm_size(&bytes) {
                if bytes.len() >= expected {
                    break;
                }
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for the screendump to finish writing {out:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    /// Move the emulated mouse by `(dx, dy)` pixels.
    ///
    /// This is the "scripted synthetic mouse move" the golden test needs: it
    /// produces the same PS/2 packets a real mouse would, so the kernel's
    /// input path is exercised end to end without a human at the keyboard.
    pub fn mouse_move(&self, dx: i32, dy: i32) -> Result<()> {
        let reply = self.monitor_command(&format!("mouse_move {dx} {dy}"))?;
        if reply.contains("Error") || reply.contains("error") {
            bail!("the QEMU monitor rejected the mouse move: {reply}");
        }
        Ok(())
    }
}

impl Drop for Vm {
    /// Terminate the machine.
    ///
    /// This runs on every path out of the owning scope, including the ones
    /// that unwind. Killing without waiting would leave a zombie, so the
    /// `wait` here is what makes the teardown complete rather than merely
    /// requested.
    fn drop(&mut self) {
        if self.reaped {
            // The machine stopped on its own, so QEMU removed its own monitor
            // socket as it exited.
            self.paths.remove_monitor_socket();
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped = true;
        // QEMU removes the monitor socket on a *clean* exit. A killed VM never
        // gets that far, so the socket file survives the process -- and the
        // boot smoke test's "no leaked VM" check looks for exactly that file.
        //
        // So the teardown removes it. The socket is a filesystem entry and
        // nothing else can be using it once the process is reaped.
        self.paths.remove_monitor_socket();
    }
}

/// Boot `image`, print the serial log, and return it.
///
/// `timeout_secs` is a hard ceiling. The VM is killed and waited on before
/// this function returns, in every case.
pub fn run(root: &Path, image: &Path, display: bool, timeout_secs: u64) -> Result<String> {
    let paths = Paths::tagged(root, "xtask");
    let mut vm = Vm::start(image, paths, display)?;

    let stopped = vm.wait_for(timeout_secs)?;
    if !stopped {
        println!("==> timeout after {timeout_secs}s; killing qemu");
    }

    let log = vm.serial_log();
    print_log(&log);

    // The loader's own log, when there is one. It is printed after the serial
    // log because the serial log is the one the boot smoke test asserts on;
    // this is context.
    let loader_log = vm.debugcon_log();
    if !loader_log.trim().is_empty() {
        println!("==> loader log (debugcon):");
        for line in strip_ansi(&loader_log).lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            println!("  {t}");
        }
    }

    // The loader's marker is reported for diagnostics: it says the loader ran
    // at all, which separates "the handoff was rejected" from "nothing ever
    // happened". It is deliberately not part of the success decision.
    if !loader_marker_present(&loader_log) {
        println!("==> the loader's marker is absent; the loader may not have run");
    }

    if !stopped {
        // A timeout is only a failure if the machine never reached a stable
        // state. The kernel has no reason to power itself off — it idles
        // forever so the desktop stays up — so a *successful* boot always
        // ends in a timeout. Killing the VM and reporting success is
        // therefore the normal path, not the exceptional one.
        //
        // The distinction is made on the kernel's own ready banner, which is
        // the same signal the boot smoke test asserts on: a kernel that
        // printed its banner booted, however it was stopped.
        //
        // The loader's marker is deliberately *not* enough. It only proves
        // the loader ran, and a loader that ran and then failed to hand off
        // would satisfy it — so treating it as success would turn a broken
        // handoff into a green run.
        if looks_like_success(&log) {
            println!(
                "==> kernel reached its ready banner; the VM was still running, so it is killed"
            );
            return Ok(log);
        }
        bail!(
            "qemu did not exit within {timeout_secs}s and the kernel never reached its \
             ready banner (log: {})",
            vm.paths().serial.display()
        );
    }

    // The machine stopped on its own. If the banner is there, that is a
    // program that called `PowerOff` after a successful boot, which is a
    // success.
    if looks_like_success(&log) {
        return Ok(log);
    }

    // It stopped *early* — a panic, a rejected handoff, or a user program
    // that exited before the desktop came up. A clean QEMU exit is not a
    // success on its own.
    bail!(
        "the guest exited without reaching the kernel's ready banner (log: {})",
        vm.paths().serial.display()
    )
}

fn print_log(log: &str) {
    if log.trim().is_empty() {
        println!("(serial log empty)");
        return;
    }
    // Strip ANSI escapes and the OVMF screen-drawing noise that dominates the
    // log, so the useful lines are readable.
    let cleaned = strip_ansi(log);
    let mut printed = 0;
    for line in cleaned.lines() {
        let t = line.trim();
        if t.is_empty() || t.chars().all(|c| c == ' ' || c == '\u{2500}') {
            continue;
        }
        println!("  {t}");
        printed += 1;
        if printed > 200 {
            println!("  ... (truncated)");
            break;
        }
    }
}

/// Remove ANSI escape sequences and carriage returns from a log.
///
/// OVMF's console output is full of cursor-movement escapes, and the kernel's
/// serial output is plain text with `CRLF` line endings. Both have to go
/// before a marker can be searched for.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI / other escape sequence: skip until a final byte.
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        if c != '\r' {
            out.push(c);
        }
    }
    out
}

/// True when the log contains every boot marker.
pub fn looks_like_success(log: &str) -> bool {
    let clean = strip_ansi(log);
    BOOT_MARKERS.iter().all(|m| clean.contains(m))
}

/// The total length a PPM file must have when its header is complete.
///
/// QEMU's screendump can return before its asynchronous write has caught up,
/// so the golden capture waits until the file contains every promised byte.
/// This parses only the `P6 width height maxval` header, which is enough to
/// derive the exact byte count.
fn ppm_size(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 3 || &bytes[..2] != b"P6" {
        return None;
    }

    let mut cursor = 2;
    let mut tokens = Vec::new();
    while tokens.len() < 3 {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return None;
        }
        if bytes[cursor] == b'#' {
            while cursor < bytes.len() && bytes[cursor] != b'\n' {
                cursor += 1;
            }
            continue;
        }
        let start = cursor;
        while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor == bytes.len() {
            return None;
        }
        tokens.push(String::from_utf8_lossy(&bytes[start..cursor]).into_owned());
    }

    let width: usize = tokens[0].parse().ok()?;
    let height: usize = tokens[1].parse().ok()?;
    tokens[2]
        .parse::<u32>()
        .ok()
        .filter(|maxval| *maxval == 255)?;
    let pixels = width.checked_mul(height)?.checked_mul(3)?;
    pixels.checked_add(cursor + 1)
}
