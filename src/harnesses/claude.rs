//! Claude Code: hardcodes ~/.claude and ~/.claude.json, and reads the shared
//! agent config through mounts into its view of ~/.claude.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Ctx, HarnessSpec, UnsafeSupport, agent_config};

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
        UnsafeSupport::Via {
            var: "CLAUDE_CONFIG_DIR",
            dir: None,
        }
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

    // Claude reads its global config from ~/.claude itself, and calls the
    // memory file CLAUDE.md.
    fn extra_bwrap_args(&self, ctx: &Ctx) -> Result<Vec<OsString>> {
        agent_config::mounts(
            ctx,
            "",
            &["agents", "commands", "skills", "hooks", "output-styles"],
            "CLAUDE.md",
        )
    }
}
