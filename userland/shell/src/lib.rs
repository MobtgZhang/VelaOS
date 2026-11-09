//! `vela_shell` — the VelaOS command-line interpreter.
//!
//! # What it is
//!
//! An AmigaDOS shell, in the shape AROS's shell has: commands are named the
//! way AmigaDOS names them (`Dir`, `List`, `MakeDir`, `Type`), arguments are
//! given as keywords (`Copy FROM=RAM:a TO=RAM:b`), and every command
//! publishes its own argument template, so `Copy ?` prints how `Copy` wants
//! to be called rather than leaving the caller to guess.
//!
//! # What it does *not* do
//!
//! It is not bash. There are no pipes, no redirection, and no job control;
//! background starts exist as `Run`, which reports honestly that v0.1.0
//! cannot load a second program yet. Command names and keywords are matched
//! case-insensitively, the way AmigaDOS matches them, and arguments are
//! separated by spaces — a path with a space in it goes in double quotes.
//!
//! # Return codes
//!
//! Every command answers with a number, in the AmigaDOS bands: 0–4 is
//! success, 5–9 a warning, 10–19 an error, and 20 or more a failure. A
//! script's `IF` can test them, and `FailAt`-style thresholds are a
//! script-runner concern this shell does not need yet.
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
use alloc::vec;
use alloc::vec::Vec;

use vela_sys::api;

/// How many directory entries a listing reads at once.
///
/// Sixty-four, because the seeded `SYS:` tree holds more directories than
/// the thirty-two a smaller listing could show, and a listing that silently
/// stops early looks like a complete one. The buffer is allocated on the
/// heap rather than the stack: sixty-four entries is fifteen kilobytes, and
/// a guest stack is not where that belongs.
const DIR_BUFFER_ENTRIES: usize = 64;

/// How many assigns one listing reads at once.
///
/// The system's own assigns number about twenty; the rest of the room is
/// for the ones a session adds. Heap-allocated, for the same reason the
/// directory buffer is.
const ASSIGN_BUFFER_ENTRIES: usize = 32;

/// How many bytes of input a single line can hold.
const LINE_CAPACITY: usize = 256;

/// The default user name shown in the prompt.
const USER_NAME: &str = "vela";

/// How deeply `Execute` may nest scripts.
///
/// A script that runs a script that runs a script is legitimate; one that
/// runs *itself* is a loop, and without a bound the loop is unbounded. Four
/// levels is enough for any real script and small enough that the stack
/// survives the deepest legal nesting.
const MAX_SCRIPT_DEPTH: u32 = 4;

/// The banner printed when the shell starts.
///
/// "VelaOS" in the classic figlet `big` style. Written out rather than
/// composed at runtime because the shell has no font of its own and the
/// console renders one byte per cell: an ASCII drawing is the only drawing
/// available. The exact spelling is kept out of `Version`'s answer so a boot
/// test that waits for the version string can still trust it came from a
/// typed command rather than from this banner.
const BANNER: &str = r"__      __  _        ____   _____
\ \    / / | |      / __ \ / ____|
 \ \  / /__| | __ _| |  | | (___
  \ \/ / _ \ |/ _` | |  | |\___ \
   \  /  __/ | (_| | |__| |____) |
    \/ \___|_|\__,_|\____/|_____/ ";

/// The result of running a command: whether to keep going, and the command's
/// AmigaDOS return code.
///
/// The code is produced on every path — including usage errors, which answer
/// with `FAILURE` — because a code a caller cannot see is a code the script
/// runner cannot test with `IF`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Keep running, or stop the shell.
    pub result: CommandResult,
    /// The AmigaDOS return code: 0–4 success, 5–9 warning, 10–19 error,
    /// 20+ failure.
    pub code: i32,
}

/// Return-code bands, in the AmigaDOS convention.
mod rc {
    /// Success.
    pub const OK: i32 = 0;
    /// Success, with something worth noting.
    pub const WARN: i32 = 5;
    /// The operation failed.
    pub const ERROR: i32 = 10;
    /// The command could not run at all — a usage error.
    pub const FAILURE: i32 = 20;
}

use rc::{ERROR, FAILURE, OK, WARN};

impl Outcome {
    /// A successful command that changes nothing.
    const OK: Outcome = Outcome {
        result: CommandResult::Continue,
        code: OK,
    };
    /// A command that failed, banding the code in with any earlier one.
    fn failed(code: i32) -> Self {
        Outcome {
            result: CommandResult::Continue,
            code,
        }
    }
}

/// Keep running, or stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandResult {
    Continue,
    /// Stop the shell and power the machine off.
    Halt,
}

// ---------------------------------------------------------------------------
// ReadArgs: the argument parser behind every template
// ---------------------------------------------------------------------------

/// One argument of a command's template.
///
/// The template syntax is the AmigaDOS one: a name, optionally preceded by
/// aliases joined with `=`, followed by modifiers — `/A` required, `/K`
/// keyword-only, `/S` switch, `/N` numeric, `/M` multiple values, `/F` the
/// rest of the line. `Copy FROM/A,TO/A,ALL/S` names two required arguments
/// and a switch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArgDef {
    /// Every name the argument answers to, case-insensitively. The *last*
    /// one is the canonical spelling; the ones before it are aliases, the
    /// way `BUF=BUFFER` lets `Copy BUF=20` spell `BUFFER=20`.
    names: Vec<String>,
    /// Required: the command refuses to run without it.
    required: bool,
    /// Keyword-only: it must be given as `NAME=value`, never positionally.
    keyword_only: bool,
    /// Switch: its presence is the value.
    switch: bool,
    /// Numeric: the value must parse as a number.
    numeric: bool,
    /// Multiple: it collects the positionals that follow it.
    multiple: bool,
    /// Rest of line: it takes everything the other arguments left behind.
    rest: bool,
}

impl ArgDef {
    /// Parse one comma-separated template entry.
    fn parse(spec: &str) -> Self {
        let mut parts = spec.split('/');
        let head = parts.next().unwrap_or("");
        let names: Vec<String> = head
            .split('=')
            .filter(|n| !n.is_empty())
            .map(|n| n.to_string())
            .collect();
        let mut def = Self {
            names,
            required: false,
            keyword_only: false,
            switch: false,
            numeric: false,
            multiple: false,
            rest: false,
        };
        for modifier in parts {
            match modifier {
                "A" => def.required = true,
                "K" => def.keyword_only = true,
                "S" => def.switch = true,
                "N" => def.numeric = true,
                "M" => def.multiple = true,
                "F" => def.rest = true,
                _ => {}
            }
        }
        def
    }

    /// True when the argument answers to this name.
    fn matches(&self, name: &str) -> bool {
        self.names.iter().any(|n| n.eq_ignore_ascii_case(name))
    }

    /// The argument's canonical name, for error messages.
    fn canonical(&self) -> &str {
        self.names.last().map(|s| s.as_str()).unwrap_or("")
    }
}

/// How ReadArgs failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// A required argument was not given, by name.
    Missing(String),
    /// A `/N` argument was given something that is not a number, by name.
    NotNumeric(String),
    /// More positionals arrived than the template has slots for.
    TooMany,
}

/// A parsed command line, matched against a template.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// One slot per template argument, parallel to the template's order.
    slots: Vec<Option<Vec<String>>>,
    /// The template the slots were filled from.
    defs: Vec<ArgDef>,
}

impl Args {
    /// The first value given for an argument, if any.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values_of(name)?.first().map(|s| s.as_str())
    }

    /// Every value given for an argument, in the order given.
    pub fn all(&self, name: &str) -> Vec<String> {
        self.values_of(name).cloned().unwrap_or_default()
    }

    /// True when a switch was present.
    pub fn switch(&self, name: &str) -> bool {
        self.values_of(name).is_some()
    }

    /// The first value of an argument, parsed as a number.
    pub fn number(&self, name: &str) -> Option<u64> {
        self.get(name)?.parse().ok()
    }

    fn values_of(&self, name: &str) -> Option<&Vec<String>> {
        let index = self.defs.iter().position(|def| def.matches(name))?;
        self.slots.get(index)?.as_ref()
    }
}

/// Split a raw line into tokens, honouring double quotes.
///
/// AmigaDOS arguments are space-separated and a path with a space in it goes
/// in double quotes; the quotes group the value and are stripped, so the
/// value a command sees is the path itself, not the quotation marks.
pub fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    for ch in line.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !token.is_empty() {
                    tokens.push(core::mem::take(&mut token));
                }
            }
            c => token.push(c),
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

/// Parse a template string into its arguments.
fn parse_template(template: &str) -> Vec<ArgDef> {
    template
        .split(',')
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
        .map(ArgDef::parse)
        .collect()
}

/// ReadArgs: match a command's tokens against its template.
///
/// The order of matching is the order ReadArgs uses:
///
/// 1. `NAME=value` — an explicit keyword binding. Switches accept it too,
///    because a script that wants to be explicit about a switch should not
///    have to know the difference.
/// 2. `NAME value` — the two-token keyword form, which AmigaDOS accepts
///    alongside the `=` form.
/// 3. A bare switch name — `Copy a b ALL` — turns the switch on.
/// 4. Anything else is positional, filling the template's non-keyword,
///    non-switch arguments left to right. A `/M` argument collects every
///    positional that follows it; a `/F` argument takes the rest of the
///    line.
pub fn read_args(tokens: &[String], template: &str) -> Result<Args, ParseError> {
    let defs = parse_template(template);
    let mut slots: Vec<Option<Vec<String>>> = defs.iter().map(|_| None).collect();

    // The index of the argument a bare positional lands in, remembered so a
    // `/M` argument collects the positionals that follow it.
    let mut positional = 0;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];

        // 1. NAME=value.
        if let Some(eq) = token.find('=') {
            let (key, value) = token.split_at(eq);
            let value = &value[1..];
            if let Some(slot) = slot_of(&defs, key) {
                bind_at(&mut slots, slot, &defs[slot], value);
                index += 1;
                continue;
            }
        }

        // 2. NAME value, for everything but switches.
        if let Some(slot) = slot_of(&defs, token) {
            if !defs[slot].switch {
                match tokens.get(index + 1) {
                    Some(value) => {
                        bind_at(&mut slots, slot, &defs[slot], value);
                        index += 2;
                        continue;
                    }
                    None => return Err(ParseError::Missing(defs[slot].canonical().to_string())),
                }
            }
        }

        // 3. A bare switch name.
        if let Some(slot) = slot_of(&defs, token) {
            if defs[slot].switch {
                slots[slot] = Some(Vec::new());
                index += 1;
                continue;
            }
        }

        // 4. Positional, filling left to right among the arguments that
        // accept a positional at all. A `/M` argument keeps collecting, so
        // a filled multiple slot is not a reason to skip it.
        while positional < defs.len()
            && ((slots[positional].is_some() && !defs[positional].multiple)
                || defs[positional].keyword_only
                || defs[positional].switch)
        {
            positional += 1;
        }
        match defs.get(positional) {
            None => return Err(ParseError::TooMany),
            Some(def) => {
                if def.rest {
                    // The rest of the line, spaces and all: the tokens are
                    // rejoined with the spaces that separated them, which is
                    // what a free-text argument such as Echo's wants.
                    slots[positional] = Some(vec![tokens[index..].join(" ")]);
                    index = tokens.len();
                    continue;
                }
                bind_at(&mut slots, positional, def, token);
                if !def.multiple {
                    positional += 1;
                }
                index += 1;
            }
        }
    }

    // Every required argument must be there, and every `/N` value must be a
    // number. Both are the caller's mistake, and both are reported as the
    // failure they are.
    for (def, slot) in defs.iter().zip(slots.iter()) {
        if def.required && slot.is_none() {
            return Err(ParseError::Missing(def.canonical().to_string()));
        }
        if let (true, Some(values)) = (def.numeric, slot) {
            if values.iter().any(|v| v.parse::<u64>().is_err()) {
                return Err(ParseError::NotNumeric(def.canonical().to_string()));
            }
        }
    }

    Ok(Args { slots, defs })
}

/// Bind one value into the slot at `index`.
///
/// A multiple-value argument accumulates; every other kind replaces what it
/// had, so a repeated keyword answers with its last value.
fn bind_at(slots: &mut [Option<Vec<String>>], index: usize, def: &ArgDef, value: &str) {
    match &mut slots[index] {
        Some(values) if def.multiple => values.push(value.to_string()),
        slot => *slot = Some(vec![value.to_string()]),
    }
}

/// The slot that answers to a keyword, if the template defines one.
fn slot_of(defs: &[ArgDef], name: &str) -> Option<usize> {
    defs.iter().position(|def| def.matches(name))
}

// ---------------------------------------------------------------------------
// The DOS pattern matcher
// ---------------------------------------------------------------------------

/// Match text against an AmigaDOS pattern: `?` is one character, `#?` is any
/// number of them, `#X` is any number of X, and `[a-c]` is a character
/// range. Matching is case-insensitive, the way AmigaDOS matches names.
///
/// The pattern is matched against the *whole* name: a caller that wants
/// "anything ending in .info" writes `#?.info`, not `.info`.
pub fn dos_match(pattern: &str, text: &str) -> bool {
    match_from(pattern.as_bytes(), text.as_bytes())
}

/// One element of a pattern: what a single position may match.
enum Element {
    /// Any one character.
    Any,
    /// One of the characters (or ranges) inside `[...]`.
    Class(Vec<(u8, u8)>),
    /// One literal character.
    Byte(u8),
}

/// Parse one pattern element off the front of a pattern.
///
/// Returns the element and the pattern that follows it. `#` is *not* an
/// element — it is a modifier that means "any number of the element that
/// follows" — so the caller handles it before parsing. An empty pattern
/// never reaches here: the matcher answers the base case before parsing.
fn parse_element(pattern: &[u8]) -> (Element, &[u8]) {
    match pattern.first() {
        Some(b'?') => (Element::Any, &pattern[1..]),
        Some(b'[') => {
            let mut ranges = Vec::new();
            let mut rest = &pattern[1..];
            while let Some(&c) = rest.first() {
                if c == b']' {
                    rest = &rest[1..];
                    break;
                }
                // A range is two characters with a `-` between them, and a
                // `-` at the end of the class is a literal dash, the way
                // character classes usually settle it.
                if rest.len() >= 3 && rest[1] == b'-' && rest[2] != b']' {
                    ranges.push((c, rest[2]));
                    rest = &rest[3..];
                } else {
                    ranges.push((c, c));
                    rest = &rest[1..];
                }
            }
            (Element::Class(ranges), rest)
        }
        Some(&c) => (Element::Byte(ascii_fold(c)), &pattern[1..]),
        None => (Element::Any, pattern),
    }
}

/// Fold one byte to lower case, ASCII only, the way AmigaDOS folds names.
fn ascii_fold(c: u8) -> u8 {
    c.to_ascii_lowercase()
}

/// True when a byte is inside one of a class's ranges.
fn in_class(ranges: &[(u8, u8)], c: u8) -> bool {
    let c = ascii_fold(c);
    ranges
        .iter()
        .any(|&(low, high)| ascii_fold(low) <= c && c <= ascii_fold(high))
}

/// True when one element matches one character.
fn element_matches(element: &Element, c: u8) -> bool {
    match element {
        Element::Any => true,
        Element::Class(ranges) => in_class(ranges, c),
        Element::Byte(b) => *b == ascii_fold(c),
    }
}

/// How long a run of one element the text offers, from the front.
fn element_run(element: &Element, text: &[u8]) -> usize {
    text.iter()
        .take_while(|&&c| element_matches(element, c))
        .count()
}

/// Match a pattern against text, recursively.
///
/// `#element` is the interesting case: it matches zero or more copies of the
/// element, so the matcher tries every run length the text can offer.
/// Patterns are short and names are short, so the backtracking is not a cost
/// anyone pays in practice.
fn match_from(pattern: &[u8], text: &[u8]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    if pattern[0] == b'#' {
        let (element, rest) = parse_element(&pattern[1..]);
        let run = element_run(&element, text);
        for take in (0..=run).rev() {
            if match_from(rest, &text[take..]) {
                return true;
            }
        }
        return false;
    }
    let (element, rest) = parse_element(pattern);
    match text.first() {
        Some(&c) if element_matches(&element, c) => match_from(rest, &text[1..]),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// The shell
// ---------------------------------------------------------------------------

/// One command the shell answers to.
struct Command {
    /// The command's name, as AmigaDOS spells it.
    name: &'static str,
    /// The command's ReadArgs template, published to the user by `?`.
    template: &'static str,
    /// What the command does.
    handler: fn(&mut Shell, &Args) -> Outcome,
}

/// Every command the shell answers to, with its template.
///
/// One table, because the table *is* the shell's contract with its user: the
/// names are what `Help` lists, the templates are what `Command ?` prints,
/// and a command missing from either is a command the shell does not have.
const COMMANDS: [Command; 23] = [
    Command {
        name: "assign",
        template: "NAME,TARGET",
        handler: cmd_assign,
    },
    Command {
        name: "cd",
        template: "DIR",
        handler: cmd_cd,
    },
    Command {
        name: "clear",
        template: "",
        handler: cmd_clear,
    },
    Command {
        name: "copy",
        template: "FROM/A,TO/A,ALL/S",
        handler: cmd_copy,
    },
    Command {
        name: "delete",
        template: "FILE/M",
        handler: cmd_delete,
    },
    Command {
        name: "dir",
        template: "DIR",
        handler: cmd_dir,
    },
    Command {
        name: "echo",
        template: "TEXT/F",
        handler: cmd_echo,
    },
    Command {
        name: "execute",
        template: "SCRIPT/A,ARGS/F",
        handler: cmd_execute,
    },
    Command {
        name: "exit",
        template: "",
        handler: cmd_exit,
    },
    Command {
        name: "getenv",
        template: "NAME/A",
        handler: cmd_getenv,
    },
    Command {
        name: "help",
        template: "",
        handler: cmd_help,
    },
    Command {
        name: "info",
        template: "",
        handler: cmd_info,
    },
    Command {
        name: "list",
        template: "DIR",
        handler: cmd_list,
    },
    Command {
        name: "makedir",
        template: "DIR/A",
        handler: cmd_makedir,
    },
    Command {
        name: "path",
        template: "PATHS/M",
        handler: cmd_path,
    },
    Command {
        name: "quit",
        template: "",
        handler: cmd_exit,
    },
    Command {
        name: "rename",
        template: "FROM/A,TO/A",
        handler: cmd_rename,
    },
    Command {
        name: "run",
        template: "COMMAND/A",
        handler: cmd_run,
    },
    Command {
        name: "set",
        template: "NAME,VALUE/F",
        handler: cmd_set,
    },
    Command {
        name: "setenv",
        template: "NAME,VALUE/F",
        handler: cmd_setenv,
    },
    Command {
        name: "type",
        template: "FILE/A",
        handler: cmd_type,
    },
    Command {
        name: "ver",
        template: "",
        handler: cmd_version,
    },
    Command {
        name: "version",
        template: "",
        handler: cmd_version,
    },
];

/// The shell: one input buffer, one prompt, one loop, and the state a
/// session accumulates — `Set` variables, the `Path` search list, and how
/// deeply scripts have nested.
pub struct Shell {
    /// The accumulated input line, not yet submitted.
    line: String,
    /// The machine's hostname, read once at startup.
    hostname: String,
    /// The session's `Set` variables, in the order they were set.
    vars: Vec<(String, String)>,
    /// The command search path, what `Path` shows and sets.
    path: Vec<String>,
    /// How many `Execute` scripts are running.
    depth: u32,
}

impl Shell {
    /// A fresh shell with an empty input line.
    pub fn new() -> Self {
        // Read the hostname from the kernel for the prompt.
        let mut buf = [0u8; 64];
        let hostname = match api::hostname(&mut buf) {
            Ok(n) => core::str::from_utf8(&buf[..n])
                .unwrap_or(USER_NAME)
                .to_string(),
            Err(_) => USER_NAME.to_string(),
        };
        Self {
            line: String::new(),
            hostname,
            vars: Vec::new(),
            path: vec!["C:".to_string()],
            depth: 0,
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
    /// what `CD` alone answers, so the prompt stays out of the business of
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

    /// Run one iteration of the read-eval-print loop.
    pub fn run_once(&mut self) -> Outcome {
        self.prompt();
        let Some(line) = self.read_line() else {
            return Outcome::OK;
        };
        self.run_line(&line)
    }

    /// Run one line of input, interactively or from a script.
    ///
    /// A line that is empty or all comment answers success and does
    /// nothing — the way an AmigaDOS script treats a blank line — so a
    /// script full of comments is not a script full of errors.
    pub fn run_line(&mut self, line: &str) -> Outcome {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') {
            return Outcome::OK;
        }
        let tokens = tokenize(trimmed);
        let Some(name) = tokens.first() else {
            return Outcome::OK;
        };
        let name = name.to_lowercase();

        // `Command ?` prints the command's template — the documented way to
        // ask a command how it wants to be called. `?` alone is Help's
        // business, and a `?` anywhere else is an ordinary argument.
        if tokens.get(1).map(|t| t.as_str()) == Some("?") {
            return print_template(&name);
        }

        let Some(command) = COMMANDS.iter().find(|c| c.name == name) else {
            api::print("Unknown command: ");
            api::print(&name);
            api::print("\nType Help for a list of commands.\n");
            return Outcome::failed(FAILURE);
        };

        let args = match read_args(&tokens[1..], command.template) {
            Ok(args) => args,
            Err(error) => {
                print_template(&name);
                api::print(": ");
                match error {
                    ParseError::Missing(arg) => {
                        api::print("required argument missing: ");
                        api::print(&arg);
                        api::print("\n");
                    }
                    ParseError::NotNumeric(arg) => {
                        api::print("argument must be a number: ");
                        api::print(&arg);
                        api::print("\n");
                    }
                    ParseError::TooMany => {
                        api::print("too many arguments\n");
                    }
                }
                return Outcome::failed(FAILURE);
            }
        };

        (command.handler)(self, &args)
    }

    /// Run the shell forever, or until a command asks to halt.
    pub fn run(mut self) -> ! {
        Self::print_banner();
        loop {
            let outcome = self.run_once();
            if outcome.result == CommandResult::Halt {
                break;
            }
        }
        api::print("vela: session ended\n");
        api::power_off()
    }

    // ----- helpers the commands share -----

    /// The current directory, as a path string.
    ///
    /// A separate function because several commands read it and none of them
    /// can deal with a failure by inventing a directory: a read the kernel
    /// refuses is reported, not papered over with `""`.
    fn current_dir_path(&self) -> Option<String> {
        let mut buf = [0u8; 256];
        let len = api::current_dir(&mut buf).ok()?;
        core::str::from_utf8(&buf[..len.min(buf.len() - 1)])
            .ok()
            .map(|s| s.to_string())
    }

    /// Report a failed command in the shell's own words.
    fn report(command: &str, message: &str, object: &str) {
        api::print(command);
        api::print(": ");
        api::print(message);
        if !object.is_empty() {
            api::print(" ");
            api::print(object);
        }
        api::print("\n");
    }

    /// Expand `$NAME` and `${NAME}` from the session's variables, then from
    /// the environment.
    ///
    /// A name that matches nothing is left exactly as it was written, the
    /// way AmigaDOS leaves it: a script that names a variable it never set
    /// should see its own spelling rather than silence.
    fn expand(&self, text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(dollar) = rest.find('$') {
            let after = &rest[dollar + 1..];
            let (name, skip): (Option<String>, usize) =
                if let Some(braced) = after.strip_prefix('{') {
                    match braced.find('}') {
                        Some(end) => (Some(braced[..end].to_string()), end + 2),
                        None => (None, 1),
                    }
                } else {
                    let len = after
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .count();
                    if len == 0 {
                        (None, 1)
                    } else {
                        (Some(after[..len].to_string()), len + 1)
                    }
                };
            match name.and_then(|n| self.lookup(&n).map(|v| (n, v))) {
                Some((_, value)) => {
                    out.push_str(&rest[..dollar]);
                    out.push_str(&value);
                }
                None => out.push_str(&rest[..dollar + skip]),
            }
            rest = &rest[dollar + skip..];
        }
        out.push_str(rest);
        out
    }

    /// The value of a variable: a `Set` variable first, then an `ENV:` file.
    fn lookup(&self, name: &str) -> Option<String> {
        for (key, value) in &self.vars {
            if key.eq_ignore_ascii_case(name) {
                return Some(value.clone());
            }
        }
        read_env(name)
    }

    /// Read a directory's entries, with the kind and size of each.
    fn read_entries(path: &str) -> Result<Vec<api::DirEntry>, ()> {
        let mut buf = vec![
            api::DirEntry {
                de_size: 0,
                de_type: 0,
                de_name_len: 0,
                de_name: [0; api::DIR_NAME_LEN],
            };
            DIR_BUFFER_ENTRIES
        ];
        match api::read_dir(path, &mut buf) {
            Ok(count) => Ok(buf.iter().take(count).cloned().collect()),
            Err(_) => Err(()),
        }
    }

    /// Split a possibly-patterned path into its directory and its pattern.
    ///
    /// `Dir SYS:#?.info` names a directory and a pattern in one argument:
    /// the pattern is the part after the last `:` or `/` that contains a
    /// wildcard, and the directory is everything before it. A plain path has
    /// an empty pattern and lists everything. A bare `?` never reaches here
    /// — that spelling is how a command is asked for its template.
    fn split_pattern(arg: &str) -> (String, String) {
        let wild = arg.contains("#?") || (arg.contains('?') && arg.len() > 1);
        if !wild {
            return (arg.to_string(), String::new());
        }
        match arg.rfind([':', '/']) {
            Some(at) => (arg[..at + 1].to_string(), arg[at + 1..].to_string()),
            None => (String::new(), arg.to_string()),
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

/// Print a command's template: `Copy FROM/A,TO/A,ALL/S`.
///
/// The modifiers are documentation, not input — the same line the command
/// itself prints when it cannot make sense of its arguments, so `Copy ?` and
/// a bad `Copy` answer with the same text.
fn print_template(name: &str) -> Outcome {
    api::print(name);
    if let Some(command) = COMMANDS.iter().find(|c| c.name == name) {
        api::print(" ");
        api::print(command.template);
    }
    api::print("\n");
    Outcome::OK
}

/// Read a whole file as text.
fn read_text(path: &str) -> Option<String> {
    let file = api::open(path, api::mode::READ).ok()?;
    let mut text = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        match api::read(file, &mut buf) {
            Ok(0) => break,
            Ok(n) => text.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let _ = api::close(file);
    String::from_utf8(text).ok()
}

/// Read one environment variable out of `ENV:`.
///
/// The file *is* the variable, the way AmigaDOS stores them: `SetEnv
/// WorkDir SYS:Utilities` creates `ENV:WorkDir` containing the value, and
/// `GetEnv WorkDir` reads the file back.
fn read_env(name: &str) -> Option<String> {
    let file = api::open(&alloc::format!("ENV:{}", name), api::mode::READ).ok()?;
    let mut text = Vec::new();
    let mut buf = [0u8; 128];
    loop {
        match api::read(file, &mut buf) {
            Ok(0) => break,
            Ok(n) => text.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let _ = api::close(file);
    let value = String::from_utf8(text).ok()?;
    // One trailing newline is the file's shape, not the value's.
    Some(value.trim_end_matches('\n').to_string())
}

/// Write one environment variable into `ENV:`.
fn write_env(name: &str, value: &str) -> Result<(), ()> {
    let file = api::open(&alloc::format!("ENV:{}", name), api::mode::WRITE).map_err(|_| ())?;
    let written = api::write(file, value.as_bytes())
        .map(|_| ())
        .map_err(|_| ());
    let _ = api::close(file);
    written
}

/// Delete one environment variable's file.
fn delete_env(name: &str) -> Result<(), ()> {
    api::delete_file(&alloc::format!("ENV:{}", name)).map_err(|_| ())
}

/// The current directory, or a report and nothing.
///
/// The commands that need the current directory do not always have a shell
/// to hand — `Delete` may be running inside a script — so the read stands on
/// its own rather than threading the shell through every signature that does
/// not otherwise want it.
fn current_dir_path() -> Option<String> {
    let mut buf = [0u8; 256];
    let len = api::current_dir(&mut buf).ok()?;
    core::str::from_utf8(&buf[..len.min(buf.len() - 1)])
        .ok()
        .map(|s| s.to_string())
}

/// A string padded on the right to a minimum width.
///
/// For the column of names a `List` aligns. Bytes, not chars, because the
/// console is a byte grid and a name's *displayed* width is its byte count.
fn padded(s: &str, width: usize) -> String {
    let mut out = String::from(s);
    while out.len() < width {
        out.push(' ');
    }
    out
}

// ---------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------

/// `?` on its own: not a pattern, not an argument — the command list itself.
fn cmd_help(_: &mut Shell, _: &Args) -> Outcome {
    api::print("VelaOS shell, AmigaDOS style\n");
    api::print("Arguments are keywords (Copy FROM=RAM:a TO=RAM:b) or positionals;\n");
    api::print("Command ? prints a command's template; paths with spaces are quoted.\n\n");
    api::print("  Assign [NAME: TARGET] - list, create or remove assigns\n");
    api::print("  CD [DIR]              - change directory; CD alone prints it; / goes up\n");
    api::print("  Clear                 - clear the screen\n");
    api::print("  Copy FROM= TO= [ALL]  - copy a file, or a directory tree with ALL\n");
    api::print("  Delete FILE...        - delete files; patterns like #? are expanded\n");
    api::print("  Dir [DIR]             - list names; patterns like #?.info are allowed\n");
    api::print("  Echo TEXT             - print text; $VAR is expanded\n");
    api::print("  Execute SCRIPT [ARGS] - run a script (.KEY, IF EXISTS/ELSE/ENDIF)\n");
    api::print("  Exit                  - end the session\n");
    api::print("  GetEnv NAME           - print an environment variable from ENV:\n");
    api::print("  Help                  - this list\n");
    api::print("  Info                  - list the volumes and devices\n");
    api::print("  List [DIR]            - list a directory with sizes\n");
    api::print("  MakeDir DIR           - create a directory\n");
    api::print("  Path [DIR...]         - show or set the command search path\n");
    api::print("  Rename FROM= TO=      - rename a file or directory\n");
    api::print("  Run COMMAND           - run a command in the background\n");
    api::print("  Set [NAME [VALUE]]    - set, show or clear a session variable\n");
    api::print("  SetEnv NAME [VALUE]   - set, show or clear an environment variable\n");
    api::print("  Type FILE             - print a file's contents\n");
    api::print("  Version               - print the system version\n");
    Outcome::OK
}

/// `Clear` — the `ClearScreen` syscall.
///
/// The framebuffer console erases its grid and homes its cursor, and the
/// serial log gets the ANSI sequence. Printing newlines here instead would
/// scroll rather than clear, which is the bug this call exists to avoid.
fn cmd_clear(_: &mut Shell, _: &Args) -> Outcome {
    let _ = api::clear_screen();
    Outcome::OK
}

/// `Exit` / `Quit` — end the session.
fn cmd_exit(_: &mut Shell, _: &Args) -> Outcome {
    Outcome {
        result: CommandResult::Halt,
        code: OK,
    }
}

/// `Version` — the system version.
///
/// The exact spelling is a boot test's marker: the test types `Version`'s
/// short form and waits for this line, so it must come from *here* and from
/// nowhere else — the banner deliberately does not contain it.
fn cmd_version(_: &mut Shell, _: &Args) -> Outcome {
    api::print("VelaOS 0.1.0\n");
    Outcome::OK
}

/// `CD` — change directory, or with no argument, say where we are.
///
/// AmigaDOS goes *up* with `/`, not `..`: one `/` is one level up, `//` is
/// two, and what follows them is joined back on. `CD /` at a volume's root
/// stays at the root, because there is nothing above a root to go to.
fn cmd_cd(shell: &mut Shell, args: &Args) -> Outcome {
    let Some(dir) = args.get("DIR") else {
        return match shell.current_dir_path() {
            Some(dir) => {
                api::print(&dir);
                api::print("\n");
                Outcome::OK
            }
            None => {
                Shell::report("CD", "cannot read the current directory", "");
                Outcome::failed(ERROR)
            }
        };
    };

    let target = match cd_target(shell, dir) {
        Some(target) => target,
        None => {
            Shell::report("CD", "cannot go above the root of", dir);
            return Outcome::failed(ERROR);
        }
    };
    match api::set_current_dir(&target) {
        Ok(()) => Outcome::OK,
        Err(_) => {
            Shell::report("CD", "cannot change to", &target);
            Outcome::failed(ERROR)
        }
    }
}

/// Where `CD`'s argument lands, honouring the `/`-hops.
///
/// `None` means "the hops went above the root", which is an error rather
/// than a no-op: a `CD //` that silently stayed put would leave a script
/// running in a directory it believed it had left.
fn cd_target(shell: &Shell, dir: &str) -> Option<String> {
    if !dir.starts_with('/') {
        return Some(dir.to_string());
    }
    let hops = dir.chars().take_while(|&c| c == '/').count();
    let rest = &dir[hops..];
    let mut path = shell.current_dir_path()?;
    for _ in 0..hops {
        path = parent_amiga(&path)?;
    }
    if rest.is_empty() {
        return Some(path);
    }
    // Join the remainder on: a volume root has nothing to join onto with a
    // slash, so `CD /Work` from `RAM:` lands at `RAM:Work`.
    if path.ends_with(':') {
        Some(alloc::format!("{}{}", path, rest))
    } else {
        Some(alloc::format!("{}/{}", path, rest))
    }
}

/// The parent of an AmigaDOS path, one level up.
///
/// `RAM:/work/deeper` → `RAM:/work`, `RAM:/work` → `RAM:`, and `RAM:` has no
/// parent at all, because there is nothing above a volume root.
fn parent_amiga(path: &str) -> Option<String> {
    let volume_end = path.find(':').map(|i| i + 1)?;
    match path.rfind('/') {
        Some(at) if at >= volume_end => Some(path[..at].to_string()),
        Some(_) => Some(path[..volume_end].to_string()),
        None => None,
    }
}

/// `Dir` — the names of a directory's entries, one per line.
///
/// The quick look: names only, with a directory's entries marked so a user
/// can tell what `CD` would accept. The detailed listing is `List`. An
/// argument may carry a pattern — `Dir SYS:#?.info` — and only the entries
/// that match it are shown.
fn cmd_dir(shell: &mut Shell, args: &Args) -> Outcome {
    list_names(shell, args, "Dir", false)
}

/// `List` — a directory's entries with their sizes.
///
/// One line per entry — name, kind, size — followed by a count, so the
/// summary a `List` ends with survives too. Directories report their kind
/// instead of a size, because a directory's size is an internal detail of
/// the filesystem rather than something a user asked for.
fn cmd_list(shell: &mut Shell, args: &Args) -> Outcome {
    list_names(shell, args, "List", true)
}

/// The listing both `Dir` and `List` are built from.
///
/// The two commands differ in exactly two ways — whether sizes are shown,
/// and what their failures call themselves — so one body carries both, and
/// a fix to the listing is a fix to both.
fn list_names(shell: &mut Shell, args: &Args, name: &str, detailed: bool) -> Outcome {
    let (dir, pattern) = match args.get("DIR") {
        Some(arg) => Shell::split_pattern(arg),
        None => match shell.current_dir_path() {
            Some(dir) => (dir, String::new()),
            None => {
                Shell::report(name, "cannot read the current directory", "");
                return Outcome::failed(ERROR);
            }
        },
    };
    // The kernel's resolver treats a bare name as relative to its current
    // directory, so a listing with no directory reads the path back first:
    // passing an empty string would fail long before any filesystem, because
    // an empty path parses to nothing at all.
    let dir = if dir.is_empty() {
        match current_dir_path() {
            Some(dir) => dir,
            None => {
                Shell::report(name, "cannot read the current directory", "");
                return Outcome::failed(ERROR);
            }
        }
    } else {
        dir
    };
    match Shell::read_entries(&dir) {
        Ok(entries) => {
            let mut files = 0u32;
            let mut dirs = 0u32;
            for entry in &entries {
                if !pattern.is_empty() && !dos_match(&pattern, entry.name()) {
                    continue;
                }
                if detailed {
                    api::print(&padded(entry.name(), 24));
                    if entry.is_dir() {
                        api::print("<dir>");
                        dirs += 1;
                    } else {
                        api::print_int(entry.de_size);
                        files += 1;
                    }
                } else {
                    api::print(entry.name());
                    if entry.is_dir() {
                        api::print(" (dir)");
                        dirs += 1;
                    } else {
                        files += 1;
                    }
                }
                api::print("\n");
            }
            if detailed {
                api::print_int(files as u64);
                api::print(" files, ");
                api::print_int(dirs as u64);
                api::print(" directories\n");
            }
            Outcome::OK
        }
        Err(()) => {
            Shell::report(name, "cannot read directory", &dir);
            Outcome::failed(ERROR)
        }
    }
}

/// `MakeDir` — create a directory.
fn cmd_makedir(_: &mut Shell, args: &Args) -> Outcome {
    let dir = args.get("DIR").unwrap_or("");
    match api::create_dir(dir) {
        Ok(()) => Outcome::OK,
        // "Already there" is the outcome `MakeDir` wanted, so it is reported
        // as success the way init's boot-time creation is.
        Err(e) if e.code == api::errors::OBJECT_IN_USE => Outcome::OK,
        Err(_) => {
            Shell::report("MakeDir", "cannot create", dir);
            Outcome::failed(ERROR)
        }
    }
}

/// `Copy` — duplicate a file, or a whole directory tree with `ALL`.
///
/// Without `ALL`, a directory source is refused rather than guessed at:
/// AmigaDOS makes the difference explicit, because a copy that silently
/// deep-copied when the caller meant one file is a copy that will not be
/// noticed until the disk is full.
fn cmd_copy(_: &mut Shell, args: &Args) -> Outcome {
    let from = args.get("FROM").unwrap_or("").to_string();
    let to = args.get("TO").unwrap_or("").to_string();
    let all = args.switch("ALL");

    let info = match api::examine(&from) {
        Ok(info) => info,
        Err(_) => {
            Shell::report("Copy", "cannot open", &from);
            return Outcome::failed(ERROR);
        }
    };
    // Entry type 3 is a directory, in the AmigaOS numbering the kernel uses.
    if info.fi_entry_type == api::entry_type::DIR {
        if !all {
            Shell::report("Copy", "is a directory; copy it with ALL", &from);
            return Outcome::failed(FAILURE);
        }
        return match copy_tree(&from, &to) {
            Ok(()) => Outcome::OK,
            Err(()) => {
                Shell::report("Copy", "cannot copy directory", &from);
                Outcome::failed(ERROR)
            }
        };
    }

    // A file copied onto a directory goes *into* the directory, under its
    // own name — what a user who wrote `Copy RAM:a.txt RAM:dst` meant.
    let target = if api::examine(&to)
        .map(|info| info.fi_entry_type == api::entry_type::DIR)
        .unwrap_or(false)
    {
        let name = from
            .rfind([':', '/'])
            .map(|i| &from[i + 1..])
            .unwrap_or(&from);
        alloc::format!("{}/{}", to, name)
    } else {
        to.clone()
    };
    match copy_file(&from, &target) {
        Ok(()) => Outcome::OK,
        Err(message) => {
            Shell::report("Copy", message, &target);
            Outcome::failed(ERROR)
        }
    }
}

/// Copy one file's contents.
///
/// The error is a message rather than a code, because every failure this
/// function can have is reported the same way — named, and left to the
/// caller to band.
fn copy_file(from: &str, to: &str) -> Result<(), &'static str> {
    let source = match api::open(from, api::mode::READ) {
        Ok(file) => file,
        Err(_) => return Err("cannot open"),
    };
    let target = match api::open(to, api::mode::WRITE) {
        Ok(file) => file,
        Err(_) => {
            let _ = api::close(source);
            return Err("cannot create");
        }
    };
    let mut buf = [0u8; 256];
    let mut failed = None;
    loop {
        match api::read(source, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if api::write(target, &buf[..n]).is_err() {
                    failed = Some("cannot write");
                    break;
                }
            }
            Err(_) => {
                failed = Some("cannot read");
                break;
            }
        }
    }
    let _ = api::close(source);
    let _ = api::close(target);
    match failed {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

/// Copy a directory tree, the `ALL` half of `Copy`.
///
/// The target is created, then every entry is copied: files directly,
/// directories by recursion. There is no guard against the target being
/// *inside* the source — the ramfs has no links, so the copy simply runs
/// out of entries rather than looping forever.
fn copy_tree(from: &str, to: &str) -> Result<(), ()> {
    api::create_dir(to).map_err(|_| ())?;
    for entry in Shell::read_entries(from)? {
        let child_from = alloc::format!("{}/{}", from, entry.name());
        let child_to = alloc::format!("{}/{}", to, entry.name());
        if entry.is_dir() {
            copy_tree(&child_from, &child_to)?;
        } else {
            copy_file(&child_from, &child_to).map_err(|_| ())?;
        }
    }
    Ok(())
}

/// `Rename` — give a file or directory a new name.
fn cmd_rename(_: &mut Shell, args: &Args) -> Outcome {
    let from = args.get("FROM").unwrap_or("");
    let to = args.get("TO").unwrap_or("");
    match api::rename(from, to) {
        Ok(()) => Outcome::OK,
        Err(_) => {
            Shell::report(
                "Rename",
                "cannot rename",
                &alloc::format!("{} to {}", from, to),
            );
            Outcome::failed(ERROR)
        }
    }
}

/// `Delete` — remove files, one or many, by name or by pattern.
///
/// Each argument is expanded on its own: `Delete RAM:#?.tmp` deletes every
/// `.tmp` file in `RAM:`, and `Delete RAM:a.txt RAM:b.txt` deletes both. A
/// pattern that matches nothing is a warning rather than an error — nothing
/// was deleted, which is the caller's request twice removed.
fn cmd_delete(_: &mut Shell, args: &Args) -> Outcome {
    let mut code = OK;
    for file in &args.all("FILE") {
        let (dir, pattern) = Shell::split_pattern(file);
        if pattern.is_empty() {
            delete_one(file, &mut code);
            continue;
        }
        // A patterned argument names a directory and a pattern. The
        // directory part may be empty, which means the current directory —
        // the same rule `Dir` follows.
        let dir = if dir.is_empty() {
            match current_dir_path() {
                Some(dir) => dir,
                None => {
                    Shell::report("Delete", "cannot read the current directory", "");
                    return Outcome::failed(ERROR);
                }
            }
        } else {
            dir
        };
        match Shell::read_entries(&dir) {
            Ok(entries) => {
                let matches: Vec<String> = entries
                    .iter()
                    .filter(|e| dos_match(&pattern, e.name()))
                    .map(|e| alloc::format!("{}/{}", dir, e.name()))
                    .collect();
                if matches.is_empty() {
                    Shell::report("Delete", "no file matches", file);
                    code = code.max(WARN);
                }
                for path in matches {
                    delete_one(&path, &mut code);
                }
            }
            Err(()) => {
                Shell::report("Delete", "cannot read directory", &dir);
                code = code.max(ERROR);
            }
        }
    }
    Outcome::failed(code)
}

/// Delete one path, banding the outcome into the running code.
fn delete_one(path: &str, code: &mut i32) {
    if api::delete_file(path).is_err() {
        Shell::report("Delete", "cannot delete", path);
        *code = (*code).max(ERROR);
    }
}

/// `Type` — print a file's contents.
fn cmd_type(_: &mut Shell, args: &Args) -> Outcome {
    let Some(file) = args.get("FILE") else {
        Shell::report("Type", "missing file", "");
        return Outcome::failed(FAILURE);
    };
    match api::open(file, api::mode::READ) {
        Ok(handle) => {
            let mut buf = [0u8; 256];
            loop {
                match api::read(handle, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let s = core::str::from_utf8(&buf[..n]).unwrap_or("<binary>");
                        api::print(s);
                    }
                    Err(_) => break,
                }
            }
            let _ = api::close(handle);
            Outcome::OK
        }
        Err(_) => {
            Shell::report("Type", "cannot open", file);
            Outcome::failed(ERROR)
        }
    }
}

/// `Info` — the volumes and devices the system answers to.
///
/// The two real volumes are probed, because a volume the kernel mounted is a
/// fact the system can be wrong about; `NIL:` and `CON:` are always there,
/// because they are what the kernel's own I/O is made of. The names are the
/// AmigaDOS spellings, with the colons — they are volumes, not directories.
fn cmd_info(_: &mut Shell, _: &Args) -> Outcome {
    api::print("Name  Kind     State\n");
    for (name, kind) in [("SYS:", "Volume"), ("RAM:", "Volume")] {
        let state = match api::examine(name) {
            Ok(_) => "available",
            Err(_) => "not responding",
        };
        api::print(&padded(name, 6));
        api::print(&padded(kind, 9));
        api::print(state);
        api::print("\n");
    }
    api::print(&padded("NIL:", 6));
    api::print(&padded("Device", 9));
    api::print("writes are discarded\n");
    api::print(&padded("CON:", 6));
    api::print(&padded("Device", 9));
    api::print("this console\n");
    Outcome::OK
}

/// `Assign` — list the assigns, or create, re-point or remove one.
///
/// With no arguments, every assign is listed: name, targets in search
/// order, and a marker on the ones the system created. `Assign WORK:
/// SYS:Work` creates or re-points; `Assign WORK: DELETE` removes. The
/// colon is part of how the name is *written* — the assign itself is named
/// without it, the way the kernel stores it.
fn cmd_assign(_: &mut Shell, args: &Args) -> Outcome {
    let Some(name) = args.get("NAME") else {
        return list_assigns();
    };
    // The trailing colon is spelling, not name: `WORK:` and `WORK` are the
    // same assign, and the kernel knows it without the colon.
    let name = name.trim_end_matches(':');

    let Some(target) = args.get("TARGET") else {
        Shell::report("Assign", "an assign needs a target", name);
        return Outcome::failed(FAILURE);
    };
    if target.eq_ignore_ascii_case("DELETE") {
        return match api::remove_assign(name) {
            Ok(()) => Outcome::OK,
            Err(_) => {
                Shell::report("Assign", "cannot remove", name);
                Outcome::failed(ERROR)
            }
        };
    }
    match api::add_assign(name, target) {
        Ok(()) => Outcome::OK,
        Err(_) => {
            Shell::report(
                "Assign",
                "cannot assign",
                &alloc::format!("{} to {}", name, target),
            );
            Outcome::failed(ERROR)
        }
    }
}

/// Every assign, one per line: name, targets, and the system's marker.
///
/// The targets come back in backend form — `/System/Libs` — because that is
/// how the resolver stores them; the display turns the two volume roots
/// back into the `SYS:` and `RAM:` spellings the user wrote, since an
/// assign list full of kernel-internal paths is a list nobody can read.
fn list_assigns() -> Outcome {
    let mut buf = vec![
        api::AssignEntry {
            ae_name: [0; api::ASSIGN_NAME_LEN],
            ae_name_len: 0,
            ae_targets: [0; api::ASSIGN_TARGETS_LEN],
            ae_targets_len: 0,
            ae_system: 0,
        };
        ASSIGN_BUFFER_ENTRIES
    ];
    match api::list_assigns(&mut buf) {
        Ok(count) => {
            let entries = &buf[..count];
            // The two volume roots, as the assigns themselves name them, so
            // the display can spell targets the way the user does.
            let root_of = |name: &str| {
                entries
                    .iter()
                    .find(|e| e.name().eq_ignore_ascii_case(name))
                    .and_then(|e| {
                        e.targets()
                            .split(' ')
                            .find(|t| !t.is_empty())
                            .map(String::from)
                    })
                    .unwrap_or_default()
            };
            let sys_root = root_of("SYS");
            let ram_root = root_of("RAM");
            for entry in entries {
                api::print(&padded(&alloc::format!("{}:", entry.name()), 12));
                let targets: Vec<String> = entry
                    .targets()
                    .split(' ')
                    .filter(|t| !t.is_empty())
                    .map(|t| display_target(t, &sys_root, &ram_root))
                    .collect();
                api::print(&targets.join(" "));
                if entry.is_system() {
                    api::print(" (system)");
                }
                api::print("\n");
            }
            Outcome::OK
        }
        Err(_) => {
            Shell::report("Assign", "cannot list the assigns", "");
            Outcome::failed(ERROR)
        }
    }
}

/// Spell a backend target the way the user would.
///
/// `/System/Libs` under `SYS:`'s root is `SYS:Libs`, and `/ramfs/T` under
/// `RAM:`'s root is `RAM:T`. A target under neither root is a backend path
/// the shell has no spelling for, and it is shown as it stands rather than
/// disguised.
fn display_target(target: &str, sys_root: &str, ram_root: &str) -> String {
    for (root, volume) in [(sys_root, "SYS:"), (ram_root, "RAM:")] {
        if !root.is_empty() {
            if let Some(rest) = target.strip_prefix(root) {
                return alloc::format!("{}{}", volume, rest.trim_start_matches('/'));
            }
        }
    }
    target.to_string()
}

/// `Path` — show or set the command search path.
///
/// AmigaDOS searches the path for a command a bare name does not find in the
/// current directory. VelaOS's commands are built in, so the path is a fact
/// the shell reports and records rather than one it searches — which is
/// honest, but no reason for the command to lie about what it keeps.
fn cmd_path(shell: &mut Shell, args: &Args) -> Outcome {
    let paths = args.all("PATHS");
    if paths.is_empty() {
        for entry in &shell.path {
            api::print(entry);
            api::print("\n");
        }
        return Outcome::OK;
    }
    shell.path = paths;
    Outcome::OK
}

/// `Run` — start a command in the background.
///
/// Honest about not being able to: v0.1.0 has no loader a shell could hand a
/// second program to, and a `Run` that ran its command in the *foreground*
/// would be a lie the user discovers the first time they type one.
fn cmd_run(_: &mut Shell, args: &Args) -> Outcome {
    let command = args.get("COMMAND").unwrap_or("");
    Shell::report("Run", "cannot start a program from the shell yet:", command);
    Outcome::failed(FAILURE)
}

/// `Echo` — print text, with variables expanded.
///
/// `$NAME` and `${NAME}` are replaced from the session's variables and the
/// environment; a name nothing answers to is left as written, so an `Echo`
/// that names a variable nobody set still shows the user their own words.
fn cmd_echo(shell: &mut Shell, args: &Args) -> Outcome {
    let text = args.get("TEXT").unwrap_or("");
    api::print(&shell.expand(text));
    api::print("\n");
    Outcome::OK
}

/// `Set` — session variables.
///
/// With a value, set (or replace) it; with only a name, clear it; with
/// nothing at all, list what is set. Session variables live and die with
/// the shell — `SetEnv` is the one that outlives a command, in `ENV:`.
fn cmd_set(shell: &mut Shell, args: &Args) -> Outcome {
    let Some(name) = args.get("NAME") else {
        if shell.vars.is_empty() {
            api::print("No variables are set.\n");
            return Outcome::OK;
        }
        for (key, value) in &shell.vars {
            api::print(&alloc::format!("{}={}", key, value));
            api::print("\n");
        }
        return Outcome::OK;
    };
    match args.get("VALUE") {
        Some(value) => {
            let key = name.to_string();
            match shell
                .vars
                .iter_mut()
                .find(|(k, _)| k.eq_ignore_ascii_case(&key))
            {
                Some(slot) => slot.1 = value.to_string(),
                None => shell.vars.push((key, value.to_string())),
            }
            Outcome::OK
        }
        None => {
            shell.vars.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
            Outcome::OK
        }
    }
}

/// `SetEnv` — environment variables, stored in `ENV:` where they belong.
///
/// An environment variable *is* a file under `ENV:`, the way AmigaDOS keeps
/// them — which is why `GetEnv` can read one and so can `Type`. With a
/// value, the file is written; with only a name, it is deleted; with
/// nothing at all, `ENV:` itself is listed.
fn cmd_setenv(_: &mut Shell, args: &Args) -> Outcome {
    let Some(name) = args.get("NAME") else {
        return match Shell::read_entries("ENV:") {
            Ok(entries) => {
                for entry in &entries {
                    api::print(entry.name());
                    api::print("\n");
                }
                Outcome::OK
            }
            Err(()) => {
                Shell::report("SetEnv", "cannot list", "ENV:");
                Outcome::failed(ERROR)
            }
        };
    };
    match args.get("VALUE") {
        Some(value) => match write_env(name, value) {
            Ok(()) => Outcome::OK,
            Err(()) => {
                Shell::report("SetEnv", "cannot write", &alloc::format!("ENV:{}", name));
                Outcome::failed(ERROR)
            }
        },
        None => match delete_env(name) {
            Ok(()) => Outcome::OK,
            Err(_) => {
                Shell::report("SetEnv", "cannot delete", &alloc::format!("ENV:{}", name));
                Outcome::failed(ERROR)
            }
        },
    }
}

/// `GetEnv` — print an environment variable.
fn cmd_getenv(shell: &mut Shell, args: &Args) -> Outcome {
    let Some(name) = args.get("NAME") else {
        return Outcome::OK;
    };
    match shell.lookup(name) {
        Some(value) => {
            api::print(&value);
            api::print("\n");
            Outcome::OK
        }
        None => {
            Shell::report("GetEnv", "is not set", name);
            Outcome::failed(ERROR)
        }
    }
}

/// `Execute` — run a script.
///
/// A script is a text file of shell lines, `;` for comments, and the
/// argument conventions AmigaDOS scripts use: a `.KEY` line publishes the
/// script's own template, `.BRA`/`.KET` change the substitution brackets,
/// and `{NAME}` is replaced by the argument's value. `IF EXISTS` /
/// `ELSE` / `ENDIF` guard whole stretches of the script.
fn cmd_execute(shell: &mut Shell, args: &Args) -> Outcome {
    let Some(script) = args.get("SCRIPT") else {
        return Outcome::OK;
    };
    if shell.depth >= MAX_SCRIPT_DEPTH {
        Shell::report("Execute", "scripts nested too deeply", script);
        return Outcome::failed(FAILURE);
    }
    let text = match read_text(script) {
        Some(text) => text,
        None => {
            Shell::report("Execute", "cannot open", script);
            return Outcome::failed(ERROR);
        }
    };
    // The `.KEY` template, and the bracket characters, come from the script
    // itself: they are read before anything runs, because the arguments are
    // matched against the template and the substitution needs the brackets.
    let mut template = String::new();
    let mut bra = '{';
    let mut ket = '}';
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = directive(trimmed, ".key") {
            template = rest.to_string();
        } else if let Some(rest) = directive(trimmed, ".bra") {
            bra = rest.chars().next().unwrap_or('{');
        } else if let Some(rest) = directive(trimmed, ".ket") {
            ket = rest.chars().next().unwrap_or('}');
        }
    }
    // The script's own arguments, matched against its `.KEY` template. The
    // arguments arrive as one rest-of-line value, which is *not* re-split:
    // the quotes the user typed to keep a path together are gone already,
    // and re-splitting would tear the value apart at its spaces.
    let script_args = match read_args(&args.all("ARGS"), &template) {
        Ok(args) => args,
        Err(_) => {
            Shell::report("Execute", "wrong arguments for", script);
            api::print(&alloc::format!("{} {}", script, template));
            api::print("\n");
            return Outcome::failed(FAILURE);
        }
    };

    shell.depth += 1;
    let code = run_script(shell, &text, &script_args, bra, ket);
    shell.depth -= 1;
    Outcome::failed(code)
}

/// The rest of a line after one of a script's dot-directives, if the line is
/// that directive.
///
/// Directives are matched case-insensitively, the way AmigaDOS matches
/// everything else, and a directive with no argument (`.KEY` alone) is not
/// one: there is nothing to configure.
fn directive<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let first = line.split_whitespace().next()?;
    if first.eq_ignore_ascii_case(name) {
        let rest = line[first.len()..].trim();
        if rest.is_empty() {
            None
        } else {
            Some(rest)
        }
    } else {
        None
    }
}

/// One `IF` frame of a running script.
struct Frame {
    /// Whether the `IF`'s condition held.
    condition: bool,
    /// Whether the `ELSE` half is the one running now.
    in_else: bool,
    /// Whether the frame's enclosing `IF`s allow anything to run at all.
    outer_active: bool,
}

impl Frame {
    /// Whether a line inside this frame runs.
    fn active(&self) -> bool {
        self.outer_active && (self.condition ^ self.in_else)
    }
}

/// Run the lines of one script.
///
/// Each line is substituted, then — if the `IF`s allow it — run through the
/// same dispatcher an interactive line goes through. The `IF` stack is what
/// keeps the nesting honest: a frame records its condition and whether its
/// `ELSE` half is the one running, and a line is executed only when every
/// frame on the stack says yes.
fn run_script(shell: &mut Shell, text: &str, script_args: &Args, bra: char, ket: char) -> i32 {
    let mut frames: Vec<Frame> = Vec::new();
    let mut code = OK;
    for line in text.lines() {
        let trimmed = line.trim();
        // Comments and blanks never run — and directives are consumed when
        // the script is read, not when it runs.
        if trimmed.is_empty() || trimmed.starts_with(';') {
            continue;
        }
        if directive(trimmed, ".key").is_some()
            || directive(trimmed, ".bra").is_some()
            || directive(trimmed, ".ket").is_some()
        {
            continue;
        }

        let mut words = trimmed.split_whitespace();
        match words.next().map(str::to_lowercase).as_deref() {
            Some("if") => {
                frames.push(Frame {
                    condition: eval_if(trimmed, script_args, bra, ket),
                    in_else: false,
                    outer_active: frames.iter().all(Frame::active),
                });
                continue;
            }
            Some("else") => {
                if let Some(frame) = frames.last_mut() {
                    frame.in_else = true;
                }
                continue;
            }
            Some("endif") => {
                frames.pop();
                continue;
            }
            _ => {}
        }

        if !frames.iter().all(Frame::active) {
            continue;
        }
        let substituted = substitute(trimmed, script_args, bra, ket, shell);
        let outcome = shell.run_line(&substituted);
        code = outcome.code;
        if outcome.result == CommandResult::Halt {
            break;
        }
    }
    code
}

/// Evaluate one script `IF`.
///
/// Two forms are understood, the ones the report's script examples use:
/// `IF EXISTS <path>`, which is true when the path can be examined, and
/// `IF <a> EQ <b>`, which compares two words. Anything else is false — an
/// `IF` the shell cannot understand is an `IF` whose body should not run.
fn eval_if(line: &str, script_args: &Args, bra: char, ket: char) -> bool {
    // The condition is substituted with a fresh shell, so an `IF` can read
    // the session's variables but never change them.
    let substituted = substitute(line, script_args, bra, ket, &Shell::new());
    let mut tokens = tokenize(&substituted);
    if tokens.first().map(|t| t.eq_ignore_ascii_case("if")) == Some(true) {
        tokens.remove(0);
    }
    match tokens.as_slice() {
        [first, rest @ ..] if first.eq_ignore_ascii_case("EXISTS") => {
            let path = rest.join(" ");
            api::examine(path.trim_matches('"')).is_ok()
        }
        [a, op, b @ ..] if op.eq_ignore_ascii_case("EQ") => {
            let b = b.join(" ");
            a.trim_matches('"') == b.trim_matches('"')
        }
        _ => false,
    }
}

/// Replace a script's `{NAME}` brackets, then the shell's `$VAR` references.
///
/// The brackets are whatever the script's `.BRA`/`.KET` lines chose, and a
/// name nothing answers to is left as written.
fn substitute(line: &str, script_args: &Args, bra: char, ket: char, shell: &Shell) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(open) = rest.find(bra) {
        let after = &rest[open + bra.len_utf8()..];
        match after.find(ket) {
            Some(close) => {
                let name = &after[..close];
                out.push_str(&rest[..open]);
                match script_args.values_of(name) {
                    Some(values) => out.push_str(&values.join(" ")),
                    // An unknown name is left as written, brackets and all.
                    None => {
                        out.push_str(&rest[open..open + bra.len_utf8() + close + ket.len_utf8()])
                    }
                }
                rest = &after[close + ket.len_utf8()..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    shell.expand(&out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_on_spaces_and_keeps_quoted_spaces() {
        assert_eq!(
            tokenize("Copy FROM=RAM:a TO=RAM:b"),
            vec!["Copy", "FROM=RAM:a", "TO=RAM:b"]
        );
        assert_eq!(
            tokenize(r#"Type "RAM:name with spaces.txt""#),
            vec!["Type", "RAM:name with spaces.txt"]
        );
        assert_eq!(tokenize("  "), Vec::<String>::new());
    }

    #[test]
    fn read_args_binds_keywords_aliases_switches_and_positionals() {
        // Keywords, in both the `=` and the two-token form, with an alias.
        let tokens = tokenize("FROM=RAM:a TO=RAM:b ALL BUF=20");
        let args = read_args(&tokens, "FROM/A,TO/A,ALL/S,BUF=BUFFER/K/N").unwrap();
        assert_eq!(args.get("FROM"), Some("RAM:a"));
        assert_eq!(args.get("TO"), Some("RAM:b"));
        assert_eq!(args.get("BUFFER"), Some("20"));
        assert_eq!(args.number("BUFFER"), Some(20));
        assert!(args.switch("ALL"));

        // Positional form, filling the template left to right.
        let tokens = tokenize("RAM:a RAM:b");
        let args = read_args(&tokens, "FROM/A,TO/A,ALL/S").unwrap();
        assert_eq!(args.get("FROM"), Some("RAM:a"));
        assert_eq!(args.get("TO"), Some("RAM:b"));
        assert!(!args.switch("ALL"));

        // A bare switch name turns the switch on.
        let tokens = tokenize("RAM:a RAM:b ALL");
        let args = read_args(&tokens, "FROM/A,TO/A,ALL/S").unwrap();
        assert!(args.switch("ALL"));

        // `/M` collects every positional that follows it.
        let tokens = tokenize("RAM:a.txt RAM:b.txt RAM:c.txt");
        let args = read_args(&tokens, "FILE/M").unwrap();
        assert_eq!(
            args.all("FILE"),
            vec!["RAM:a.txt", "RAM:b.txt", "RAM:c.txt"]
        );

        // `/F` takes the rest of the line, spaces and all.
        let tokens = tokenize("hello world again");
        let args = read_args(&tokens, "TEXT/F").unwrap();
        assert_eq!(args.get("TEXT"), Some("hello world again"));
    }

    #[test]
    fn read_args_refuses_incomplete_and_nonsense() {
        // A required argument missing is a failure, named.
        let tokens = tokenize("FROM=RAM:a");
        assert_eq!(
            read_args(&tokens, "FROM/A,TO/A,ALL/S"),
            Err(ParseError::Missing("TO".to_string()))
        );
        // A `/N` argument with a non-number is a failure, named.
        let tokens = tokenize("BUFFER=lots");
        assert_eq!(
            read_args(&tokens, "BUFFER/K/N"),
            Err(ParseError::NotNumeric("BUFFER".to_string()))
        );
        // More positionals than the template has slots for is a failure.
        let tokens = tokenize("a b c");
        assert_eq!(read_args(&tokens, "FROM/A,TO/A"), Err(ParseError::TooMany));
    }

    #[test]
    fn dos_match_implements_the_amigados_wildcards() {
        // `?` is one character.
        assert!(dos_match("a?c", "abc"));
        assert!(!dos_match("a?c", "ac"));
        // `#?` is any number of characters — AmigaDOS's `*`.
        assert!(dos_match("#?", "anything at all"));
        assert!(dos_match("#?.info", "readme.info"));
        assert!(!dos_match("#?.info", "readme.txt"));
        // `#X` is any number of X — zero of them counts too.
        assert!(dos_match("ab#c", "abc"));
        assert!(dos_match("ab#c", "abccc"));
        assert!(dos_match("ab#c", "ab"));
        // Any number of *that letter only*: a stray other letter breaks it.
        assert!(!dos_match("ab#c", "abbbc"));
        assert!(!dos_match("ab#c", "abqc"));
        // `[a-c]` is a character range, and matching folds case.
        assert!(dos_match("[a-c]bc", "abc"));
        assert!(dos_match("[A-C]bc", "AbC"));
        assert!(!dos_match("[a-c]bc", "zbc"));
        // The pattern must cover the whole name.
        assert!(!dos_match("read", "readme.info"));
        assert!(!dos_match("#?.info", "info"));
    }

    #[test]
    fn a_patterned_argument_splits_into_directory_and_pattern() {
        assert_eq!(
            Shell::split_pattern("SYS:#?.info"),
            ("SYS:".to_string(), "#?.info".to_string())
        );
        // No separator means the current directory, the way `Dir #?.info`
        // reads.
        assert_eq!(
            Shell::split_pattern("#?.info"),
            (String::new(), "#?.info".to_string())
        );
        // A plain path carries no pattern at all.
        assert_eq!(
            Shell::split_pattern("SYS:S/Startup-Sequence"),
            ("SYS:S/Startup-Sequence".to_string(), String::new())
        );
    }

    #[test]
    fn parent_amiga_walks_up_one_level_at_a_time() {
        assert_eq!(
            parent_amiga("RAM:/work/deeper"),
            Some("RAM:/work".to_string())
        );
        assert_eq!(parent_amiga("RAM:/work"), Some("RAM:".to_string()));
        // A volume root has no parent: there is nothing above it.
        assert_eq!(parent_amiga("RAM:"), None);
        assert_eq!(parent_amiga("SYS:"), None);
    }

    #[test]
    fn display_target_spells_the_volumes_the_user_does() {
        assert_eq!(
            display_target("/System/Libs", "/System", "/ramfs"),
            "SYS:Libs"
        );
        assert_eq!(display_target("/ramfs/T", "/System", "/ramfs"), "RAM:T");
        // A target under neither root is shown as it stands.
        assert_eq!(
            display_target("/elsewhere/x", "/System", "/ramfs"),
            "/elsewhere/x"
        );
    }

    #[test]
    fn substitute_replaces_bracketed_names_and_leaves_unknown_ones() {
        let script_args = read_args(&tokenize("RAM:in RAM:out"), "SOURCE/A,DEST/A").unwrap();
        let shell = Shell {
            line: String::new(),
            hostname: "vela".to_string(),
            vars: Vec::new(),
            path: Vec::new(),
            depth: 0,
        };
        assert_eq!(
            substitute(
                "COPY FROM=\"{SOURCE}\" TO=\"{DEST}\"",
                &script_args,
                '{',
                '}',
                &shell
            ),
            "COPY FROM=\"RAM:in\" TO=\"RAM:out\""
        );
        // A name nothing answers to is left as written.
        assert_eq!(
            substitute("Echo {NOWHERE}", &script_args, '{', '}', &shell),
            "Echo {NOWHERE}"
        );
    }
}
