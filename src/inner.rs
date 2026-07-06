//! Hidden entry point re-exec'd inside the namespace: copies the sync file
//! onto the tmpfs (rename-safe, unlike a file bind-mount), runs the tool, and
//! syncs the file back out when the tool exits, however it exits. With no
//! sync file the tool is exec'd directly.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// argv: `<state> <bin> <sync-file-name or ""> -- <tool args...>`
pub fn run(argv: &[OsString]) -> ! {
    let usage = "mittens: __inner expects <state> <bin> <sync> -- <args...>";
    let [state, bin, sync, sep, tool_args @ ..] = argv else {
        eprintln!("{usage}");
        std::process::exit(2);
    };
    if sep != "--" {
        eprintln!("{usage}");
        std::process::exit(2);
    }
    let state = PathBuf::from(state);
    let sync = (!sync.is_empty()).then(|| sync.to_string_lossy().into_owned());
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is not set"));

    if sync.is_none() {
        let err = Command::new(bin).args(tool_args).exec();
        eprintln!("mittens: failed to exec {}: {err}", Path::new(bin).display());
        std::process::exit(127);
    }
    let sync = sync.unwrap();

    // <state>/claude.json -> ~/.claude.json (on the tmpfs)
    let state_copy = state.join(&sync[1..]);
    let home_copy = home.join(&sync);
    if let Err(err) = std::fs::copy(&state_copy, &home_copy) {
        eprintln!("mittens: copying {} in: {err}", state_copy.display());
    }

    // The tool owns terminal signals: ignore SIGINT/SIGQUIT while waiting
    // (dispositions are inherited across exec, so restore the default in the
    // child) and sync back out after however the tool exits.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
    let mut cmd = Command::new(bin);
    cmd.args(tool_args);
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    let status = cmd.status();

    sync_out(&state, &home, &sync);

    match status {
        Ok(status) => {
            std::process::exit(status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
        }
        Err(err) => {
            eprintln!("mittens: failed to run {}: {err}", Path::new(bin).display());
            std::process::exit(127);
        }
    }
}

/// Best-effort: ~/.claude.json -> <state>/claude.json, plus any
/// ~/.claude.json.* backups the tool left next to it (kept under their own
/// names; only the sync file itself drops the leading dot in the state dir).
fn sync_out(state: &Path, home: &Path, sync: &str) {
    let _ = std::fs::copy(home.join(sync), state.join(&sync[1..]));
    let Ok(entries) = std::fs::read_dir(home) else { return };
    let backup_prefix = format!("{sync}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.as_bytes().starts_with(backup_prefix.as_bytes()) {
            let _ = std::fs::copy(entry.path(), state.join(&name));
        }
    }
}
