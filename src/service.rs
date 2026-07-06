//! Service handlers: everything tool-specific lives here.
//!
//! Each service defines the endpoints mittens must know about — the binary,
//! which real-home path the tool hardcodes, whether a top-level config file
//! needs copy-sync, whether an unsandboxed fallback exists, and what extra
//! mounts to wire. Everything outside this module is service-agnostic.

use std::convert::Infallible;
use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::util::which;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Service {
    Claude,
    Opencode,
}

impl Service {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "opencode" => Some(Self::Opencode),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
        }
    }

    /// Top-level home entry the tool hardcodes; shadowed to `<state>/dot-<name>`.
    pub fn dot(self) -> &'static str {
        match self {
            Self::Claude => ".claude",
            Self::Opencode => ".opencode",
        }
    }

    /// Top-level config file the tool rewrites via atomic rename, copy-synced
    /// through the tmpfs (stored host-side as `<state>/<name minus leading
    /// dot>`); None if the tool has no such file.
    pub fn sync_file(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some(".claude.json"),
            Self::Opencode => None,
        }
    }

    fn state_leaf(self) -> &'static str {
        match self {
            // Deliberately avoids ~/.local/state/claude, which Claude Code's
            // native launcher already uses for its own lock files.
            Self::Claude => "claude-home",
            Self::Opencode => "opencode-home",
        }
    }

    fn bin_env(self) -> &'static str {
        match self {
            Self::Claude => "MITTENS_CLAUDE_BIN",
            Self::Opencode => "MITTENS_OPENCODE_BIN",
        }
    }

    fn default_bin(self, home: &Path, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
        match self {
            Self::Claude => Some(home.join(".local/bin/claude")),
            Self::Opencode => which("opencode", env),
        }
    }

    /// --unsafe: exec the tool outside the sandbox with a relocation
    /// environment variable, or refuse when the tool has no trustworthy one.
    pub fn unsafe_exec(self, ctx: &Ctx, bin: &Path, args: &[OsString]) -> Result<Infallible> {
        match self {
            Self::Claude => {
                // CLAUDE_CONFIG_DIR relocates every ~/.claude path — including
                // the top-level config, which claude then keeps at
                // dot-claude/.claude.json rather than at <state>/claude.json
                // where wrapped runs sync it. Seed it from the wrapped copy
                // once; the two evolve independently afterwards.
                let seed = ctx.dot_state().join(".claude.json");
                let wrapped = ctx.sync_state().expect("claude has a sync file");
                if !seed.exists() && wrapped.is_file() {
                    fs::copy(&wrapped, &seed)
                        .with_context(|| format!("seeding {}", seed.display()))?;
                }
                let err = Command::new(bin)
                    .args(args)
                    .env("CLAUDE_CONFIG_DIR", ctx.dot_state())
                    .exec();
                Err(err).with_context(|| format!("exec {}", bin.display()))
            }
            Self::Opencode => bail!(
                "--unsafe is not supported for opencode: OPENCODE_CONFIG_DIR only \
                 relocates config loading, nothing relocates ~/.opencode itself"
            ),
        }
    }

    /// Service-specific bwrap mounts, appended after the generic ones.
    pub fn extra_mounts(self, ctx: &Ctx) -> Result<Vec<OsString>> {
        match self {
            Self::Claude => claude_shared_agent_mounts(ctx),
            // opencode's global config is the real, XDG-proper
            // ~/.config/opencode — already visible inside the namespace.
            // Share pieces of ~/.config/agents into it with plain symlinks
            // instead; no mount wiring needed.
            Self::Opencode => Ok(Vec::new()),
        }
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
fn claude_shared_agent_mounts(ctx: &Ctx) -> Result<Vec<OsString>> {
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

/// Everything resolved once from the environment for the selected service.
pub struct Ctx {
    pub svc: Service,
    pub home: PathBuf,
    /// The tool's executable; None if it could not be found.
    pub bin: Option<PathBuf>,
    pub state: PathBuf,
    pub agents_cfg: PathBuf,
}

impl Ctx {
    pub fn resolve(svc: Service) -> Self {
        Self::resolve_with(svc, &|k| std::env::var_os(k))
    }

    pub fn resolve_with(svc: Service, env: &dyn Fn(&str) -> Option<OsString>) -> Self {
        let home = PathBuf::from(env("HOME").expect("HOME is not set"));
        let state = env("MITTENS_STATE_DIR").map(PathBuf::from).unwrap_or_else(|| {
            env("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/state"))
                .join(svc.state_leaf())
        });
        let agents_cfg = env("MITTENS_AGENTS_DIR").map(PathBuf::from).unwrap_or_else(|| {
            env("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("agents")
        });
        let bin = env(svc.bin_env())
            .map(PathBuf::from)
            .or_else(|| svc.default_bin(&home, env));
        Ctx { svc, home, bin, state, agents_cfg }
    }

    /// `<state>/dot-<name>` — mounted over `~/.<name>` inside the namespace.
    pub fn dot_state(&self) -> PathBuf {
        self.state.join(format!("dot-{}", &self.svc.dot()[1..]))
    }

    /// Host-side home of the copy-synced config file (`<state>/claude.json`).
    pub fn sync_state(&self) -> Option<PathBuf> {
        self.svc.sync_file().map(|f| self.state.join(&f[1..]))
    }

    /// The real-home path the tool hardcodes (`~/.claude`, `~/.opencode`).
    pub fn home_dot(&self) -> PathBuf {
        self.home.join(self.svc.dot())
    }

    /// Human-readable list of the real-home paths the startup guard checks.
    pub fn guard_names(&self) -> String {
        match self.svc.sync_file() {
            Some(sync) => format!("~/{} or ~/{}*", self.svc.dot(), sync),
            None => format!("~/{}", self.svc.dot()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| {
            pairs
                .iter()
                .find(|(pk, _)| pk == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn resolves_defaults_per_service() {
        let env = env_from(&[("HOME", "/h")]);
        let c = Ctx::resolve_with(Service::Claude, &env);
        assert_eq!(c.state, PathBuf::from("/h/.local/state/claude-home"));
        assert_eq!(c.dot_state(), PathBuf::from("/h/.local/state/claude-home/dot-claude"));
        assert_eq!(c.sync_state(), Some(PathBuf::from("/h/.local/state/claude-home/claude.json")));
        assert_eq!(c.bin, Some(PathBuf::from("/h/.local/bin/claude")));
        assert_eq!(c.agents_cfg, PathBuf::from("/h/.config/agents"));

        let o = Ctx::resolve_with(Service::Opencode, &env);
        assert_eq!(o.state, PathBuf::from("/h/.local/state/opencode-home"));
        assert_eq!(o.dot_state(), PathBuf::from("/h/.local/state/opencode-home/dot-opencode"));
        assert_eq!(o.sync_state(), None);
        assert_eq!(o.bin, None); // no opencode on the empty PATH
    }

    #[test]
    fn env_overrides_win() {
        let env = env_from(&[
            ("HOME", "/h"),
            ("MITTENS_STATE_DIR", "/elsewhere"),
            ("MITTENS_CLAUDE_BIN", "/opt/claude"),
            ("MITTENS_AGENTS_DIR", "/cfg/agents"),
            ("XDG_STATE_HOME", "/xdg-state"),
        ]);
        let c = Ctx::resolve_with(Service::Claude, &env);
        assert_eq!(c.state, PathBuf::from("/elsewhere"));
        assert_eq!(c.bin, Some(PathBuf::from("/opt/claude")));
        assert_eq!(c.agents_cfg, PathBuf::from("/cfg/agents"));

        let env = env_from(&[("HOME", "/h"), ("XDG_STATE_HOME", "/xdg-state")]);
        let c = Ctx::resolve_with(Service::Claude, &env);
        assert_eq!(c.state, PathBuf::from("/xdg-state/claude-home"));
    }

    #[test]
    fn service_names_round_trip() {
        for svc in [Service::Claude, Service::Opencode] {
            assert_eq!(Service::from_name(svc.name()), Some(svc));
        }
        assert_eq!(Service::from_name("emacs"), None);
    }
}
