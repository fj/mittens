//! Claude Code: hardcodes ~/.claude and ~/.claude.json, and reads the shared
//! agent config through mounts into its view of ~/.claude.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Ctx, HarnessSpec, UnsafeSupport};

pub struct Claude;

impl HarnessSpec for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn dot(&self) -> &'static str {
        ".claude"
    }

    // Deliberately avoids ~/.local/state/claude, which Claude Code's native
    // launcher already uses for its own lock files.
    fn state_leaf(&self) -> &'static str {
        "claude-home"
    }

    fn bin_env(&self) -> &'static str {
        "MITTENS_CLAUDE_BIN"
    }

    fn sync_file(&self) -> Option<&'static str> {
        Some(".claude.json")
    }

    fn default_bin(
        &self,
        home: &Path,
        _snap_root: &Path,
        _env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Option<PathBuf> {
        Some(home.join(".local/bin/claude"))
    }

    fn unsafe_support(&self) -> UnsafeSupport {
        UnsafeSupport::Via("CLAUDE_CONFIG_DIR")
    }

    fn prepare_unsafe(&self, ctx: &Ctx) -> Result<()> {
        // CLAUDE_CONFIG_DIR relocates every ~/.claude path — including the
        // top-level config, which claude then keeps at
        // dot-claude/.claude.json rather than at <state>/claude.json where
        // wrapped runs sync it. Seed it from the wrapped copy once; the two
        // evolve independently afterwards.
        let seed = ctx.dot_state().join(".claude.json");
        let wrapped = ctx.sync_state().expect("claude has a sync file");
        if !seed.exists() && wrapped.is_file() {
            fs::copy(&wrapped, &seed).with_context(|| format!("seeding {}", seed.display()))?;
        }
        Ok(())
    }

    fn extra_bwrap_args(&self, ctx: &Ctx) -> Result<Vec<OsString>> {
        shared_agent_mounts(ctx)
    }
}

/// Wire the shared, tool-agnostic agent config (XDG ~/.config/agents) into
/// what Claude sees as ~/.claude, so authored config — subagents, commands,
/// skills, hooks, output styles, and top-level memory — comes from one place
/// shared across agent tools rather than from the per-tool state dir.
/// Bind-mounted (not symlinked into dot-claude) so the wiring is explicit
/// here and survives a wiped state dir: delete ~/.claude, run mittens, and
/// these are reconstructed. Sources resolve host-side; mountpoints are
/// ensured on the dot-claude bind so bwrap has somewhere to mount. Each is
/// optional and mounted only if its source exists, so a name that does not
/// exist yet costs nothing and starts working the moment you create it under
/// the shared config. Machine-specific state (settings.json, plugins,
/// projects, sessions, credentials, caches, …) is deliberately NOT shared and
/// stays in dot-claude.
fn shared_agent_mounts(ctx: &Ctx) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = Vec::new();
    let dot_state = ctx.dot_state();
    for d in ["agents", "commands", "skills", "hooks", "output-styles"] {
        let src = ctx.agents_cfg.join(d);
        if !src.is_dir() {
            continue;
        }
        fs::create_dir_all(dot_state.join(d))
            .with_context(|| format!("creating mountpoint {}", dot_state.join(d).display()))?;
        args.push("--bind".into());
        args.push(src.into());
        args.push(ctx.home.join(".claude").join(d).into());
    }
    let agents_md = ctx.agents_cfg.join("AGENTS.md");
    if agents_md.is_file() {
        // CLAUDE.md is just AGENTS.md (the source of truth) under Claude's
        // name, so mount it read-only: memory edits go to AGENTS.md directly,
        // never diverge.
        let mountpoint = dot_state.join("CLAUDE.md");
        if !mountpoint.exists() {
            fs::write(&mountpoint, "")
                .with_context(|| format!("creating mountpoint {}", mountpoint.display()))?;
        }
        args.push("--ro-bind".into());
        args.push(agents_md.into());
        args.push(ctx.home.join(".claude/CLAUDE.md").into());
    }
    Ok(args)
}
