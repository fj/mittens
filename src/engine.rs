//! Harness-agnostic launch machinery: the startup guard, bwrap argument
//! assembly, and the exec into the namespace.

use std::convert::Infallible;
use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::harnesses::Ctx;
use crate::relocate;
use crate::util::{is_executable, name_starts_with, sorted_entry_names, tilde, which};
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
        Some(bin) => bail!(
            "{} binary not found at {}",
            ctx.harness.name(),
            bin.display()
        ),
        None => bail!("{} binary not found", ctx.harness.name()),
    };

    // Optionally wipe the real-home data first, so that the relocation below
    // finds nothing left to move.
    let skipped = matches!(mode, Mode::SkipPawmissions);
    if skipped {
        wipe::skip_pawmissions(ctx)?;
    }

    adopt(ctx)?;
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

    let bwrap = bwrap_args(ctx)?;

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
    cmd.args(&bwrap)
        .arg("--")
        .arg(exe)
        .args(crate::inner::argv(ctx, bin))
        .args(args);
    Err(cmd.exec()).context("exec bwrap")
}

/// Take over whatever the harness left in the real home before launching.
/// Those paths are shadowed and ignored inside the namespace, so anything in
/// them (fresh credentials, config edits from an unwrapped run) would silently
/// diverge from the state directory that wrapped sessions actually use.
/// Reported step by step: this moves the user's data without being asked, so
/// it has to say exactly what it did.
fn adopt(ctx: &Ctx) -> Result<()> {
    let stray = ctx.stray()?;
    if stray.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = stray
        .iter()
        .map(|(from, _)| tilde(from, &ctx.home))
        .collect();
    eprintln!(
        "mittens: found {} in the real home; moving into {}",
        names.join(" "),
        ctx.state.display()
    );
    relocate::run(&stray, &mut |step| {
        eprintln!("mittens:   {}", step.describe(&ctx.home))
    })?;
    eprintln!("mittens: starting {}…", ctx.harness.name());
    Ok(())
}

/// Build the namespace: tmpfs over $HOME, every real top-level entry bound
/// back, state dir mounted at the tool's dot-directory. Sources are resolved
/// on the host side, so they remain visible after the tmpfs covers $HOME.
/// Not pure: the wiring below materializes what it mounts (snap shims, ssh
/// config replicas, agent-config mountpoints) under `<state>` on the way.
pub fn bwrap_args(ctx: &Ctx) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = vec![
        "--dev-bind".into(),
        "/".into(),
        "/".into(),
        "--tmpfs".into(),
        ctx.home.clone().into(),
    ];

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
    // (only the current uid is mapped), which trips OpenSSH's ownership check
    // on the drop-ins the system-wide config includes and aborts every ssh run
    // ("Bad owner or permissions on …"); overmount each included file with a
    // replica owned by the invoking user (see src/ssh.rs).
    args.extend(crate::ssh::config_args(&ctx.state, &ctx.etc_ssh)?);

    // Snap-packaged tools invoked inside the namespace would hit the snap
    // dispatcher, whose snap-confine can never run in an unprivileged user
    // namespace; overmount /snap/bin with shims that exec classic snaps'
    // commands directly (see src/snap.rs).
    args.extend(crate::snap::shim_args(&ctx.state, &ctx.snap_root)?);

    args.extend(ctx.harness.extra_bwrap_args(ctx)?);
    Ok(args)
}

fn sorted_home_entries(home: &Path) -> Result<Vec<OsString>> {
    sorted_entry_names(home).with_context(|| format!("reading {}", home.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harnesses::{CLAUDE, Harness, OPENCODE, PI};
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
            // Nonexistent: the snap shim wiring stays out of these tests'
            // args (src/snap.rs has its own).
            snap_root: tmp.path().join("snap-root"),
            // Likewise for the ssh config replicas (src/ssh.rs has its own);
            // ssh_wiring_is_appended below opts back in.
            etc_ssh: tmp.path().join("etc-ssh"),
        };
        fs::create_dir_all(ctx.dot_state()).unwrap();
        (tmp, ctx)
    }

    fn strs(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn claude_args_shadow_dot_and_sync_file_and_wire_agents() {
        let (_tmp, ctx) = scratch_ctx(CLAUDE);
        // Stray-looking entries inside home must be skipped from the binds
        // (inside the namespace they are shadowed, not bound back).
        fs::create_dir_all(ctx.home.join(".claude")).unwrap();
        fs::write(ctx.home.join(".claude.json"), "{}").unwrap();
        fs::write(ctx.home.join(".claude.json.backup"), "{}").unwrap();
        // Shared agent config: every name claude is meant to take, plus one
        // it is not.
        for d in [
            "agents",
            "commands",
            "skills",
            "hooks",
            "output-styles",
            "plugins",
        ] {
            fs::create_dir_all(ctx.agents_cfg.join(d)).unwrap();
        }
        fs::write(ctx.agents_cfg.join("AGENTS.md"), "# memory\n").unwrap();

        let args = strs(&bwrap_args(&ctx).unwrap());
        let home = ctx.home.display().to_string();

        assert_eq!(args[..5], ["--dev-bind", "/", "/", "--tmpfs", &home][..]);
        assert!(args.contains(&format!("{home}/docs")));
        assert!(args.contains(&format!("{home}/.ssh")));
        assert!(!args.contains(&format!("{home}/.claude.json")));
        assert!(!args.contains(&format!("{home}/.claude.json.backup")));
        // ~/.claude appears only as the bind target of dot-claude and the
        // agent-config mountpoints, never as a --dev-bind source.
        let dot_bind = args
            .iter()
            .position(|a| a == &ctx.dot_state().display().to_string());
        assert!(
            dot_bind.is_some_and(
                |i| args[i - 1] == "--bind" && args[i + 1] == format!("{home}/.claude")
            )
        );
        for d in ["agents", "commands", "skills", "hooks", "output-styles"] {
            assert!(
                args.windows(3).any(|w| w[0] == "--bind"
                    && w[1] == ctx.agents_cfg.join(d).display().to_string()
                    && w[2] == format!("{home}/.claude/{d}")),
                "{d} is not wired"
            );
        }
        assert!(args.windows(3).any(|w| w[0] == "--ro-bind"
            && w[1] == ctx.agents_cfg.join("AGENTS.md").display().to_string()
            && w[2] == format!("{home}/.claude/CLAUDE.md")));
        // Names outside claude's list stay in the shared config.
        assert!(!args.iter().any(|a| a.ends_with("/.claude/plugins")));
        // Empty CLAUDE.md mountpoint was created on the dot-claude bind.
        assert!(ctx.dot_state().join("CLAUDE.md").is_file());
    }

    #[test]
    fn opencode_args_have_no_sync_or_agent_wiring() {
        let (_tmp, ctx) = scratch_ctx(OPENCODE);
        fs::create_dir_all(ctx.agents_cfg.join("agents")).unwrap();

        let args = strs(&bwrap_args(&ctx).unwrap());
        let home = ctx.home.display().to_string();

        assert!(args.windows(3).any(|w| w[0] == "--bind"
            && w[1] == ctx.dot_state().display().to_string()
            && w[2] == format!("{home}/.opencode")));
        assert!(
            !args
                .iter()
                .any(|a| a.contains("CLAUDE.md") || a.contains("/agents"))
        );
        // Nothing sets environment for a plain opencode binary.
        assert!(!args.contains(&"--setenv".to_string()));
    }

    #[test]
    fn pi_args_wire_the_agent_config_one_level_into_the_dot_dir() {
        let (_tmp, ctx) = scratch_ctx(PI);
        for d in ["skills", "prompts", "themes", "tools", "output-styles"] {
            fs::create_dir_all(ctx.agents_cfg.join(d)).unwrap();
        }
        fs::write(ctx.agents_cfg.join("AGENTS.md"), "# memory\n").unwrap();

        let args = strs(&bwrap_args(&ctx).unwrap());
        let home = ctx.home.display().to_string();

        assert!(args.windows(3).any(|w| w[0] == "--bind"
            && w[1] == ctx.dot_state().display().to_string()
            && w[2] == format!("{home}/.pi")));
        // pi's global config is ~/.pi/agent, not ~/.pi itself, and it reads
        // the memory file under the shared config's own name.
        for d in ["skills", "prompts", "themes", "tools"] {
            assert!(
                args.windows(3).any(|w| w[0] == "--bind"
                    && w[1] == ctx.agents_cfg.join(d).display().to_string()
                    && w[2] == format!("{home}/.pi/agent/{d}")),
                "{d} is not wired"
            );
            assert!(ctx.dot_state().join("agent").join(d).is_dir());
        }
        assert!(args.windows(3).any(|w| w[0] == "--ro-bind"
            && w[1] == ctx.agents_cfg.join("AGENTS.md").display().to_string()
            && w[2] == format!("{home}/.pi/agent/AGENTS.md")));
        // Mountpoints were materialized below dot-pi, which bwrap needs even
        // though pi has never run and created the agent directory itself.
        assert!(ctx.dot_state().join("agent/AGENTS.md").is_file());
        // claude-only names stay in the shared config.
        assert!(!args.iter().any(|a| a.ends_with("/.pi/agent/output-styles")));
    }

    #[test]
    fn opencode_snap_binary_disables_autoupdate() {
        let (_tmp, mut ctx) = scratch_ctx(OPENCODE);
        // A non-snap binary gets no extra environment.
        let args = strs(&bwrap_args(&ctx).unwrap());
        assert!(!args.contains(&"OPENCODE_DISABLE_AUTOUPDATE".to_string()));

        // The snap's squashfs is read-only, so self-update can never work;
        // the setenv the bypassed snap wrapper would have set is preserved.
        ctx.bin = Some(PathBuf::from("/snap/opencode/current/bin/opencode"));
        let args = strs(&bwrap_args(&ctx).unwrap());
        assert!(
            args.windows(3).any(|w| w[0] == "--setenv"
                && w[1] == "OPENCODE_DISABLE_AUTOUPDATE"
                && w[2] == "1")
        );
    }

    #[test]
    fn ssh_wiring_is_appended() {
        let (_tmp, ctx) = scratch_ctx(CLAUDE);
        // No system-wide ssh config: nothing to replicate, nothing appended.
        let args = strs(&bwrap_args(&ctx).unwrap());
        assert!(!args.iter().any(|a| a.contains("ssh-config")));

        // A drop-in the system config includes is overmounted with its
        // replica; ssh perm-checks the original and would abort on its
        // nobody:nogroup owner inside the namespace.
        let drop_in = ctx.etc_ssh.join("ssh_config.d/20-proxy.conf");
        fs::create_dir_all(drop_in.parent().unwrap()).unwrap();
        fs::write(
            ctx.etc_ssh.join("ssh_config"),
            "Include ssh_config.d/*.conf\n",
        )
        .unwrap();
        fs::write(&drop_in, "Host .host\n").unwrap();

        let args = strs(&bwrap_args(&ctx).unwrap());
        let replica = ctx
            .state
            .join("ssh-config")
            .join(drop_in.strip_prefix("/").unwrap());
        assert!(
            args.windows(3).any(|w| w[0] == "--ro-bind"
                && w[1] == replica.display().to_string()
                && w[2] == drop_in.display().to_string()),
            "{args:?}"
        );
    }
}
