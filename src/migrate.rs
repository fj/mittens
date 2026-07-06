//! --migrate: move the service's existing real-home data into its state
//! directory, once, so wrapped runs pick it up from there.

use std::fs;
use std::os::unix::ffi::OsStrExt;

use anyhow::{Context, Result, bail};

use crate::service::Ctx;
use crate::util::pgrep;

pub fn run(ctx: &Ctx) -> Result<()> {
    let svc = ctx.svc.name();
    if !pgrep(svc).is_empty() {
        bail!("{svc} processes are running; close all {svc} sessions before migrating");
    }
    fs::create_dir_all(&ctx.state).with_context(|| format!("creating {}", ctx.state.display()))?;

    let home_dot = ctx.home_dot();
    let dot_state = ctx.dot_state();
    // symlink_metadata, not is_dir: the guard trips on anything at ~/.<name>
    // (a dangling symlink included), so migrate must move anything too —
    // otherwise it reports success while the guard keeps advising --migrate.
    if home_dot.symlink_metadata().is_ok() {
        if dot_state.symlink_metadata().is_ok() {
            if fs::read_dir(&dot_state).map(|mut d| d.next().is_some()).unwrap_or(true) {
                bail!("refusing to migrate: {} already contains data", dot_state.display());
            }
            fs::remove_dir(&dot_state)?;
        }
        rename(&home_dot, &dot_state)?;
        println!("moved ~/{} -> {}", ctx.svc.dot(), dot_state.display());
    }

    if let (Some(sync), Some(sync_state)) = (ctx.svc.sync_file(), ctx.sync_state()) {
        let home_sync = ctx.home.join(sync);
        if home_sync.is_file() {
            rename(&home_sync, &sync_state)?;
            println!("moved ~/{sync} -> {}", sync_state.display());
        }
        let backup_prefix = format!("{sync}.");
        let mut backups: Vec<_> = fs::read_dir(&ctx.home)
            .with_context(|| format!("reading {}", ctx.home.display()))?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().as_bytes().starts_with(backup_prefix.as_bytes()))
            .map(|e| e.path())
            .collect();
        backups.sort();
        for backup in backups {
            let dest = ctx.state.join(backup.file_name().expect("backup has a file name"));
            rename(&backup, &dest)?;
            println!("moved {} -> {}/", backup.display(), ctx.state.display());
        }
    }

    println!("migration complete; run mittens normally from now on");
    Ok(())
}

fn rename(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    fs::rename(from, to).with_context(|| {
        format!(
            "moving {} -> {} (if they are on different filesystems, move it by hand)",
            from.display(),
            to.display()
        )
    })
}
