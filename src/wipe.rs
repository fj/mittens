//! --dangerously-skip-pawmissions: verbosely inspect the real-home data that
//! the startup guard trips on, count down for 10s (abortable with Ctrl-C),
//! then delete it so the guard passes and the tool can launch. Purely
//! destructive; the caller pauses again before actually starting the tool.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::harnesses::Ctx;
use crate::util::{entries_with_prefix, human_size, pause, pgrep};

pub fn skip_pawmissions(ctx: &Ctx) -> Result<()> {
    let harness = ctx.harness.name();
    let targets = targets(ctx)?;

    eprintln!("mittens: --dangerously-skip-pawmissions");
    if targets.is_empty() {
        eprintln!("mittens: no real-home {harness} data ({}); nothing to wipe", ctx.guard_names());
        return Ok(());
    }

    eprintln!("mittens: inspecting real-home {harness} data slated for deletion:");
    for t in &targets {
        inspect(t);
    }

    // Indicate any running processes of the tool. Wrapped sessions read the
    // state dir, not the real home, but an unwrapped run may be writing to
    // the very files about to be deleted. Informational only — this is the
    // dangerous path.
    let pids = pgrep(harness);
    if !pids.is_empty() {
        eprintln!();
        eprintln!("mittens: WARNING — {harness} appears to be running ({}):", pids.join(", "));
        if let Ok(out) =
            Command::new("ps").args(["-o", "pid=,etime=,args=", "-p", &pids.join(",")]).output()
        {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                eprintln!("  {line}");
            }
        }
        eprintln!("mittens: if any of these is an UNWRAPPED run, it may be writing to the");
        eprintln!("mittens: files below right now. Ctrl-C during the countdown to abort.");
    }

    eprintln!();
    eprintln!("mittens: the above will be PERMANENTLY DELETED (rm -rf). Ctrl-C to abort.");
    countdown();

    for t in &targets {
        let removed = if t.is_dir() && !t.is_symlink() {
            fs::remove_dir_all(t)
        } else {
            fs::remove_file(t)
        };
        removed.with_context(|| format!("wiping {}", t.display()))?;
        eprintln!("mittens: wiped {}", t.display());
    }
    Ok(())
}

fn targets(ctx: &Ctx) -> Result<Vec<PathBuf>> {
    let mut targets = Vec::new();
    if ctx.home_dot().symlink_metadata().is_ok() {
        targets.push(ctx.home_dot());
    }
    if let Some(sync) = ctx.harness.sync_file() {
        targets.extend(
            entries_with_prefix(&ctx.home, sync)
                .with_context(|| format!("reading {}", ctx.home.display()))?,
        );
    }
    Ok(targets)
}

fn inspect(path: &Path) {
    if path.is_dir() {
        let (entries, bytes) = dir_stats(path);
        eprintln!("  {}  (dir, {}, {entries} entries)", path.display(), human_size(bytes));
        if let Ok(dir) = fs::read_dir(path) {
            let mut names: Vec<_> =
                dir.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            for name in names {
                eprintln!("        {name}");
            }
        }
    } else {
        let size = path.metadata().map(|m| m.len()).unwrap_or(0);
        eprintln!("  {}  (file, {})", path.display(), human_size(size));
    }
}

/// Recursive entry count and byte total, like `find | wc -l` + `du -s`.
fn dir_stats(path: &Path) -> (u64, u64) {
    let (mut entries, mut bytes) = (0, 0);
    let Ok(dir) = fs::read_dir(path) else { return (entries, bytes) };
    for entry in dir.filter_map(|e| e.ok()) {
        entries += 1;
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            let (sub_entries, sub_bytes) = dir_stats(&entry.path());
            entries += sub_entries;
            bytes += sub_bytes;
        } else {
            bytes += meta.len();
        }
    }
    (entries, bytes)
}

/// Ten seconds of red progress bar on stderr; Ctrl-C (default SIGINT
/// disposition) is the abort.
fn countdown() {
    const WIDTH: usize = 40;
    const TOTAL: usize = 10;
    for i in 0..TOTAL {
        let filled = i * WIDTH / TOTAL;
        eprint!(
            "\r  \x1b[1;31m[{}{}]\x1b[0m  deleting in {:2}s ",
            "#".repeat(filled),
            ".".repeat(WIDTH - filled),
            TOTAL - i
        );
        let _ = std::io::stderr().flush();
        pause(1);
    }
    eprintln!("\r  \x1b[1;31m[{}]\x1b[0m  deleting now      ", "#".repeat(WIDTH));
}
