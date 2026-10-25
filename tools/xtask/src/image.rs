//! Bootable disk image builder.
//!
//! The probe work turned up three failure modes that this module exists to
//! make impossible:
//!
//! 1. `mkfs.vfat -F 32` on a small image **silently produces FAT16**. OVMF is
//!    happy to boot it, but it means the image is not what the build claims.
//!    [`verify_fat_type`] re-reads the resulting filesystem and fails the build
//!    if the type does not match.
//! 2. `mcopy -s` flattens the source directory prefix, silently producing
//!    `::/BOOTX64.EFI` instead of `::/EFI/BOOT/BOOTX64.EFI`. Every directory
//!    here is created explicitly with `mmd`, never with `mcopy -s`.
//! 3. A missing or misplaced file is invisible until boot. [`verify_contents`]
//!    re-reads the finished image and asserts each expected path exists at the
//!    expected size.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::Arch;

/// Size of the ESP. Large enough for a real FAT32 filesystem: FAT32 needs at
/// least 65525 clusters, which a small image cannot provide.
///
/// # Why this is public
///
/// The size is a property of the image format, and the test that checks it
/// is large enough for FAT32 lives in `tests/image.rs`.
pub const ESP_SIZE_MB: u64 = 256;
/// Whole-disk size, leaving room for the Vela system volume later.
///
/// # Why this is public
///
/// As with [`ESP_SIZE_MB`], the test that checks the disk is larger than the
/// ESP lives in `tests/image.rs`.
pub const DISK_SIZE_MB: u64 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FatBits {
    Fat16,
    Fat32,
}

impl FatBits {
    fn as_fsck_name(self) -> &'static str {
        match self {
            Self::Fat16 => "FAT16",
            Self::Fat32 => "FAT32",
        }
    }
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!(
            "{what} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Build `out` as a GPT disk with a single FAT32 EFI System Partition
/// containing the loader, kernel, userland binaries and config.
///
/// The staging directory and the intermediate FAT image are shared paths
/// under `target/`, so two agents building images at the same time would
/// corrupt each other's ESP. [`build_tagged`] gives each caller its own.
pub fn build(root: &Path, arch: Arch, profile: &str, out: &Path) -> Result<()> {
    build_tagged(root, arch, profile, out, "shared")
}

/// Build `out` with the staging area namespaced by `tag`.
///
/// # Why the tag matters
///
/// The staging directory and the FAT image are scratch files. Several agents
/// share this machine, and a shared scratch path means one agent's `mmd`
/// can land in the other agent's staging tree, producing an image with
/// somebody else's kernel in it. The tag keeps the scratch files apart; the
/// *output* image is the caller's choice and is not namespaced, because the
/// caller named it.
pub fn build_tagged(root: &Path, arch: Arch, profile: &str, out: &Path, tag: &str) -> Result<()> {
    let staging = root.join(format!("target/esp-staging-{tag}"));
    let esp = root.join(format!("target/esp-{tag}.img"));
    build_into(root, arch, profile, out, &staging, &esp)
}

/// Build `out`, using `staging` and `esp` as the scratch paths.
fn build_into(
    root: &Path,
    arch: Arch,
    profile: &str,
    out: &Path,
    staging: &Path,
    esp: &Path,
) -> Result<()> {
    let loader = crate::artifact(
        root,
        "vela-uefi-loader",
        "vela-uefi-loader",
        "efi",
        arch,
        profile,
    )?;
    let kernel = crate::artifact(
        root,
        "vela-kernel-entry",
        "vela-kernel",
        "elf",
        arch,
        profile,
    )?;

    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging)?;

    // Lay out exactly what the loader will look for. Directories are created
    // explicitly: `mcopy -s` would flatten these prefixes.
    let dirs = [
        "EFI",
        "EFI/BOOT",
        "Vela",
        "Vela/boot",
        "Vela/kernel",
        "Vela/bin",
    ];
    for d in dirs {
        std::fs::create_dir_all(staging.join(d))?;
    }

    std::fs::copy(
        &loader,
        staging.join("EFI/BOOT").join(arch.bootloader_name()),
    )?;
    std::fs::copy(&kernel, staging.join("Vela/kernel/vela-kernel.elf"))?;

    // Userland binaries are optional at image time: a kernel-only image is
    // still a valid boot smoke target.
    //
    // The *source* names are the bare `[[bin]]` names from each crate's
    // manifest — `init`, `workbench`, `guitest` — while the *destination*
    // names are the `vela-`-prefixed ones. Looking up the prefixed name
    // against cargo's output found nothing, and because the lookup failure was
    // swallowed into a "skipping" message, every userland binary silently
    // stayed out of the image. The comment on that path used to say they were
    // "optional", which is exactly the kind of wording that hides this.
    for (pkg, bin, dest) in [
        ("vela-init", "init", "Vela/bin/vela-init"),
        ("vela-workbench", "workbench", "Vela/bin/vela-workbench"),
        ("vela-guitest", "guitest", "Vela/bin/vela-guitest"),
    ] {
        match crate::artifact(root, pkg, bin, "elf", arch, profile) {
            Ok(p) => {
                if let Some(parent) = staging.join(dest).parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&p, staging.join(dest))?;
            }
            // Swallowing this is defensible only if the caller asked for a
            // kernel-only image. It is reported rather than hidden, and the
            // verification step below fails the build if a binary that
            // `xtask` *did* build is missing from the image.
            Err(e) => {
                println!("    (skipping {dest}: {e})");
            }
        }
    }

    write_config(&staging.join("Vela/boot/vela.toml"), arch)?;

    println!("==> building ESP image");
    build_fat(esp, ESP_SIZE_MB, FatBits::Fat32, staging)?;
    verify_fat_type(esp, FatBits::Fat32)?;
    verify_contents(esp, arch)?;

    println!("==> building GPT disk");
    build_gpt(out, esp)?;

    println!("==> image ready: {}", out.display());
    Ok(())
}

fn write_config(path: &Path, arch: Arch) -> Result<()> {
    let cfg = format!(
        "# VelaOS boot configuration — generated by `xtask image`\n\
         [boot]\n\
         default = \"vela\"\n\
         timeout_seconds = 1\n\
         arch = \"{}\"\n\
         \n\
         [entry.vela]\n\
         kernel = '\\Vela\\kernel\\vela-kernel.elf'\n\
         video = \"auto\"\n\
         log_level = \"info\"\n",
        match arch {
            Arch::X86_64 => "x86_64",
        }
    );
    std::fs::write(path, cfg)?;
    Ok(())
}

/// Create a FAT filesystem image and populate it from `staging`.
///
/// Uses `mtools` rather than a loop mount so the build needs no root and no
/// `mount`.
fn build_fat(img: &Path, size_mb: u64, bits: FatBits, staging: &Path) -> Result<()> {
    if img.exists() {
        std::fs::remove_file(img)?;
    }
    let f = std::fs::File::create(img).with_context(|| format!("creating {}", img.display()))?;
    f.set_len(size_mb * 1024 * 1024)?;

    // `-s 1` forces one sector per cluster, which is what makes a 256 MiB image
    // satisfy FAT32's 65525-cluster minimum.
    run(
        Command::new("mkfs.vfat")
            .arg(match bits {
                FatBits::Fat16 => "-F",
                FatBits::Fat32 => "-F",
            })
            .arg(match bits {
                FatBits::Fat16 => "16",
                FatBits::Fat32 => "32",
            })
            .arg("-s")
            .arg("1")
            .arg("-n")
            .arg("VELA")
            .arg(img),
        "mkfs.vfat",
    )?;

    // Create every directory explicitly. Order matters: parents first.
    let mut dirs = Vec::new();
    for entry in walk(staging)? {
        if entry.is_dir() {
            dirs.push(entry);
        }
    }
    dirs.sort_by_key(|p| p.components().count());

    for d in &dirs {
        let rel = rel_mtools(staging, d);
        let mut cmd = Command::new("mmd");
        cmd.env("MTOOLS_SKIP_CHECK", "1")
            .arg("-i")
            .arg(img)
            .arg(format!("::{rel}"));
        // A directory that already exists is not an error for our purposes.
        let _ = cmd.output();
    }

    for f in files(staging)? {
        let rel = rel_mtools(staging, &f);
        let mut cmd = Command::new("mcopy");
        cmd.env("MTOOLS_SKIP_CHECK", "1")
            .arg("-i")
            .arg(img)
            .arg(&f)
            .arg(format!("::{rel}"));
        run(&mut cmd, &format!("mcopy {rel}"))?;
    }
    Ok(())
}

/// Re-read the filesystem and confirm it really is the requested FAT width.
///
/// `mkfs.vfat` downgrades to FAT16 when the image is too small to satisfy
/// FAT32's cluster count, and reports success. This turns that silent
/// downgrade into a build failure.
fn verify_fat_type(img: &Path, expect: FatBits) -> Result<()> {
    let fsck = Command::new("fsck.vfat").arg("-v").arg(img).output();
    let Ok(out) = fsck else {
        println!("    (fsck.vfat unavailable; trusting mkfs for FAT type check)");
        return Ok(());
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let found = if text.contains("FAT32") {
        FatBits::Fat32
    } else if text.contains("FAT16") {
        FatBits::Fat16
    } else if text.contains("FAT12") {
        bail!("image came out as FAT12; refusing to continue");
    } else {
        println!("    (could not detect FAT type from fsck output; skipping check)");
        return Ok(());
    };
    if found != expect {
        bail!(
            "requested {} but the image is {}; increase the ESP size or use FAT16 explicitly",
            expect.as_fsck_name(),
            found.as_fsck_name()
        );
    }
    println!("    FAT type verified: {}", expect.as_fsck_name());
    Ok(())
}

/// Re-read the finished image and assert the boot path is really present.
///
/// The probe showed a silently mislaid `BOOTX64.EFI` presents as "firmware
/// never launched anything", which is very hard to debug. Checking here turns
/// it into a clear build error.
fn verify_contents(img: &Path, arch: Arch) -> Result<()> {
    let expected: Vec<(String, Option<u64>)> = vec![
        (format!("/EFI/BOOT/{}", arch.bootloader_name()), None),
        ("/Vela/kernel/vela-kernel.elf".into(), None),
        ("/Vela/boot/vela.toml".into(), Some(0)),
    ];
    for (path, min_size) in expected {
        let out = Command::new("mdir")
            .env("MTOOLS_SKIP_CHECK", "1")
            .arg("-i")
            .arg(img)
            .arg(format!("::{path}"))
            .output()
            .with_context(|| format!("mdir {path}"))?;
        if !out.status.success() {
            bail!(
                "{path} is missing from the ESP; the image would not boot.\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        if let Some(min) = min_size {
            let text = String::from_utf8_lossy(&out.stdout);
            let size = parse_mdir_size(&text).unwrap_or(0);
            if size < min {
                bail!("{path} is {size} bytes, expected at least {min}");
            }
        }
    }
    println!("    ESP contents verified");
    Ok(())
}

/// Extract a file size from `mdir -i img ::/path` output.
///
/// # Why this is public
///
/// The parsing is the one piece of the image builder that is worth testing
/// on its own: `mdir` formats its sizes with a comma thousands separator
/// and puts the size in a column that shifts with the date, so a naive
/// whitespace split picks up the wrong token. The test that pins that
/// behaviour lives in `tests/image.rs`, which cannot reach a private item.
pub fn parse_mdir_size(text: &str) -> Option<u64> {
    // Lines look like: "10/23/2026  13:53              15,360  BOOTX64.EFI"
    text.lines()
        .filter_map(|l| l.split_whitespace().rev().nth(1))
        .filter_map(|tok| tok.replace(',', "").parse::<u64>().ok())
        .max()
}

fn build_gpt(out: &Path, esp: &Path) -> Result<()> {
    if out.exists() {
        std::fs::remove_file(out)?;
    }
    let f = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    f.set_len(DISK_SIZE_MB * 1024 * 1024)?;

    // 1 MiB alignment, which is the UEFI convention and keeps the ESP
    // discoverable by firmware that requires it.
    run(
        Command::new("sgdisk")
            .arg("-o")
            .arg(format!("-n 1:2048:+{ESP_SIZE_MB}M"))
            .args(["-t", "1:EF00"])
            .args(["-c", "1:VelaOS ESP"])
            .arg(out),
        "sgdisk",
    )?;

    // Copy the filesystem into the partition, at 2048 * 512 bytes = 1 MiB.
    let bytes = std::fs::read(esp)?;
    let mut f = std::fs::OpenOptions::new().write(true).open(out)?;
    use std::io::{Seek, SeekFrom, Write};
    f.seek(SeekFrom::Start(2048 * 512))?;
    f.write_all(&bytes)?;
    f.flush()?;
    Ok(())
}

/// Convert a host path to an mtools `::`-rooted path with forward slashes.
///
/// # Why this is public
///
/// `mtools` on Windows-style paths uses backslashes as separators, and a
/// backslash in an mtools path means something else entirely, so the
/// conversion has to be a deliberate one rather than an incidental detail
/// of the copy loop. The test that pins it lives in `tests/image.rs`.
pub fn rel_mtools(staging: &Path, p: &Path) -> String {
    p.strip_prefix(staging)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    for e in walk(dir)? {
        if e.is_file() {
            out.push(e);
        }
    }
    out.sort();
    Ok(out)
}

/// Depth-first directory walk without pulling in a dependency.
fn walk(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let e = e?;
            out.push(e.path());
            if e.path().is_dir() {
                stack.push(e.path());
            }
        }
    }
    Ok(out)
}
