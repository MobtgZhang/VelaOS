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

/// How many directory entries a listing reads at once.
const DIR_BUFFER_ENTRIES: usize = 32;

/// How many bytes of input a single line can hold.
const LINE_CAPACITY: usize = 256;

/// The default user name shown in the prompt.
const USER_NAME: &str = "vela";

/// The banner printed when the shell starts.
///
/// "VelaOS" in the classic figlet `big` style. Written out rather than
/// composed at runtime because the shell has no font of its own and the
/// console renders one byte per cell: an ASCII drawing is the only drawing
/// available. The exact spelling is kept out of `ver`'s answer so a boot
/// test that waits for the version string can still trust it came from a
/// typed command rather than from this banner.
const BANNER: &str = r"__      __  _        ____   _____
\ \    / / | |      / __ \ / ____|
 \ \  / /__| | __ _| |  | | (___
  \ \/ / _ \ |/ _` | |  | |\___ \
   \  /  __/ | (_| | |__| |____) |
    \/ \___|_|\__,_|\____/|_____/ ";

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

    /// Print the startup banner.
    ///
    /// The drawing first, then one line naming the system and a blank line
    /// so the first prompt starts clear of the art. Printed once, when the
    /// shell starts — a prompt that redrew the banner on every command
    /// would scroll it away on anything but an empty screen.
    fn print_banner() {
        api::print(BANNER);
        api::print("\n");
        api::print("An AmigaOS-inspired operating system\n\n");
    }

    /// Print the prompt: `hostname@username:> `.
    ///
    /// The form is the shell's own: machine, user, and a colon-terminated
    /// separator, the way a shell that runs *before* a filesystem may not
    /// have a meaningful current directory to show. Where the shell is is
    /// what `cd` alone answers, so the prompt stays out of the business of
    /// repeating it.
    fn prompt(&self) {
        api::print(&self.hostname);
        api::print("@");
        api::print(USER_NAME);
        api::print(":> ");
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
        Self::print_banner();
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
            "list" | "ls" => Self::cmd_list(&cmd.args),
            "dir" => Self::cmd_dir(&cmd.args),
            "cd" => Self::cmd_cd(&cmd.args),
            "echo" => Self::cmd_echo(&cmd.args),
            "cat" => Self::cmd_cat("cat", &cmd.args),
            "type" => Self::cmd_cat("type", &cmd.args),
            "makedir" | "md" => Self::cmd_makedir(&cmd.args),
            "delete" | "rm" | "erase" => Self::cmd_delete(&cmd.args),
            "rename" | "mv" => Self::cmd_rename(&cmd.args),
            "copy" | "cp" => Self::cmd_copy(&cmd.args),
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
        api::print("  list [path]     - list a directory in detail (ls is a synonym)\n");
        api::print("  dir [path]      - list a directory's names only\n");
        api::print("  cd <path>       - change the current directory; cd alone prints it\n");
        api::print("  makedir <path>  - create a directory (md is a synonym)\n");
        api::print("  delete <path>   - delete a file or directory (rm, erase)\n");
        api::print("  rename <a> <b>  - rename a file or directory (mv)\n");
        api::print("  copy <a> <b>    - copy a file (cp)\n");
        api::print("  echo <text>     - print text to the console\n");
        api::print("  type <file>     - print a file's contents (cat is a synonym)\n");
        api::print("  whoami          - print the current user\n");
        api::print("  hostname        - print the machine's hostname\n");
        api::print("  ver             - print the system version\n");
        api::print("  ping <host>     - test connectivity (network not in v0.1.0)\n");
        api::print("  ifconfig        - show network config (network not in v0.1.0)\n");
        api::print("  netstat         - show network stats (network not in v0.1.0)\n");
        CommandResult::Continue
    }

    fn cmd_clear() -> CommandResult {
        // The `ClearScreen` syscall: the framebuffer console erases its grid
        // and homes its cursor, and the serial log gets the ANSI sequence.
        // Printing newlines here instead would scroll rather than clear,
        // which is the bug this call exists to avoid.
        let _ = api::clear_screen();
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

    /// List a directory, one name per line.
    ///
    /// `dir` is the AmigaOS quick look: names only, with a directory's
    /// entries marked so a user can tell what `cd` would accept. The
    /// detailed listing is [`Shell::cmd_list`].
    fn cmd_dir(args: &[String]) -> CommandResult {
        let Some(path) = Self::listing_path("dir", args) else {
            return CommandResult::Continue;
        };
        match Self::read_entries(&path) {
            Ok(entries) => {
                for entry in &entries {
                    api::print(entry.name());
                    if entry.is_dir() {
                        api::print(" (dir)");
                    }
                    api::print("\n");
                }
            }
            Err(()) => Self::report_list_error("dir", &path),
        }
        CommandResult::Continue
    }

    /// List a directory in detail, the way AmigaOS's `List` does.
    ///
    /// One line per entry — name, kind, size — followed by a count, so the
    /// summary a `List` ends with survives too. Directories report their
    /// kind instead of a size, because a directory's size is an internal
    /// detail of the filesystem rather than something a user asked for.
    fn cmd_list(args: &[String]) -> CommandResult {
        let Some(path) = Self::listing_path("list", args) else {
            return CommandResult::Continue;
        };
        match Self::read_entries(&path) {
            Ok(entries) => {
                let mut files = 0u32;
                let mut dirs = 0u32;
                for entry in &entries {
                    api::print(&Self::padded(entry.name(), 24));
                    if entry.is_dir() {
                        api::print("<dir>");
                        dirs += 1;
                    } else {
                        api::print_int(entry.de_size);
                        files += 1;
                    }
                    api::print("\n");
                }
                api::print_int(files as u64);
                api::print(" files, ");
                api::print_int(dirs as u64);
                api::print(" directories\n");
            }
            Err(()) => Self::report_list_error("list", &path),
        }
        CommandResult::Continue
    }

    /// The path a listing command was given, or the current directory.
    ///
    /// No directory named means the one the shell is in. The kernel's
    /// resolver treats a bare name as relative to its current directory, so
    /// `dir` reads back the path first and lists that: passing an empty
    /// string would fail long before any filesystem, because an empty
    /// path parses to nothing at all.
    fn listing_path(command: &str, args: &[String]) -> Option<String> {
        match args.first() {
            Some(arg) => Some(arg.clone()),
            None => match Self::current_dir_path() {
                Some(dir) => Some(dir),
                None => {
                    api::print("vela: ");
                    api::print(command);
                    api::print(": cannot read current directory\n");
                    None
                }
            },
        }
    }

    /// Read a directory's entries, with the kind and size of each.
    fn read_entries(path: &str) -> Result<Vec<vela_sys::api::DirEntry>, ()> {
        let mut buf = [vela_sys::api::DirEntry {
            de_size: 0,
            de_type: 0,
            de_name_len: 0,
            de_name: [0; vela_sys::api::DIR_NAME_LEN],
        }; DIR_BUFFER_ENTRIES];
        match api::read_dir(path, &mut buf) {
            Ok(count) => Ok(buf.iter().take(count).cloned().collect()),
            Err(_) => Err(()),
        }
    }

    /// The message a failed listing prints.
    fn report_list_error(command: &str, path: &str) {
        api::print("vela: ");
        api::print(command);
        api::print(": cannot read directory ");
        api::print(path);
        api::print("\n");
    }

    /// A string padded on the right to a minimum width.
    ///
    /// For the column of names a `list` aligns. Bytes, not chars, because
    /// the console is a byte grid and a name's *displayed* width is its
    /// byte count.
    fn padded(s: &str, width: usize) -> String {
        let mut out = String::from(s);
        while out.len() < width {
            out.push(' ');
        }
        out
    }

    /// `makedir` — create a directory.
    fn cmd_makedir(args: &[String]) -> CommandResult {
        let Some(path) = args.first() else {
            api::print("vela: makedir: missing directory name\n");
            return CommandResult::Continue;
        };
        match api::create_dir(path) {
            Ok(()) => {}
            // "Already there" is the outcome `makedir` wanted, so it is
            // reported as success the way init's boot-time creation does.
            Err(e) if e.code == vela_sys::api::errors::OBJECT_IN_USE => {}
            Err(_) => {
                api::print("vela: makedir: cannot create ");
                api::print(path);
                api::print("\n");
            }
        }
        CommandResult::Continue
    }

    /// `delete` — remove a file or directory, one path per argument.
    fn cmd_delete(args: &[String]) -> CommandResult {
        if args.is_empty() {
            api::print("vela: delete: missing file\n");
            return CommandResult::Continue;
        }
        for path in args {
            match api::delete_file(path) {
                Ok(()) => {}
                Err(_) => {
                    api::print("vela: delete: cannot delete ");
                    api::print(path);
                    api::print("\n");
                }
            }
        }
        CommandResult::Continue
    }

    /// `rename` — give a file or directory a new name.
    fn cmd_rename(args: &[String]) -> CommandResult {
        if args.len() < 2 {
            api::print("vela: rename: usage: rename <from> <to>\n");
            return CommandResult::Continue;
        }
        let from = &args[0];
        let to = &args[1];
        match api::rename(from, to) {
            Ok(()) => {}
            Err(_) => {
                api::print("vela: rename: cannot rename ");
                api::print(from);
                api::print(" to ");
                api::print(to);
                api::print("\n");
            }
        }
        CommandResult::Continue
    }

    /// `copy` — duplicate a file's contents under a new name.
    fn cmd_copy(args: &[String]) -> CommandResult {
        if args.len() < 2 {
            api::print("vela: copy: usage: copy <from> <to>\n");
            return CommandResult::Continue;
        }
        let from = &args[0];
        let to = &args[1];
        let source = match api::open(from, vela_sys::api::mode::READ) {
            Ok(file) => file,
            Err(_) => {
                api::print("vela: copy: cannot open ");
                api::print(from);
                api::print("\n");
                return CommandResult::Continue;
            }
        };
        let target = match api::open(to, vela_sys::api::mode::WRITE) {
            Ok(file) => file,
            Err(_) => {
                let _ = api::close(source);
                api::print("vela: copy: cannot create ");
                api::print(to);
                api::print("\n");
                return CommandResult::Continue;
            }
        };
        let mut buf = [0u8; 256];
        loop {
            match api::read(source, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if api::write(target, &buf[..n]).is_err() {
                        api::print("vela: copy: cannot write ");
                        api::print(to);
                        api::print("\n");
                        break;
                    }
                }
                Err(_) => {
                    api::print("vela: copy: cannot read ");
                    api::print(from);
                    api::print("\n");
                    break;
                }
            }
        }
        let _ = api::close(source);
        let _ = api::close(target);
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

    /// `cat` / `type` — print a file's contents.
    ///
    /// One handler for the two names: `cat` is the Unix spelling this
    /// shell grew up with and `type` the AmigaDOS one, and the only
    /// difference between them is which name errors report.
    fn cmd_cat(name: &str, args: &[String]) -> CommandResult {
        if args.is_empty() {
            api::print("vela: ");
            api::print(name);
            api::print(": missing file\n");
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
                api::print("vela: ");
                api::print(name);
                api::print(": cannot open ");
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
