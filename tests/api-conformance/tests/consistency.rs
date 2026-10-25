//! The consistency between the matrix and the tests that back it.
//!
//! The matrix in `tests/api-conformance/src/lib.rs` is the *input* to the
//! test suite, not its output. A test that read a status out of the same
//! place it wrote it would assert nothing. So the matrix names the test
//! that holds each `verified` API to its promise, and this file checks that
//! the named tests really exist.
//!
//! The check is done by *reading the test sources* rather than by asking
//! the test binaries what they contain: a test binary's list of test names
//! is not available to another test binary, and the sources are what a
//! reviewer reads anyway.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use vela_test_api_conformance::{Status, LIBRARIES, MATRIX};

/// The workspace root.
fn root() -> PathBuf {
    vela_test_api_conformance::workspace_root()
}

/// The directory the behaviour tests live in.
fn tests_dir() -> PathBuf {
    root().join("tests/api-conformance/tests")
}

/// Every `#[test]` function name in the test sources.
fn declared_tests() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let dir = tests_dir();
    let entries = std::fs::read_dir(&dir).expect("the tests directory must exist");
    for entry in entries {
        let path = entry.expect("a directory entry must be readable").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a test source must be readable");
        for name in test_names_in(&text) {
            names.insert(name);
        }
    }
    names
}

/// The names of the `#[test]` functions in a Rust source.
///
/// The scan is textual rather than a parse, because the test sources are
/// machine-written and follow one shape: `#[test]` on its own line, then
/// `fn <name>`.
fn test_names_in(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut expect_fn = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "#[test]" {
            expect_fn = true;
            continue;
        }
        if !expect_fn {
            continue;
        }
        expect_fn = false;
        let Some(rest) = trimmed.strip_prefix("fn ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            names.push(name);
        }
    }
    names
}

/// Every `verified` entry names a test that exists.
#[test]
fn every_verified_entry_names_a_test_that_exists() {
    let declared = declared_tests();
    let mut missing = Vec::new();
    for entry in MATRIX {
        let Some(name) = entry.verified_by else {
            continue;
        };
        if !declared.contains(name) {
            missing.push(format!("{}::{} -> {name}", entry.library, entry.name));
        }
    }
    assert!(
        missing.is_empty(),
        "these entries claim to be verified by a test that does not exist:\n{}",
        missing.join("\n"),
    );
}

/// A `verified` entry always names a test, and only a `verified` entry
/// does.
///
/// The two halves together mean the column cannot lie in either direction:
/// a `verified` API without a test, and a test without a `verified` API.
#[test]
fn the_verified_by_column_is_exactly_the_verified_entries() {
    for entry in MATRIX {
        match entry.status {
            Status::Verified => {
                assert!(
                    entry.verified_by.is_some(),
                    "{}::{} is verified but names no test",
                    entry.library,
                    entry.name,
                );
            }
            _ => {
                assert!(
                    entry.verified_by.is_none(),
                    "{}::{} is {} but names a test",
                    entry.library,
                    entry.name,
                    entry.status.as_str(),
                );
            }
        }
    }
}

/// The matrix's own helper agrees with the matrix.
///
/// `verified_test_names` is what the generated documents use, so a helper
/// that disagreed with the table would produce documents that lie.
#[test]
fn the_helper_agrees_with_the_table() {
    let mut from_table: Vec<&str> = MATRIX
        .iter()
        .filter_map(|e| e.verified_by)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    from_table.sort_unstable();
    let from_helper = vela_test_api_conformance::verified_test_names();
    assert_eq!(from_table, from_helper);
}

/// Every syscall number in the matrix is inside the range its library owns.
///
/// The numbering is grouped by library, so a number outside the group's
/// range would be a number the dispatcher cannot route.
#[test]
fn every_syscall_number_is_inside_its_library_range() {
    for entry in MATRIX {
        let Some(number) = entry.syscall else {
            continue;
        };
        let (first, count) = range_for(entry.library);
        assert!(
            number >= first && number < first + count,
            "{}::{} has syscall {number}, outside {first}..{}",
            entry.library,
            entry.name,
            first + count,
        );
    }
}

/// The first syscall number and the count of a library's group.
fn range_for(library: &str) -> (u64, u64) {
    match library {
        "exec.library" => (0, 20),
        "graphics.library" => (64, 13),
        "layers.library" => (128, 10),
        "intuition.library" => (192, 13),
        "dos.library" => (256, 11),
        "utility.library" => (320, 3),
        other => panic!("unknown library {other}"),
    }
}

/// The matrix's libraries are exactly the ones the ABI defines.
#[test]
fn the_matrix_covers_every_library() {
    assert_eq!(LIBRARIES.len(), 6);
    for library in LIBRARIES {
        assert!(
            !vela_test_api_conformance::entries_for(library).is_empty(),
            "{library} has no entries at all",
        );
    }
}

/// Every entry's status is one of the four the taxonomy defines.
#[test]
fn every_status_is_one_of_the_four() {
    for entry in MATRIX {
        assert!(
            Status::parse(entry.status.as_str()).is_some(),
            "{}::{} has an unknown status {}",
            entry.library,
            entry.name,
            entry.status.as_str(),
        );
    }
}

/// The generated documents, written when the update flag is set.
///
/// Both document tests call this, so that the update mode writes the
/// documents *before* either test looks at them: the two tests run in
/// parallel, and a test that wrote the documents while another test was
/// looking for them would fail for a reason that has nothing to do with the
/// documents.
fn documents() -> PathBuf {
    let docs = root().join("docs/amiga-api");
    let update = std::env::var("VELA_UPDATE_CONFORMANCE_DOCS")
        .map(|v| v == "1")
        .unwrap_or(false);
    if update {
        std::fs::create_dir_all(&docs).expect("the docs directory must be creatable");
        for (name, expected) in vela_test_api_conformance::render_all() {
            write_atomically(&docs.join(&name), &expected);
        }
    }
    docs
}

/// Write a file so that a concurrent reader never sees it half-written.
///
/// # Why the rename is necessary
///
/// The two document tests both call [`documents`], and cargo runs the tests
/// in a file in parallel threads. A plain `std::fs::write` truncates the
/// destination before it writes, so a reader in the other thread could
/// observe an **empty** file and report the document as out of date — a
/// failure that has nothing to do with the documents.
///
/// Writing to a temporary file and renaming it over the destination makes
/// the replacement atomic: the reader sees either the old content or the
/// new content, never a partial one.
///
/// The temporary name carries the thread's identity, so two threads
/// regenerating the same document at the same time do not write to the same
/// temporary file.
fn write_atomically(path: &Path, content: &str) {
    let thread = format!("{:?}", std::thread::current().id());
    let temp = path.with_extension(format!("tmp-{thread}"));
    std::fs::write(&temp, content).expect("the temporary document must be writable");
    std::fs::rename(&temp, path).expect("the temporary document must be renamed into place");
}

/// The generated documents exist and match the matrix.
///
/// The documents under `docs/amiga-api/` are generated from this table, and
/// this test is what keeps them honest. Setting
/// `VELA_UPDATE_CONFORMANCE_DOCS=1` rewrites them instead of comparing.
#[test]
fn the_generated_documents_match_the_matrix() {
    let docs = documents();

    if !docs.exists() {
        panic!(
            "docs/amiga-api does not exist; run \
             `VELA_UPDATE_CONFORMANCE_DOCS=1 cargo test -p vela-test-api-conformance`",
        );
    }

    for (name, expected) in vela_test_api_conformance::render_all() {
        let path = docs.join(&name);
        let actual = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("{} must exist and be readable: {e}", path.display());
        });
        assert_eq!(
            actual,
            expected,
            "{} is out of date; regenerate it with \
             VELA_UPDATE_CONFORMANCE_DOCS=1 cargo test -p vela-test-api-conformance",
            path.display(),
        );
    }
}

/// The documents directory is where the plan says it is.
#[test]
fn the_documents_live_where_the_plan_says() {
    let docs = documents();
    for stem in ["exec", "dos", "graphics", "layers", "intuition", "utility"] {
        let path: PathBuf = docs.join(format!("{stem}-conformance-matrix.md"));
        assert!(path.exists(), "{} must exist", path.display());
    }
}
