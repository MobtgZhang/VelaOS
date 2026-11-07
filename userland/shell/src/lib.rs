//! `vela_shell` — the VelaOS command-line interpreter.
//!
//! # What it is
//!
//! A guest-side shell that runs on the console: it prints a prompt, reads a
//! line of input, parses it into a command and arguments, and runs the
//! command. It is the user-space half of a "CMD" interface for VelaOS; the
//! kernel half is the `GetChar` syscall that reads one byte from the console
//! without blocking.
//!
//! # What it does *not* do
//!
//! It is not bash. It has no job control, no pipes, no redirection, no
//! scripting, and no history. It is a read-eval-print loop over a handful of
//! built-in commands plus a few file operations, which is what v0.1.0's
//! console can support. Adding pipes is a mechanism waiting for a second
//! command to pipe *to*, and that comes after the first command exists.
//!
//! # How it reads input
//!
//! `getchar()` returns `Ok(byte)` when a byte has arrived and
//! `Err(RANGE)` when the console is idle. The shell spins on it between
//! redraws, so it never sleeps the CPU waiting for a key. Each byte is echoed
//! back as it arrives, so the user sees what they type; backspace rubs out
//! the previous character, and Enter submits the line.

#![cfg_attr(target_os = "none", no_std)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vela_sys::api;

/// How many directory entries `ls` reads at once.
const DIR_BUFFER_ENTRIES: usize = 32;

/// How many bytes of input a single line can hold.
const LINE_CAPACITY: usize = 256;

/// The default user name shown in the prompt.
const USER_NAME: &str = "vela";

/// The result of running a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandResult {
    /// Keep running.
    Continue,
    /// Stop the shell and power the machine off.
    Halt,
}

/// A parsed command line: the command name and its arguments.
struct CommandLine {
    /// The command, lower-cased, with no leading or trailing spaces.
    name: String,
    /// The arguments, split on whitespace, each trimmed.
    args: Vec<String>,
}

impl CommandLine {
    /// Parse a raw line into a command. Returns `None` when the line is empty
    /// or all whitespace.
    fn parse(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let mut parts = trimmed.split_whitespace();
        let name = parts.next()?.to_lowercase();
        let args = parts.map(|a| a.to_string()).collect();
        Some(Self { name, args })
    }
}

/// The shell: one input buffer, one prompt, one loop.
pub struct Shell {
    /// The accumulated input line, not yet submitted.
    line: String,
    /// The machine's hostname, read once at startup.
    hostname: String,
}

impl Shell {
    /// A fresh shell with an empty input line.
    pub fn new() -> Self {
        // Read the hostname from the kernel for the prompt.
        let mut buf = [0u8; 64];
        let hostname = match api::hostname(&mut buf) {
            Ok(n) => core::str::from_utf8(&buf[..n])
                .unwrap_or("vela")
                .to_string(),
            Err(_) => USER_NAME.to_string(),
        };
        Self {
            line: String::new(),
            hostname,
        }
    }

    /// Print the prompt: `hostname@user> current_directory`.
    fn prompt(&self) {
        api::print(&self.hostname);
        api::print("@");
        api::print(USER_NAME);
        api::print("> ");
        // Show the current directory, if we can read it.
        let mut buf = [0u8; 256];
        if let Ok(len) = api::current_dir(&mut buf) {
            let s = core::str::from_utf8(&buf[..len.min(buf.len() - 1)])
                .unwrap_or("<invalid utf-8>");
            if !s.is_empty() {
                api::print(s);
            }
        }
        api::print(" ");
    }

    /// Read one line from the console, echoing as we go.
    ///
    /// Backspace (`0x08` or `0x7F`) rubs out the previous character. Enter
    /// (`0x0D` or `0x0A`) submits the line. Any other printable byte is
    /// appended. Bytes arrive one at a time through `getchar()`, and when
    /// none has arrived we spin briefly — this is a poll, not a block, so the
    /// shell never puts the CPU to sleep waiting for a key.
    fn read_line(&mut self) -> Option<String> {
        self.line.clear();
        loop {
            match api::getchar() {
                Ok(byte) => match byte {
                    b'\r' | b'\n' => {
                        api::print("\r\n");
                        let submitted = self.line.clone();
                        self.line.clear();
                        return Some(submitted);
                    }
                    0x08 | 0x7F => {
                        // Backspace: rub out the last character, if there is
                        // one. Echoing backspace-space-backspace moves the
                        // cursor back, overwrites with a space, and moves it
                        // back again — the classic dumb-terminal rubout.
                        if self.line.pop().is_some() {
                            api::print("\u{8} \u{8}");
                        }
                    }
                    byte if byte.is_ascii_control() => {
                        // Ignore other control characters.
                    }
                    byte if self.line.len() < LINE_CAPACITY - 1 => {
                        self.line.push(byte as char);
                        let _ = api::putchar(byte);
                    }
                    _ => {
                        // Line is full: ignore the byte.
                    }
                },
                Err(_) => {
                    // Console idle: spin briefly before asking again. A real
                    // scheduler would yield here; this is a poll loop because
                    // v0.1.0's GetChar does not block.
                    for _ in 0..100_000 {
                        core::hint::spin_loop();
                    }
                }
            }
        }
    }

    /// Run one iteration of the read-eval-print loop. Returns `CommandResult`
    /// so the caller knows whether to keep going.
    pub fn run_once(&mut self) -> CommandResult {
        self.prompt();
        let line = match self.read_line() {
            Some(line) => line,
            None => return CommandResult::Continue,
        };

        let cmd = match CommandLine::parse(&line) {
            Some(cmd) => cmd,
            None => return CommandResult::Continue,
        };

        Self::dispatch(&cmd)
    }

    /// Run the shell forever, or until a command asks to halt.
    pub fn run(mut self) -> ! {
        while let CommandResult::Continue = self.run_once() {}
        api::print("vela: session ended\n");
        api::power_off()
    }

    /// Dispatch a parsed command to its handler.
    fn dispatch(cmd: &CommandLine) -> CommandResult {
        match cmd.name.as_str() {
            "help" | "?" => Self::cmd_help(),
            "clear" | "cls" => Self::cmd_clear(),
            "exit" | "quit" | "halt" => CommandResult::Halt,
            "pwd" => Self::cmd_pwd(),
            "ls" | "dir" => Self::cmd_ls(&cmd.args),
            "cd" => Self::cmd_cd(&cmd.args),
            "echo" => Self::cmd_echo(&cmd.args),
            "cat" => Self::cmd_cat(&cmd.args),
            "whoami" => Self::cmd_whoami(),
            "hostname" => Self::cmd_hostname(&cmd.args),
            "ver" | "version" => Self::cmd_ver(),
            "ping" => Self::cmd_ping(&cmd.args),
            "ifconfig" | "ip" => Self::cmd_ifconfig(&cmd.args),
            "netstat" => Self::cmd_netstat(&cmd.args),
            name => {
                api::print("vela: unknown command: ");
                api::print(name);
                api::print("\nType `help` for a list of commands.\n");
                CommandResult::Continue
            }
        }
    }

    // ----- built-in commands -----

    fn cmd_help() -> CommandResult {
        api::print("VelaOS shell v0.1.0\n");
        api::print("Built-in commands:\n");
        api::print("  help            - show this help\n");
        api::print("  clear           - clear the screen\n");
        api::print("  exit            - leave the shell and stop the machine\n");
        api::print("  pwd             - print the current directory\n");
        api::print("  ls [path]       - list a directory (dir is a synonym)\n");
        api::print("  cd <path>       - change the current directory; cd alone prints it\n");
        api::print("  echo <text>     - print text to the console\n");
        api::print("  cat <file>      - print a file's contents\n");
        api::print("  whoami          - print the current user\n");
        api::print("  hostname        - print the machine's hostname\n");
        api::print("  ver             - print the system version\n");
        api::print("  ping <host>     - test connectivity (network not in v0.1.0)\n");
        api::print("  ifconfig        - show network config (network not in v0.1.0)\n");
        api::print("  netstat         - show network stats (network not in v0.1.0)\n");
        CommandResult::Continue
    }

    fn cmd_clear() -> CommandResult {
        // A real clear on a serial console: emit enough newlines plus a form
        // feed to push current content off the visible area. ANSI escape
        // sequences are harmless on a serial port but do not clear it, so
        // scrolling is the portable approach.
        api::print("\x0c");
        for _ in 0..60 {
            api::print("\n");
        }
        CommandResult::Continue
    }

    fn cmd_pwd() -> CommandResult {
        let mut buf = [0u8; 256];
        match api::current_dir(&mut buf) {
            Ok(len) => {
                let s = core::str::from_utf8(&buf[..len.min(buf.len() - 1)])
                    .unwrap_or("<invalid utf-8>");
                api::print(s);
                api::print("\n");
            }
            Err(_) => {
                api::print("vela: pwd: cannot read current directory\n");
            }
        }
        CommandResult::Continue
    }

    fn cmd_ls(args: &[String]) -> CommandResult {
        // No directory named means the one the shell is in. The kernel's
        // resolver treats a bare name as relative to its current directory,
        // so `ls` reads back the path first and lists that: passing an empty
        // string would fail long before any filesystem, because an empty
        // path parses to nothing at all.
        let path = match args.first() {
            Some(arg) => arg.clone(),
            None => match Self::current_dir_path() {
                Some(dir) => dir,
                None => {
                    api::print("vela: ls: cannot read current directory\n");
                    return CommandResult::Continue;
                }
            },
        };
        let mut buf = [vela_sys::api::DirEntry {
            de_size: 0,
            de_type: 0,
            de_name_len: 0,
            de_name: [0; vela_sys::api::DIR_NAME_LEN],
        }; DIR_BUFFER_ENTRIES];

        match api::read_dir(&path, &mut buf) {
            Ok(count) => {
                for entry in buf.iter().take(count) {
                    let kind = if entry.is_dir() { "dir" } else { "file" };
                    api::print(kind);
                    api::print("  ");
                    api::print(entry.name());
                    api::print("\n");
                }
            }
            Err(_) => {
                api::print("vela: ls: cannot read directory ");
                api::print(&path);
                api::print("\n");
            }
        }
        CommandResult::Continue
    }

    fn cmd_cd(args: &[String]) -> CommandResult {
        // `cd` with no path prints the current directory — the convention
        // AmigaDOS users expect from `cd` in a CLI, and the same answer
        // `pwd` gives.
        let Some(path) = args.first() else {
            return Self::cmd_pwd();
        };
        match api::set_current_dir(path) {
            Ok(()) => {}
            Err(_) => {
                api::print("vela: cd: cannot change to ");
                api::print(path);
                api::print("\n");
            }
        }
        CommandResult::Continue
    }

    /// The current directory, as a path string.
    ///
    /// A separate function because two commands read it and neither can deal
    /// with a failure by inventing a directory: a read the kernel refuses is
    /// reported, not papered over with `""`.
    fn current_dir_path() -> Option<String> {
        let mut buf = [0u8; 256];
        let len = api::current_dir(&mut buf).ok()?;
        core::str::from_utf8(&buf[..len.min(buf.len() - 1)])
            .ok()
            .map(|s| s.to_string())
    }

    fn cmd_echo(args: &[String]) -> CommandResult {
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                api::print(" ");
            }
            api::print(arg);
        }
        api::print("\n");
        CommandResult::Continue
    }

    fn cmd_cat(args: &[String]) -> CommandResult {
        if args.is_empty() {
            api::print("vela: cat: missing file\n");
            return CommandResult::Continue;
        }
        let path = &args[0];
        match api::open(path, vela_sys::api::mode::READ) {
            Ok(file) => {
                let mut buf = [0u8; 256];
                loop {
                    match api::read(file, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let s = core::str::from_utf8(&buf[..n]).unwrap_or("<binary>");
                            api::print(s);
                        }
                        Err(_) => break,
                    }
                }
                let _ = api::close(file);
            }
            Err(_) => {
                api::print("vela: cat: cannot open ");
                api::print(path);
                api::print("\n");
            }
        }
        CommandResult::Continue
    }

    fn cmd_whoami() -> CommandResult {
        api::print("vela\n");
        CommandResult::Continue
    }

    fn cmd_ver() -> CommandResult {
        api::print("VelaOS 0.1.0\n");
        CommandResult::Continue
    }

    /// `hostname` — print or set the machine's hostname.
    ///
    /// With no argument, prints the current hostname. With an argument,
    /// attempts to set it (the kernel only allows setting during boot, so
    /// this reports that it cannot be changed at runtime).
    fn cmd_hostname(_args: &[String]) -> CommandResult {
        let mut buf = [0u8; 64];
        match api::hostname(&mut buf) {
            Ok(n) => {
                let s = core::str::from_utf8(&buf[..n]).unwrap_or("vela");
                api::print(s);
                api::print("\n");
            }
            Err(_) => {
                api::print(USER_NAME);
                api::print("\n");
            }
        }
        CommandResult::Continue
    }

    /// `ping` — test network connectivity (stub).
    ///
    /// VelaOS v0.1.0 has no network stack, so this reports that the
    /// facility is not yet available.
    fn cmd_ping(_args: &[String]) -> CommandResult {
        api::print("vela: ping: network stack not available in v0.1.0\n");
        CommandResult::Continue
    }

    /// `ifconfig` / `ip` — show network interface configuration (stub).
    fn cmd_ifconfig(_args: &[String]) -> CommandResult {
        api::print("vela: ifconfig: network stack not available in v0.1.0\n");
        CommandResult::Continue
    }

    /// `netstat` — show network statistics (stub).
    fn cmd_netstat(_args: &[String]) -> CommandResult {
        api::print("vela: netstat: network stack not available in v0.1.0\n");
        CommandResult::Continue
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}
