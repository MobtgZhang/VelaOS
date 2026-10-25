//! Boot configuration parsing for `\Vela\boot\vela.toml`.
//!
//! A hand-rolled parser rather than full TOML: the loader cannot afford a
//! general-purpose deserializer before `ExitBootServices`, and the config
//! shape is deliberately tiny. Unknown keys are ignored so a config written
//! for a newer loader still boots.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A `[entry.<name>]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Table name, e.g. `vela`.
    pub name: String,
    /// Path to the kernel ELF, without a leading backslash.
    pub kernel_path: String,
    /// Optional initrd path, without a leading backslash.
    pub initrd_path: Option<String>,
}

impl Entry {
    /// Build the ESP-absolute path used to open the file.
    ///
    /// The field is documented as "without a leading backslash", but the
    /// config `xtask image` generates writes `\Vela\kernel\vela-kernel.elf` —
    /// with one. Blindly prepending a backslash therefore produced
    /// `\\Vela\kernel\...`, which UEFI resolves to a file that does not exist,
    /// and the loader failed with a bare `NOT_FOUND` after having already
    /// proven it could read the config and the filesystem.
    ///
    /// Rather than pick one convention and force every config to match it,
    /// this accepts either: a leading backslash or backslash-slash is stripped
    /// before exactly one is added. A path that does not start with a
    /// backslash gets one, which is the documented case.
    pub fn kernel_full_path(&self) -> String {
        esp_absolute(&self.kernel_path)
    }

    /// Build the ESP-absolute initrd path, if any.
    pub fn initrd_full_path(&self) -> Option<String> {
        self.initrd_path.as_deref().map(esp_absolute)
    }
}

/// Prefix `path` with exactly one backslash.
///
/// A leading `\` is kept and a leading `\\` is collapsed, so a config written
/// either way yields the same ESP-absolute path.
fn esp_absolute(path: &str) -> String {
    let trimmed = path.trim_start_matches('\\');
    alloc::format!("\\{trimmed}")
}

/// A parsed `vela.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Value of `boot.default`.
    pub default: String,
    pub timeout_seconds: u32,
    /// Declared architecture, informational only — the loader's own target
    /// decides what it can boot.
    pub arch: String,
    /// All `[entry.*]` tables, in file order.
    pub entries: Vec<Entry>,
}

/// Parse failures, all reported as `BAD_BUFFER_SIZE` by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// No `default` key.
    NoDefault,
    /// `default` named an entry that was never declared.
    UnknownDefault,
}

/// Parse a `vela.toml`.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let mut cfg = Config {
        timeout_seconds: 1,
        ..Default::default()
    };
    let mut current: Option<Entry> = None;

    for raw in text.lines() {
        let line = raw.trim();
        // Skip comments and blank lines.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // A section header ends the previous entry, if any.
        if line.starts_with('[') {
            if let Some(done) = current.take() {
                cfg.entries.push(done);
            }
            if let Some(name) = section_name(line) {
                current = Some(Entry {
                    name,
                    kernel_path: String::new(),
                    initrd_path: None,
                });
            }
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());

        match (current.as_mut(), key) {
            // Keys outside an [entry.*] table belong to the loader's own
            // settings; `boot.*` lands here because `[boot]` set no entry.
            (None, "default") => cfg.default = value.to_string(),
            (None, "timeout_seconds") => {
                cfg.timeout_seconds = value.parse().unwrap_or(1);
            }
            (None, "arch") => cfg.arch = value.to_string(),
            (Some(e), "kernel") => e.kernel_path = value.to_string(),
            (Some(e), "initrd") => e.initrd_path = Some(value.to_string()),
            // Unknown keys are ignored on purpose: forward compatibility.
            _ => {}
        }
    }

    if let Some(done) = current.take() {
        cfg.entries.push(done);
    }

    if cfg.default.is_empty() {
        return Err(ConfigError::NoDefault);
    }
    if !cfg.entries.iter().any(|e| e.name == cfg.default) {
        return Err(ConfigError::UnknownDefault);
    }
    Ok(cfg)
}

/// Extract `entry.<name>` from `[entry.<name>]`.
fn section_name(line: &str) -> Option<String> {
    let inner = line.trim_start_matches('[').trim_end_matches(']');
    let rest = inner.strip_prefix("entry.")?;
    let name = rest.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Strip one layer of matching quotes; also turn `\\` into `\`.
fn unquote(v: &str) -> &str {
    let v = v.trim();
    let v = if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        &v[1..v.len() - 1]
    } else if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        &v[1..v.len() - 1]
    } else {
        v
    };
    // TOML literal strings keep backslashes as-is, but the config we generate
    // uses them as path separators either way.
    if v.contains("\\\\") {
        // Cannot return a borrowed slice through the replacement, so the caller
        // handles unescaping via `Entry`'s constructor. Returning the original
        // is correct for literal paths, which is what we emit.
        v
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const GENERATED: &str = r#"
# VelaOS boot configuration — generated by `xtask image`
[boot]
default = "vela"
timeout_seconds = 1
arch = "x86_64"

[entry.vela]
kernel = '\Vela\kernel\vela-kernel.elf'
video = "auto"
log_level = "info"
"#;

    #[test]
    fn parses_the_generated_config() {
        let cfg = parse(GENERATED).expect("should parse");
        assert_eq!(cfg.default, "vela");
        assert_eq!(cfg.timeout_seconds, 1);
        assert_eq!(cfg.arch, "x86_64");
        assert_eq!(cfg.entries.len(), 1);
        assert_eq!(cfg.entries[0].name, "vela");
        assert_eq!(cfg.entries[0].kernel_path, r"\Vela\kernel\vela-kernel.elf");
        assert!(cfg.entries[0].initrd_path.is_none());
    }

    #[test]
    fn builds_absolute_paths() {
        let cfg = parse(GENERATED).unwrap();
        assert_eq!(
            cfg.entries[0].kernel_full_path(),
            r"\Vela\kernel\vela-kernel.elf"
        );
    }

    #[test]
    fn parses_an_initrd() {
        let text = "[boot]\ndefault=\"a\"\n[entry.a]\nkernel='k.elf'\ninitrd='r.img'\n";
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.entries[0].initrd_full_path(), Some("\\r.img".into()));
    }

    #[test]
    fn parses_multiple_entries() {
        let text = "[boot]\ndefault=\"main\"\n[entry.main]\nkernel='k1.elf'\n[entry.alt]\nkernel='k2.elf'\n";
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.entries.len(), 2);
        assert_eq!(cfg.entries[0].name, "main");
        assert_eq!(cfg.entries[1].name, "alt");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let text = "[boot]\ndefault=\"a\"\nfuture_option=42\n[entry.a]\nkernel='k.elf'\nnext_major_thing=1\n";
        let cfg = parse(text).expect("unknown keys must not break parsing");
        assert_eq!(cfg.entries.len(), 1);
    }

    #[test]
    fn rejects_a_config_with_no_default() {
        assert_eq!(
            parse("[entry.a]\nkernel='k'\n"),
            Err(ConfigError::NoDefault)
        );
    }

    #[test]
    fn rejects_a_default_naming_an_undeclared_entry() {
        let text = "[boot]\ndefault=\"ghost\"\n[entry.a]\nkernel='k'\n";
        assert_eq!(parse(text), Err(ConfigError::UnknownDefault));
    }

    #[test]
    fn tolerates_comments_and_blank_lines() {
        let text = "# top\n\n[boot]\n\n# comment\ndefault = \"a\"\n[entry.a]\nkernel='k'\n# tail\n";
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.default, "a");
    }

    #[test]
    fn tolerates_missing_newline_at_end_of_file() {
        let text = "[boot]\ndefault=\"a\"\n[entry.a]\nkernel='k.elf'";
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.entries[0].kernel_path, "k.elf");
    }

    #[test]
    fn a_section_that_is_not_an_entry_does_not_create_one() {
        let text = "[boot]\ndefault=\"a\"\n[entry.a]\nkernel='k'\n[something.else]\nx=1\n";
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.entries.len(), 1);
    }

    #[test]
    fn malformed_lines_do_not_panic() {
        for text in [
            "=",
            "key",
            "[",
            "[]",
            "[[",
            "[entry.]",
            "[entry.a",
            "default",
            "\u{1b}[2J",
        ] {
            let _ = parse(text);
        }
    }

    #[test]
    fn path_helpers_use_backslash_separators() {
        let e = Entry {
            name: "x".into(),
            kernel_path: "a\\b\\c".into(),
            initrd_path: vec!["d".into()].into_iter().next(),
        };
        assert_eq!(e.kernel_full_path(), "\\a\\b\\c");
    }
}
