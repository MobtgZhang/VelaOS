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

/// The prompt printed before each line.
const PROMPT: &str = "vela> ";

/// How many bytes of input a single line can hold.
const LINE_CAPACITY: usize = 256;

/// How many directory entries `ls` reads at once.
const DIR_BUFFER_ENTRIES: usize = 32;

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
}

impl Shell {
    /// A fresh shell with an empty input line.
    pub fn new() -> Self {
        Self {
            line: String::new(),
        }
    }

    /// Print the prompt.
    fn prompt(&self) {
        api::print(PROMPT);
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
            "ls" => Self::cmd_ls(&cmd.args),
            "cd" => Self::cmd_cd(&cmd.args),
            "echo" => Self::cmd_echo(&cmd.args),
            "cat" => Self::cmd_cat(&cmd.args),
            "whoami" => Self::cmd_whoami(),
            "ver" | "version" => Self::cmd_ver(),
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
        api::print("  ls [path]       - list a directory\n");
        api::print("  cd <path>       - change the current directory\n");
        api::print("  echo <text>     - print text to the console\n");
        api::print("  cat <file>      - print a file's contents\n");
        api::print("  whoami          - print the current user\n");
        api::print("  ver             - print the system version\n");
        CommandResult::Continue
    }

    fn cmd_clear() -> CommandResult {
        // Print enough newlines to push the current content off the top of a
        // typical 80x25 or 80x50 console. Not a real clear, but good enough.
        for _ in 0..40 {
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
        let path = args.first().map(String::as_str).unwrap_or("");
        let mut buf = [vela_sys::api::DirEntry {
            de_size: 0,
            de_type: 0,
            de_name_len: 0,
            de_name: [0; vela_sys::api::DIR_NAME_LEN],
        }; DIR_BUFFER_ENTRIES];

        match api::read_dir(path, &mut buf) {
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
                api::print(path);
                api::print("\n");
            }
        }
        CommandResult::Continue
    }

    fn cmd_cd(args: &[String]) -> CommandResult {
        if args.is_empty() {
            api::print("vela: cd: missing path\n");
            return CommandResult::Continue;
        }
        // VelaOS does not have a per-process current directory syscall that
        // changes it: `current_dir` only reads. Recording the directory here
        // would be a shell-side fiction, so say so rather than pretend.
        api::print("vela: cd: changing directories is not implemented in v0.1.0\n");
        CommandResult::Continue
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
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}
