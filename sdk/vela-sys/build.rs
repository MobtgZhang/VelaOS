//! The SDK's generator: one IDL → the jump tables, the offsets, the stubs.
//!
//! # What this generates, and why it is generated at all
//!
//! The AmigaOS calling model is a **table**. A program holds a library base
//! in `A6` and calls `jsr -offset(A6)`, so a library's entire public
//! surface is a list of *offsets into one contiguous table*. Every property
//! that matters — which slot a function occupies, what its offset is, which
//! syscall it reaches — is a relationship between entries in that list.
//!
//! Writing the list by hand means writing the same relationship three times:
//! once in the offset constants, once in the `OFFSETS` array, and once in the
//! jump table itself. Three copies of one list drift, and the drift is
//! *silent*: a function whose offset and slot disagree still compiles, still
//! links, and still answers — just the wrong answer.
//!
//! So the list is written **once**, in `idl/*.yaml`, and everything else is
//! derived from it by this script. The generator refuses to emit anything if
//! the list is not coherent, which turns "the table drifted" from a bug found
//! by a user into a build failure.
//!
//! # The coherence rules the generator enforces
//!
//! 1. Every LVO is **negative** — an AmigaOS library vector offset is
//!    measured downwards from the base.
//! 2. Every LVO is a **multiple of the slot size** (eight bytes), so an
//!    offset names a whole slot and never the middle of one.
//! 3. The offsets of one library **step by exactly one slot**, from the
//!    deepest to the shallowest, so no two functions share a slot and none
//!    is skipped.
//! 4. The deepest offset lands in **slot zero** and the shallowest in slot
//!    `count - 1`, so the slot arithmetic is the identity the base's
//!    `Layout::slot` computes.
//!
//! # What the generator does *not* do
//!
//! It does not check the syscall numbers against the kernel. The kernel's
//! numbers live in a crate the SDK cannot depend on without creating a
//! cycle, so that check lives in the SDK's hosted test, which *can* see both
//! sides. What the generator does check is that each IDL entry's `number`
//! matches the SDK's own `numbers.rs` constant — that catches a typo in the
//! IDL, which is the common case.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The size of one jump-table slot, in bytes.
///
/// Eight, because a slot holds a 64-bit function pointer. The generator's
/// rules 2 and 3 are stated in terms of this.
const SLOT_SIZE: i16 = 8;

/// One function in the IDL.
struct Function {
    /// The AmigaOS name.
    name: String,
    /// The library vector offset.
    lvo: i16,
    /// The SDK's syscall constant, or `None` when the kernel has no such
    /// call.
    syscall: Option<String>,
    /// The syscall number, when there is one.
    number: Option<u64>,
    /// The kernel's constant for the same number, for the conformance doc.
    kernel: Option<String>,
    /// The AmigaOS spelling of the function's name, without underscores.
    ///
    /// `PutMsg` becomes `PUTMSG` and `CreateMsgPort` becomes `CREATEMSGPORT`
    /// — which is how AmigaOS spells these, and how a ported program already
    /// refers to them.
    offset: String,
    /// The Rust name of the generated stub.
    ///
    /// Spelled out in the IDL rather than derived, because the stub names are
    /// the SDK's own API: `CreateMsgPort`'s stub is `create_port`, not the
    /// mechanical `create_msg_port`, and `FindTask`'s is `find_task`.
    stub: String,
    /// The conformance status the API has.
    status: Option<String>,
}

/// One library in the IDL.
struct Library {
    /// The module name: `exec`, `graphics`, and so on.
    module: String,
    /// The AmigaOS library name: `exec.library`.
    name: String,
    /// The prefix for the generated offset constants.
    prefix: String,
    /// The library's functions, deepest offset first.
    functions: Vec<Function>,
}

impl Library {
    /// The name of the generated stub function.
    fn stub_name(&self, function: &Function) -> String {
        function.stub.clone()
    }

    /// The `LibraryId` variant for this library.
    ///
    /// The IDL's module names are lower case — `exec`, `graphics` — while the
    /// enum's variants are the CamelCase spellings the ABI uses. The mapping
    /// is explicit here rather than derived, because a derived one would be a
    /// guess about a name the two sides must agree on exactly.
    fn library_id(&self) -> &'static str {
        match self.module.as_str() {
            "exec" => "Exec",
            "graphics" => "Graphics",
            "layers" => "Layers",
            "intuition" => "Intuition",
            "dos" => "Dos",
            other => panic!("no `LibraryId` variant for the module `{other}`"),
        }
    }

    /// The name of the generated offset constant's stem.
    ///
    /// `EXEC_PUTMSG` for `PutMsg` — the AmigaOS spelling, without the
    /// `_OFFSET` suffix the generated constant carries.
    fn offset_const(&self, function: &Function) -> String {
        format!("{}_{}", self.prefix, function.offset)
    }

    /// The full name of the generated offset constant.
    ///
    /// `EXEC_PUTMSG_OFFSET` — the stem plus the suffix that says what the
    /// constant is.
    fn offset_const_full(&self, function: &Function) -> String {
        format!("{}_OFFSET", self.offset_const(function))
    }

    /// The name of the flat, Amiga-spelled offset constant.
    ///
    /// `EXEC_PUTMSG` rather than `EXEC_PUTMSG_OFFSET`: a program writes
    /// `base.call(offset::EXEC_PUTMSG, ..)`, and the `_OFFSET` suffix is a
    /// generator detail that has no business in the program's source.
    fn flat_offset_const(&self, function: &Function) -> String {
        self.offset_const(function)
    }
}

/// Parse one IDL file.
///
/// The parser is deliberately small: it reads the exact subset of YAML the
/// IDL uses, and it *fails* on anything it does not understand rather than
/// guessing. A generator that silently ignores a key it does not recognise
/// is a generator that produces a table missing a function, which is exactly
/// the failure the whole exercise exists to prevent.
fn parse(text: &str, path: &Path) -> Result<Library, String> {
    let module = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("{}: the file name must be a module name", path.display()))?
        .to_string();
    let mut name = String::new();
    let mut prefix = String::new();
    let mut functions: Vec<Function> = Vec::new();
    let mut current: Option<Function> = None;

    for (number, raw) in text.lines().enumerate() {
        let line = match raw.split('#').next() {
            Some(part) => part.trim(),
            None => "",
        };
        if line.is_empty() {
            continue;
        }
        // A list item begins a new function.
        let starts_item = line.starts_with("- ");
        let body = if starts_item { line[2..].trim() } else { line };
        let Some((key, value)) = body.split_once(':') else {
            return Err(format!(
                "{}:{}: expected `key: value`, found `{line}`",
                path.display(),
                number + 1
            ));
        };
        let key = key.trim();
        let value = value.trim();

        if starts_item {
            if let Some(function) = current.take() {
                functions.push(function);
            }
            current = Some(Function {
                name: String::new(),
                lvo: 0,
                syscall: None,
                number: None,
                kernel: None,
                offset: String::new(),
                stub: String::new(),
                status: None,
            });
        }

        match key {
            "library" => name = value.to_string(),
            "prefix" => prefix = value.to_string(),
            "functions" => {}
            "name" => {
                if let Some(function) = current.as_mut() {
                    function.name = value.to_string();
                }
            }
            "lvo" => {
                let parsed: i16 = value.parse().map_err(|_| {
                    format!(
                        "{}:{}: `lvo` must be an integer",
                        path.display(),
                        number + 1
                    )
                })?;
                if let Some(function) = current.as_mut() {
                    function.lvo = parsed;
                }
            }
            "syscall" => {
                if let Some(function) = current.as_mut() {
                    function.syscall = if value == "none" {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
            }
            "number" => {
                let parsed: u64 = value.parse().map_err(|_| {
                    format!(
                        "{}:{}: `number` must be an integer",
                        path.display(),
                        number + 1
                    )
                })?;
                if let Some(function) = current.as_mut() {
                    function.number = Some(parsed);
                }
            }
            "kernel" => {
                if let Some(function) = current.as_mut() {
                    function.kernel = Some(value.to_string());
                }
            }
            "offset" => {
                if let Some(function) = current.as_mut() {
                    function.offset = value.to_string();
                }
            }
            "stub" => {
                if let Some(function) = current.as_mut() {
                    function.stub = value.to_string();
                }
            }
            "status" => {
                if let Some(function) = current.as_mut() {
                    function.status = Some(value.to_string());
                }
            }
            other => {
                return Err(format!(
                    "{}:{}: unknown key `{other}`",
                    path.display(),
                    number + 1
                ))
            }
        }
    }
    if let Some(function) = current.take() {
        functions.push(function);
    }

    if name.is_empty() {
        return Err(format!("{}: the file has no `library` key", path.display()));
    }
    if prefix.is_empty() {
        return Err(format!("{}: the file has no `prefix` key", path.display()));
    }
    if functions.is_empty() {
        return Err(format!("{}: the file has no functions", path.display()));
    }
    for function in &functions {
        if function.name.is_empty() {
            return Err(format!("{}: a function has no `name`", path.display()));
        }
        if function.offset.is_empty() {
            return Err(format!(
                "{}: `{}` has no `offset`",
                path.display(),
                function.name
            ));
        }
        if function.stub.is_empty() {
            return Err(format!(
                "{}: `{}` has no `stub`",
                path.display(),
                function.name
            ));
        }
    }
    Ok(Library {
        module,
        name,
        prefix,
        functions,
    })
}

/// Check that a library's offsets are coherent.
///
/// The four rules, each with the reason it exists.
fn check(library: &Library) -> Result<(), String> {
    let mut previous: Option<i16> = None;
    for (index, function) in library.functions.iter().enumerate() {
        if function.lvo >= 0 {
            return Err(format!(
                "{}: `{}` has LVO {}, which is not negative",
                library.name, function.name, function.lvo
            ));
        }
        if function.lvo % SLOT_SIZE != 0 {
            return Err(format!(
                "{}: `{}` has LVO {}, which is not a whole number of {SLOT_SIZE}-byte slots",
                library.name, function.name, function.lvo
            ));
        }
        if let Some(previous) = previous {
            if function.lvo - previous != SLOT_SIZE {
                return Err(format!(
                    "{}: `{}` has LVO {}, which does not step by {SLOT_SIZE} from {}",
                    library.name, function.name, function.lvo, previous
                ));
            }
        }
        previous = Some(function.lvo);
        if index == 0 && function.lvo != -(library.functions.len() as i16) * SLOT_SIZE {
            return Err(format!(
                "{}: the deepest offset is {}, which does not put the last function in slot zero \
                 for {} functions",
                library.name,
                function.lvo,
                library.functions.len()
            ));
        }
    }
    Ok(())
}

/// The offset constants, the `OFFSETS` array, the stubs and the jump table,
/// for one library.
fn generate_library(library: &Library) -> String {
    let mut out = String::new();

    out.push_str("    /// The library vector offsets, one per function.\n");
    out.push_str("    ///\n");
    out.push_str("    /// Negative, and one 8-byte slot apart, because a jump-table slot is\n");
    out.push_str("    /// one 64-bit function pointer wide. The values are generated from the\n");
    out.push_str("    /// IDL, so they cannot drift from the table they describe.\n");
    for function in &library.functions {
        let _ = writeln!(
            out,
            "    /// `{}::{}`.\n    pub const {}: i16 = {};\n",
            library.name,
            function.name,
            library.offset_const_full(function),
            function.lvo
        );
    }

    let _ = writeln!(
        out,
        "    /// Every offset this library defines, in table order: deepest first.\n    pub const OFFSETS: [i16; {}] = [",
        library.functions.len()
    );
    for function in &library.functions {
        let _ = writeln!(out, "        {},", library.offset_const_full(function));
    }
    out.push_str("    ];\n");

    for function in &library.functions {
        let body = match &function.syscall {
            Some(constant) => format!(
                "    // SAFETY: forwarded from the caller, which makes the same promise\n    // about the arguments.\n    unsafe {{ crate::library_base::issue(crate::numbers::{}, [_a0, _a1, _a2, _a3, _a4, _a5]) }}",
                constant
            ),
            None => "    // The kernel does not expose this call, so the stub says so rather\n    // than issuing some other library's syscall — a stub that quietly did\n    // the wrong thing would be worse than one that admits it does nothing.\n    crate::library_base::pack(0, crate::numbers::errors::NOT_IMPLEMENTED)"
                .to_string(),
        };
        let _ = writeln!(
            out,
            "\n    /// The stub for `{}::{}`, at library vector offset {}.\n    ///\n    /// # Safety\n    ///\n    /// Every pointer among the arguments is validated by the kernel, so a bad\n    /// one is an error rather than a fault.\n    pub unsafe extern \"sysv64\" fn stub_{}(\n        _a0: u64,\n        _a1: u64,\n        _a2: u64,\n        _a3: u64,\n        _a4: u64,\n        _a5: u64,\n    ) -> u64 {{\n{}\n    }}\n",
            library.name,
            function.name,
            function.lvo,
            library.stub_name(function),
            body
        );
    }

    let _ = writeln!(
        out,
        "    /// The jump table for `{}`.\n    ///\n    /// An immutable `static`, so the linker puts it in read-only storage and\n    /// nothing — not the program, not a stray write through a base address —\n    /// can change what a call resolves to.\n    pub(crate) static TABLE: crate::library_base::JumpTable = crate::library_base::build_table(&[",
        library.name
    );
    for function in &library.functions {
        let _ = writeln!(
            out,
            "        (LAYOUT, {}, stub_{} as crate::library_base::Slot),",
            library.offset_const_full(function),
            library.stub_name(function)
        );
    }
    out.push_str("    ]);\n");

    out
}

/// The whole generated file.
fn generate(libraries: &[Library]) -> String {
    let mut out = String::new();
    out.push_str(
        "// @generated by `sdk/vela-sys/build.rs` from `sdk/vela-sys/idl/*.yaml`.\n\
         //\n\
         // Do not edit this file. Edit the IDL and rebuild: the whole point of\n\
         // generating it is that the offsets, the slot list and the jump table\n\
         // cannot drift apart, and hand-editing any one of them breaks that.\n\
         //\n\
         // The generator refuses to emit anything if the IDL is not coherent —\n\
         // negative offsets, whole slots, one slot apart, deepest in slot zero —\n\
         // so a broken table is a build failure rather than a wrong answer.\n\n",
    );

    for library in libraries {
        let _ = writeln!(
            out,
            "/// The `{}` jump table and the offsets that address it.\npub mod {} {{\n",
            library.name, library.module
        );
        let _ = writeln!(
            out,
            "    /// The library's slot layout.\n    pub const LAYOUT: crate::library_base::Layout = crate::library_base::Layout {{\n        count: {},\n        name: \"{}\",\n    }};\n",
            library.functions.len(),
            library.name
        );
        out.push_str(&generate_library(library));
        out.push_str(
            "\n    /// The library's base.\n    pub fn base() -> crate::library_base::LibraryBase {\n        // SAFETY: `TABLE` is this library's own `static mut`, initialised at\n        // compile time and never written again.\n        unsafe { crate::library_base::base_of(&raw const TABLE, LAYOUT) }\n    }\n}\n\n",
        );
    }

    // The flat, Amiga-spelled offset constants: the names a program writes.
    out.push_str(
        "/// Every library vector offset, under the name a ported program uses.\n\
         ///\n\
         /// The generated constants are named `<PREFIX>_<FUNCTION>_OFFSET`; these\n\
         /// are named the way AmigaOS names them — `EXEC_PUTMSG`, not\n\
         /// `EXEC_PUTMSG_OFFSET` — because that is the name the program in front of\n\
         /// this SDK already has in its source.\n\
         pub mod offset {\n",
    );
    for library in libraries {
        for function in &library.functions {
            let _ = writeln!(
                out,
                "    /// `{}::{}`.\n    pub use super::{}::{} as {};",
                library.name,
                function.name,
                library.module,
                library.offset_const_full(function),
                library.flat_offset_const(function)
            );
        }
    }
    out.push_str("}\n\n");

    // The two registry tables the base resolves through.
    let _ = writeln!(
        out,
        "/// Every library that has a jump table: its id, layout, and offsets.\npub(crate) const TABLES: [(crate::abi::LibraryId, crate::library_base::Layout, &[i16]); {}] = [",
        libraries.len()
    );
    for library in libraries {
        let _ = writeln!(
            out,
            "    (crate::abi::LibraryId::{}, {}::LAYOUT, &{}::OFFSETS),",
            library.library_id(),
            library.module,
            library.module
        );
    }
    out.push_str("];\n\n");

    let _ = writeln!(
        out,
        "/// The tables themselves, by library.\npub(crate) const TABLE_PTRS: [(*const crate::library_base::JumpTable, crate::library_base::Layout); {}] = [",
        libraries.len()
    );
    for library in libraries {
        let _ = writeln!(
            out,
            "    (&raw const {}::TABLE, {}::LAYOUT),",
            library.module, library.module
        );
    }
    out.push_str("];\n");

    out
}

/// One library's C header, in the AmigaOS library-vector idiom.
///
/// The header exists so that a C program can be compiled against VelaOS's
/// libraries the same way it would be against AmigaOS's: it includes the
/// header, calls `base->Foo(...)`, and the linker resolves `Foo` through the
/// library vector. The offsets in it are the same ones the Rust jump table
/// uses, because both come from the same IDL.
fn generate_header(library: &Library) -> String {
    let mut out = String::new();
    let guard = library.name.replace('.', "_").to_uppercase();
    let _ = writeln!(
        out,
        "/* @generated from `sdk/vela-sys/idl/{}.yaml`. */",
        library.module
    );
    let _ = writeln!(out, "#ifndef VELA_{guard}_H");
    let _ = writeln!(out, "#define VELA_{guard}_H");
    let _ = writeln!(out);
    let _ = writeln!(out, "#include <stdint.h>");
    let _ = writeln!(out);
    let _ = writeln!(out, "/* The library vector offsets, in slots. */");
    for function in &library.functions {
        let _ = writeln!(
            out,
            "#define {}_{}_OFFSET {}",
            library.prefix, function.offset, function.lvo
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "/* The library vector, indexed by slot. */");
    for (index, function) in library.functions.iter().enumerate() {
        let _ = writeln!(
            out,
            "typedef long (*{}_fn)(long, long, long, long, long, long);\n\
             #define {}_{}_SLOT {}",
            library.prefix, library.prefix, function.offset, index
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "#endif /* VELA_{guard}_H */");
    out
}

/// The conformance status document: one table for the whole SDK.
///
/// The plan asks for "one IDL → Rust SDK, C headers, LVO table, conformance
/// status doc", and this is the fourth of those. It is generated rather than
/// written so that a status cannot be recorded for a function that does not
/// exist.
fn generate_conformance(libraries: &[Library]) -> String {
    let mut out = String::new();
    out.push_str("# VelaOS library conformance\n\n");
    out.push_str(
        "Generated from `sdk/vela-sys/idl/*.yaml`. Every row is a function the SDK\n\
         actually has a stub for, so a status cannot be recorded for a function that\n\
         does not exist.\n\n",
    );
    for library in libraries {
        let _ = writeln!(out, "## `{}`\n", library.name);
        out.push_str("| function | LVO | syscall | status |\n");
        out.push_str("|---|---|---|---|\n");
        for function in &library.functions {
            let syscall = match (&function.syscall, function.number) {
                (Some(constant), Some(number)) => format!("`{constant}` ({number})"),
                _ => "—".to_string(),
            };
            let status = function.status.as_deref().unwrap_or("implemented");
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | `{}` |",
                function.name, function.lvo, syscall, status
            );
        }
        out.push('\n');
    }
    out
}

/// The syscall numbers the SDK's `numbers.rs` declares, read from the source.
///
/// The SDK's own constants are the ones the generated stubs use, and the IDL
/// names them. Checking the IDL against the source means a typo in the IDL
/// is a build failure rather than a stub that issues the wrong syscall.
fn sdk_numbers(source: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for line in source.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(": u64 = ") else {
            continue;
        };
        let name = name.trim();
        let Ok(value) = value.trim().trim_end_matches(';').parse::<u64>() else {
            continue;
        };
        out.insert(name.to_string(), value);
    }
    out
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let idl_dir = manifest.join("idl");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));

    let mut paths: Vec<PathBuf> = std::fs::read_dir(&idl_dir)
        .unwrap_or_else(|e| panic!("{}: {e}", idl_dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
        .collect();
    paths.sort();

    let mut libraries = Vec::new();
    for path in &paths {
        let text = std::fs::read_to_string(path).expect("the IDL file must be readable");
        let library = parse(&text, path).unwrap_or_else(|e| panic!("{e}"));
        check(&library).unwrap_or_else(|e| panic!("{e}"));
        libraries.push(library);
    }
    assert!(!libraries.is_empty(), "the SDK needs at least one IDL file");

    // Check each IDL entry against the SDK's own numbers.
    let numbers_source =
        std::fs::read_to_string(manifest.join("src").join("numbers.rs")).expect("numbers.rs");
    let numbers = sdk_numbers(&numbers_source);
    for library in &libraries {
        for function in &library.functions {
            let (Some(constant), Some(number)) = (&function.syscall, function.number) else {
                continue;
            };
            let Some(declared) = numbers.get(constant) else {
                panic!(
                    "{}: the IDL names `{constant}`, which `numbers.rs` does not declare",
                    library.name
                );
            };
            assert_eq!(
                *declared, number,
                "{}: the IDL says `{constant}` is {number}, but `numbers.rs` says {declared}",
                library.name
            );
        }
    }

    let generated = generate(&libraries);
    std::fs::write(out_dir.join("library_tables.rs"), generated)
        .expect("the generated file must be writable");

    // The C headers, one per library.
    let header_dir = out_dir.join("headers");
    std::fs::create_dir_all(&header_dir).expect("the header directory must be creatable");
    for library in &libraries {
        let path = header_dir.join(format!("{}.h", library.module));
        std::fs::write(&path, generate_header(library))
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }

    // The conformance status document.
    std::fs::write(
        out_dir.join("conformance.md"),
        generate_conformance(&libraries),
    )
    .expect("the conformance document must be writable");

    println!("cargo:rerun-if-changed={}", idl_dir.display());
}
