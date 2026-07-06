//! Hidden entry point re-exec'd inside the namespace: copies the sync file
//! onto the tmpfs (rename-safe, unlike a file bind-mount), runs the tool, and
//! syncs the file back out when the tool exits, however it exits. With no
//! sync file the tool is exec'd directly.
//!
//! The argv protocol between the outer process and the re-exec is built
//! (argv()) and parsed (run()) only in this module, so the two sides cannot
//! drift:
//!
//!   __inner <state> <bin> <home sync name|""> <state sync name|""> -- <tool args...>

use std::ffi::OsString;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::service::Ctx;
use crate::util::entries_with_prefix;

pub const SUBCOMMAND: &str = "__inner";

/// The argv tail the outer process appends after `bwrap ... -- <own exe>`.
/// The state-side sync file name is resolved here from Ctx so the "leading
/// dot dropped in the state dir" convention lives only in Ctx::sync_state.
pub fn argv(ctx: &Ctx, bin: &Path) -> Vec<OsString> {
    let state_name: OsString = ctx
        .sync_state()
        .and_then(|p| p.file_name().map(|n| n.to_os_string()))
        .unwrap_or_default();
    vec![
        SUBCOMMAND.into(),
        ctx.state.clone().into(),
        bin.into(),
        ctx.svc.sync_file().unwrap_or("").into(),
        state_name,
        "--".into(),
    ]
}

/// Entry point; `argv` is everything after the SUBCOMMAND token.
pub fn run(argv: &[OsString]) -> ! {
    let usage = "mittens: __inner expects <state> <bin> <sync-home> <sync-state> -- <args...>";
    let [state, bin, sync_home, sync_state, sep, tool_args @ ..] = argv else {
        eprintln!("{usage}");
        std::process::exit(2);
    };
    if sep != "--" || sync_home.is_empty() != sync_state.is_empty() {
        eprintln!("{usage}");
        std::process::exit(2);
    }
    let state = PathBuf::from(state);
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is not set"));

    if sync_home.is_empty() {
        let err = Command::new(bin).args(tool_args).exec();
        eprintln!("mittens: failed to exec {}: {err}", Path::new(bin).display());
        std::process::exit(127);
    }
    let sync_home = sync_home.to_string_lossy().into_owned();

    // <state>/claude.json -> ~/.claude.json (on the tmpfs)
    let state_copy = state.join(sync_state);
    let home_copy = home.join(&sync_home);
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

    sync_out(&state, &state_copy, &home, &home_copy, &sync_home);

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
fn sync_out(state: &Path, state_copy: &Path, home: &Path, home_copy: &Path, sync_home: &str) {
    let _ = std::fs::copy(home_copy, state_copy);
    let Ok(backups) = entries_with_prefix(home, &format!("{sync_home}.")) else { return };
    for backup in backups {
        if let Some(name) = backup.file_name() {
            let _ = std::fs::copy(&backup, state.join(name));
        }
    }
}
