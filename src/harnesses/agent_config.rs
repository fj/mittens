//! The shared, tool-agnostic agent config (XDG ~/.config/agents), wired into
//! whichever directory a harness reads its global config from.

use std::ffi::OsString;
use std::fs;

use anyhow::{Context, Result};

use super::Ctx;

/// The memory file in the shared config. Tools spell its name differently, so
/// each harness says what to mount it as.
const MEMORY: &str = "AGENTS.md";

/// Wire `dirs` and the memory file into what the tool sees as its global
/// config directory, `~/<dot>/<root>` — `root` is empty for claude
/// (~/.claude) and "agent" for pi (~/.pi/agent).
///
/// Authored config (subagents, commands, skills, prompt templates, hooks,
/// themes, …) then comes from one place shared across agent tools rather than
/// from each per-tool state dir. Machine-specific state (settings, plugins,
/// projects, sessions, credentials, caches, …) is deliberately NOT shared and
/// stays in the state dir.
///
/// Bind-mounted, not symlinked into the state dir, so the wiring is explicit
/// here and survives a wiped state dir: delete the dot dir, run mittens, and
/// these are reconstructed. Sources resolve host-side; mountpoints are ensured
/// under the state dir so bwrap has somewhere to mount. Each is optional and
/// mounted only if its source exists, so a name that does not exist yet costs
/// nothing and starts working the moment you create it under the shared
/// config. The memory file is mounted read-only: it is the same file under the
/// tool's own name, so edits go to AGENTS.md directly and never diverge.
pub fn mounts(ctx: &Ctx, root: &str, dirs: &[&str], memory_as: &str) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = Vec::new();
    let mountpoints = ctx.dot_state().join(root);
    let view = ctx.home_dot().join(root);

    for name in dirs {
        let src = ctx.agents_cfg.join(name);
        if !src.is_dir() {
            continue;
        }
        let mountpoint = mountpoints.join(name);
        fs::create_dir_all(&mountpoint)
            .with_context(|| format!("creating mountpoint {}", mountpoint.display()))?;
        args.push("--bind".into());
        args.push(src.into());
        args.push(view.join(name).into());
    }

    let memory = ctx.agents_cfg.join(MEMORY);
    if memory.is_file() {
        let mountpoint = mountpoints.join(memory_as);
        if !mountpoint.exists() {
            fs::create_dir_all(&mountpoints)
                .with_context(|| format!("creating {}", mountpoints.display()))?;
            fs::write(&mountpoint, "")
                .with_context(|| format!("creating mountpoint {}", mountpoint.display()))?;
        }
        args.push("--ro-bind".into());
        args.push(memory.into());
        args.push(view.join(memory_as).into());
    }
    Ok(args)
}
