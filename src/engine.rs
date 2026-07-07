//! Harness-agnostic launch machinery: the startup guard, bwrap argument
//! assembly, and the exec into the namespace.

use std::convert::Infallible;
use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::harness::Ctx;
use crate::util::{entries_with_prefix, is_executable, name_starts_with, shell_quote, which};
use crate::wipe;

pub enum Mode {
    Wrapped,
    Unsafe,
    SkipPawmissions,
}

pub fn launch(ctx: &Ctx, mode: Mode, args: &[OsString]) -> Result<Infallible> {
    let env = |k: &str| std::env::var_os(k);
    if !matches!(mode, Mode::Unsafe) && which("bwrap", &env).is_none() {
        bail!("bwrap (bubblewrap) is not installed");
    }
    let bin = match ctx.bin.as_deref() {
        Some(bin) if is_executable(bin) => bin,
        Some(bin) => bail!("{} binary not found at {}", ctx.harness.name(), bin.display()),
        None => bail!("{} binary not found", ctx.harness.name()),
    };

    // Optionally wipe the real-home data before the guard runs, so that the
    // guard below finds nothing and lets the tool start.
    let skipped = matches!(mode, Mode::SkipPawmissions);
    if skipped {
        wipe::skip_pawmissions(ctx)?;
    }

    guard(ctx)?;
    fs::create_dir_all(ctx.dot_state())
        .with_context(|| format!("creating {}", ctx.dot_state().display()))?;

    if matches!(mode, Mode::Unsafe) {
        return ctx.harness.unsafe_exec(ctx, bin, args);
    }

    if let Some(sync_state) = ctx.sync_state()
        && !sync_state.exists()
    {
        fs::write(&sync_state, "{}\n")
            .with_context(|| format!("creating {}", sync_state.display()))?;
    }

    let bwrap = bwrap_args(ctx, env("GIT_SSH_COMMAND").is_some())?;

    // After a --dangerously-skip-pawmissions wipe, pause briefly before
    // launching.
    if skipped {
        eprintln!("mittens: starting {} in 3s…", ctx.harness.name());
        crate::util::pause(3);
    }

    // Re-exec this binary inside the namespace as the hidden __inner
    // entry point, which copy-syncs the config file (rename-safe, unlike a
    // file bind-mount) around the tool run.
    let exe = std::env::current_exe().context("resolving own executable")?;
    let mut cmd = Command::new("bwrap");
    cmd.args(&bwrap).arg("--").arg(exe).args(crate::inner::argv(ctx, bin)).args(args);
    Err(cmd.exec()).context("exec bwrap")
}

/// Hard guard: refuse to run while any of the harness's data exists in the
/// real home. It would be shadowed and ignored inside the namespace, so
/// anything in it (fresh credentials, config edits from an unwrapped run)
/// would silently diverge from the state directory that wrapped sessions
/// actually use.
fn guard(ctx: &Ctx) -> Result<()> {
    let stray = stray_paths(ctx)?;
    if stray.is_empty() {
        return Ok(());
    }
    let harness = ctx.harness.name();
    eprintln!("mittens: refusing to start: found {} in the real home", stray.join(" "));
    let dot_state = ctx.dot_state();
    let populated = fs::read_dir(&dot_state).map(|mut d| d.next().is_some()).unwrap_or(false);
    if populated {
        eprintln!(
            "mittens: the state directory ({}) is already populated, so this is",
            ctx.state.display()
        );
        eprintln!(
            "mittens: probably leftover from an unwrapped {harness} run; inspect it and either"
        );
        eprintln!("mittens: delete it or reconcile it with the state directory by hand");
    } else {
        eprintln!(
            "mittens: close all {harness} sessions and run: mittens harness:{harness} --migrate"
        );
    }
    std::process::exit(1);
}

/// The harness's real-home paths that exist right now, as ~/-relative strings.
fn stray_paths(ctx: &Ctx) -> Result<Vec<String>> {
    let mut stray = Vec::new();
    if ctx.home_dot().symlink_metadata().is_ok() {
        stray.push(format!("~/{}", ctx.harness.dot()));
    }
    if let Some(sync) = ctx.harness.sync_file() {
        for path in entries_with_prefix(&ctx.home, sync)
            .with_context(|| format!("reading {}", ctx.home.display()))?
        {
            if let Some(name) = path.file_name() {
                stray.push(format!("~/{}", name.to_string_lossy()));
            }
        }
    }
    Ok(stray)
}

/// Build the namespace: tmpfs over $HOME, every real top-level entry bound
/// back, state dir mounted at the tool's dot-directory. Sources are resolved
/// on the host side, so they remain visible after the tmpfs covers $HOME.
pub fn bwrap_args(ctx: &Ctx, git_ssh_command_set: bool) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> =
        vec!["--dev-bind".into(), "/".into(), "/".into(), "--tmpfs".into(), ctx.home.clone().into()];

    let sync = ctx.harness.sync_file();
    for name in sorted_home_entries(&ctx.home)? {
        if name.as_os_str() == ctx.harness.dot() {
            continue;
        }
        if let Some(sync) = sync
            && name_starts_with(&name, sync)
        {
            continue;
        }
        let path = ctx.home.join(&name);
        args.push("--dev-bind".into());
        args.push(path.clone().into());
        args.push(path.into());
    }
    args.push("--bind".into());
    args.push(ctx.dot_state().into());
    args.push(ctx.home_dot().into());

    // Root-owned files appear as nobody:nogroup inside the user namespace
    // (only the current uid is mapped), which trips OpenSSH's config
    // ownership check on the /etc/ssh/ssh_config.d drop-ins and aborts every
    // ssh run ("Bad owner or permissions on …"). Point git's ssh transport at
    // the user config alone (-F skips the system-wide config; /dev/null if
    // there is no user config) so git keeps working. Deliberately narrow: a
    // preexisting GIT_SSH_COMMAND wins, and non-git ssh is left alone rather
    // than papered over sandbox-wide.
    if !git_ssh_command_set {
        let ssh_cfg = ctx.home.join(".ssh/config");
        let ssh_cfg = if ssh_cfg.is_file() { ssh_cfg } else { "/dev/null".into() };
        args.push("--setenv".into());
        args.push("GIT_SSH_COMMAND".into());
        args.push(format!("ssh -F {}", shell_quote(&ssh_cfg)).into());
    }

    args.extend(ctx.harness.extra_bwrap_args(ctx)?);
    Ok(args)
}

fn sorted_home_entries(home: &Path) -> Result<Vec<OsString>> {
    let mut names: Vec<OsString> = fs::read_dir(home)
        .with_context(|| format!("reading {}", home.display()))?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|e| e.file_name())
        .collect();
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Harness;
    use std::path::PathBuf;

    fn scratch_ctx(harness: Harness) -> (tempfile::TempDir, Ctx) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join("docs")).unwrap();
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(home.join(".ssh/config"), "Host *\n").unwrap();
        let ctx = Ctx {
            harness,
            home: home.clone(),
            bin: Some(PathBuf::from("/bin/true")),
            state: tmp.path().join("state"),
            agents_cfg: tmp.path().join("agents"),
        };
        fs::create_dir_all(ctx.dot_state()).unwrap();
        (tmp, ctx)
    }

    fn strs(args: &[OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn claude_args_shadow_dot_and_sync_file_and_wire_agents() {
        let (_tmp, ctx) = scratch_ctx(Harness::Claude);
        // Stray-looking entries inside home must be skipped from the binds
        // (inside the namespace they are shadowed, not bound back).
        fs::create_dir_all(ctx.home.join(".claude")).unwrap();
        fs::write(ctx.home.join(".claude.json"), "{}").unwrap();
        fs::write(ctx.home.join(".claude.json.backup"), "{}").unwrap();
        // Shared agent config with two of the optional sources present.
        fs::create_dir_all(ctx.agents_cfg.join("agents")).unwrap();
        fs::write(ctx.agents_cfg.join("AGENTS.md"), "# memory\n").unwrap();

        let args = strs(&bwrap_args(&ctx, false).unwrap());
        let home = ctx.home.display().to_string();

        assert_eq!(args[..5], ["--dev-bind", "/", "/", "--tmpfs", &home][..]);
        assert!(args.contains(&format!("{home}/docs")));
        assert!(args.contains(&format!("{home}/.ssh")));
        assert!(!args.contains(&format!("{home}/.claude.json")));
        assert!(!args.contains(&format!("{home}/.claude.json.backup")));
        // ~/.claude appears only as the bind target of dot-claude and the
        // agent-config mountpoints, never as a --dev-bind source.
        let dot_bind = args.iter().position(|a| a == &ctx.dot_state().display().to_string());
        assert!(dot_bind.is_some_and(|i| args[i - 1] == "--bind" && args[i + 1] == format!("{home}/.claude")));
        assert!(args.windows(3).any(|w| w[0] == "--bind"
            && w[1] == ctx.agents_cfg.join("agents").display().to_string()
            && w[2] == format!("{home}/.claude/agents")));
        assert!(args.windows(3).any(|w| w[0] == "--ro-bind"
            && w[1] == ctx.agents_cfg.join("AGENTS.md").display().to_string()
            && w[2] == format!("{home}/.claude/CLAUDE.md")));
        // Absent optional sources are not wired.
        assert!(!args.iter().any(|a| a.ends_with("/.claude/skills")));
        // Empty CLAUDE.md mountpoint was created on the dot-claude bind.
        assert!(ctx.dot_state().join("CLAUDE.md").is_file());
        assert!(args.iter().any(|a| a.starts_with("ssh -F ")));
    }

    #[test]
    fn opencode_args_have_no_sync_or_agent_wiring() {
        let (_tmp, ctx) = scratch_ctx(Harness::Opencode);
        fs::create_dir_all(ctx.agents_cfg.join("agents")).unwrap();

        let args = strs(&bwrap_args(&ctx, true).unwrap());
        let home = ctx.home.display().to_string();

        assert!(args.windows(3).any(|w| w[0] == "--bind"
            && w[1] == ctx.dot_state().display().to_string()
            && w[2] == format!("{home}/.opencode")));
        assert!(!args.iter().any(|a| a.contains("CLAUDE.md") || a.contains("/agents")));
        // Preexisting GIT_SSH_COMMAND wins: no --setenv.
        assert!(!args.contains(&"--setenv".to_string()));
    }

    #[test]
    fn opencode_snap_binary_disables_autoupdate() {
        let (_tmp, mut ctx) = scratch_ctx(Harness::Opencode);
        // A non-snap binary gets no extra environment.
        let args = strs(&bwrap_args(&ctx, true).unwrap());
        assert!(!args.contains(&"OPENCODE_DISABLE_AUTOUPDATE".to_string()));

        // The snap's squashfs is read-only, so self-update can never work;
        // the setenv the bypassed snap wrapper would have set is preserved.
        ctx.bin = Some(PathBuf::from("/snap/opencode/current/bin/opencode"));
        let args = strs(&bwrap_args(&ctx, true).unwrap());
        assert!(args.windows(3).any(|w| w[0] == "--setenv"
            && w[1] == "OPENCODE_DISABLE_AUTOUPDATE"
            && w[2] == "1"));
    }

    #[test]
    fn missing_user_ssh_config_falls_back_to_dev_null() {
        let (_tmp, ctx) = scratch_ctx(Harness::Claude);
        fs::remove_file(ctx.home.join(".ssh/config")).unwrap();
        let args = strs(&bwrap_args(&ctx, false).unwrap());
        assert!(args.contains(&"ssh -F '/dev/null'".to_string()));
    }

    #[test]
    fn stray_detection_matches_guard_globs() {
        let (_tmp, ctx) = scratch_ctx(Harness::Claude);
        assert!(stray_paths(&ctx).unwrap().is_empty());
        fs::write(ctx.home.join(".claude.json.corrupt.bak"), "{}").unwrap();
        assert_eq!(stray_paths(&ctx).unwrap(), vec!["~/.claude.json.corrupt.bak"]);
        fs::create_dir_all(ctx.home.join(".claude")).unwrap();
        assert_eq!(stray_paths(&ctx).unwrap(), vec!["~/.claude", "~/.claude.json.corrupt.bak"]);

        let (_tmp, octx) = scratch_ctx(Harness::Opencode);
        fs::write(octx.home.join(".opencode-lookalike"), "").unwrap();
        // Only the exact dot-dir counts for harnesses without a sync file.
        assert!(stray_paths(&octx).unwrap().is_empty());
        fs::create_dir_all(octx.home.join(".opencode")).unwrap();
        assert_eq!(stray_paths(&octx).unwrap(), vec!["~/.opencode"]);
    }
}
