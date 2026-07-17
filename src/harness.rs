//! Harness handlers: everything tool-specific lives here.
//!
//! Each harness defines the endpoints mittens must know about — the binary,
//! which real-home path the tool hardcodes, whether a top-level config file
//! needs copy-sync, whether an unsandboxed fallback exists, and what extra
//! mounts to wire. Everything outside this module is harness-agnostic.

use std::convert::Infallible;
use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::util::{resolve_snap_shim, which};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Harness {
    Claude,
    Opencode,
}

impl Harness {
    pub const ALL: [Harness; 2] = [Self::Claude, Self::Opencode];

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|h| h.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
        }
    }

    /// Comma-separated harness names, for error and help text.
    pub fn known() -> String {
        Self::ALL.map(Self::name).join(", ")
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

    fn default_bin(
        self,
        home: &Path,
        snap_root: &Path,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Option<PathBuf> {
        match self {
            Self::Claude => Some(home.join(".local/bin/claude")),
            // A PATH hit may be the snap dispatcher (/snap/bin/opencode ->
            // /usr/bin/snap), whose snap-confine refuses to run inside the
            // unprivileged user namespace; use the snap's real binary instead.
            Self::Opencode => which("opencode", env)
                .map(|p| resolve_snap_shim("opencode", &p, snap_root).unwrap_or(p)),
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

    /// Harness-specific bwrap arguments, appended after the generic ones.
    pub fn extra_bwrap_args(self, ctx: &Ctx) -> Result<Vec<OsString>> {
        match self {
            Self::Claude => claude_shared_agent_mounts(ctx),
            // opencode's global config is the real, XDG-proper
            // ~/.config/opencode — already visible inside the namespace.
            // Share pieces of ~/.config/agents into it with plain symlinks
            // instead; no mount wiring needed.
            Self::Opencode => {
                let mut args: Vec<OsString> = Vec::new();
                // The snap wrapper we bypass sets this (the squashfs the
                // binary lives on is read-only, so self-update cannot work);
                // preserve it when running the snap's binary directly. A
                // plain prefix check rather than the dispatcher resolution:
                // anything under /snap is on the read-only squashfs, however
                // it was selected — an explicit MITTENS_OPENCODE_BIN
                // included.
                if ctx.bin.as_deref().is_some_and(|b| b.starts_with("/snap")) {
                    args.push("--setenv".into());
                    args.push("OPENCODE_DISABLE_AUTOUPDATE".into());
                    args.push("1".into());
                }
                Ok(args)
            }
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

/// Everything resolved once from the environment for the selected harness.
pub struct Ctx {
    pub harness: Harness,
    pub home: PathBuf,
    /// The tool's executable; None if it could not be found.
    pub bin: Option<PathBuf>,
    pub state: PathBuf,
    pub agents_cfg: PathBuf,
    /// Where snapd mounts snaps (normally /snap). MITTENS_SNAP_ROOT relocates
    /// it purely so tests can exercise the snap wiring (like
    /// MITTENS_DELAY_SECS).
    pub snap_root: PathBuf,
}

impl Ctx {
    pub fn resolve(harness: Harness) -> Self {
        Self::resolve_with(harness, &|k| std::env::var_os(k))
    }

    pub fn resolve_with(harness: Harness, env: &dyn Fn(&str) -> Option<OsString>) -> Self {
        let home = PathBuf::from(env("HOME").expect("HOME is not set"));
        let state = env("MITTENS_STATE_DIR").map(PathBuf::from).unwrap_or_else(|| {
            env("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/state"))
                .join(harness.state_leaf())
        });
        let agents_cfg = env("MITTENS_AGENTS_DIR").map(PathBuf::from).unwrap_or_else(|| {
            env("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("agents")
        });
        let snap_root =
            env("MITTENS_SNAP_ROOT").map(PathBuf::from).unwrap_or_else(|| "/snap".into());
        let bin = env(harness.bin_env())
            .map(PathBuf::from)
            .or_else(|| harness.default_bin(&home, &snap_root, env));
        Ctx { harness, home, bin, state, agents_cfg, snap_root }
    }

    /// `<state>/dot-<name>` — mounted over `~/.<name>` inside the namespace.
    pub fn dot_state(&self) -> PathBuf {
        self.state.join(format!("dot-{}", &self.harness.dot()[1..]))
    }

    /// Host-side home of the copy-synced config file (`<state>/claude.json`).
    pub fn sync_state(&self) -> Option<PathBuf> {
        self.harness.sync_file().map(|f| self.state.join(&f[1..]))
    }

    /// The real-home path the tool hardcodes (`~/.claude`, `~/.opencode`).
    pub fn home_dot(&self) -> PathBuf {
        self.home.join(self.harness.dot())
    }

    /// Human-readable list of the real-home paths the startup guard checks.
    pub fn guard_names(&self) -> String {
        match self.harness.sync_file() {
            Some(sync) => format!("~/{} or ~/{}*", self.harness.dot(), sync),
            None => format!("~/{}", self.harness.dot()),
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
    fn resolves_defaults_per_harness() {
        let env = env_from(&[("HOME", "/h")]);
        let c = Ctx::resolve_with(Harness::Claude, &env);
        assert_eq!(c.state, PathBuf::from("/h/.local/state/claude-home"));
        assert_eq!(c.dot_state(), PathBuf::from("/h/.local/state/claude-home/dot-claude"));
        assert_eq!(c.sync_state(), Some(PathBuf::from("/h/.local/state/claude-home/claude.json")));
        assert_eq!(c.bin, Some(PathBuf::from("/h/.local/bin/claude")));
        assert_eq!(c.agents_cfg, PathBuf::from("/h/.config/agents"));

        let o = Ctx::resolve_with(Harness::Opencode, &env);
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
        let c = Ctx::resolve_with(Harness::Claude, &env);
        assert_eq!(c.state, PathBuf::from("/elsewhere"));
        assert_eq!(c.bin, Some(PathBuf::from("/opt/claude")));
        assert_eq!(c.agents_cfg, PathBuf::from("/cfg/agents"));

        let env = env_from(&[("HOME", "/h"), ("XDG_STATE_HOME", "/xdg-state")]);
        let c = Ctx::resolve_with(Harness::Claude, &env);
        assert_eq!(c.state, PathBuf::from("/xdg-state/claude-home"));
    }

    #[test]
    fn opencode_path_hit_resolves_through_the_snap_dispatcher() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let executable = |path: &Path| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        // PATH finds <bin>/opencode, a symlink to the snap dispatcher.
        let dispatcher = tmp.path().join("usr-bin/snap");
        executable(&dispatcher);
        let path_dir = tmp.path().join("bin");
        fs::create_dir_all(&path_dir).unwrap();
        std::os::unix::fs::symlink(&dispatcher, path_dir.join("opencode")).unwrap();
        let snap_root = tmp.path().join("snap");
        let real = snap_root.join("opencode/current/bin/opencode");
        executable(&real);

        let pairs = [
            ("HOME", "/h"),
            ("PATH", path_dir.to_str().unwrap()),
            ("MITTENS_SNAP_ROOT", snap_root.to_str().unwrap()),
        ];
        let env = env_from(&pairs);
        let ctx = Ctx::resolve_with(Harness::Opencode, &env);
        assert_eq!(ctx.bin, Some(real.clone()));

        // A PATH hit that is not the dispatcher passes through untouched.
        let plain = tmp.path().join("plain/opencode");
        executable(&plain);
        let pairs = [
            ("HOME", "/h"),
            ("PATH", plain.parent().unwrap().to_str().unwrap()),
            ("MITTENS_SNAP_ROOT", snap_root.to_str().unwrap()),
        ];
        let env = env_from(&pairs);
        assert_eq!(Ctx::resolve_with(Harness::Opencode, &env).bin, Some(plain.clone()));

        // An explicit override is used verbatim, dispatcher or not.
        let pairs = [
            ("HOME", "/h"),
            ("PATH", path_dir.to_str().unwrap()),
            ("MITTENS_SNAP_ROOT", snap_root.to_str().unwrap()),
            ("MITTENS_OPENCODE_BIN", "/snap/bin/opencode"),
        ];
        let env = env_from(&pairs);
        let ctx = Ctx::resolve_with(Harness::Opencode, &env);
        assert_eq!(ctx.bin, Some(PathBuf::from("/snap/bin/opencode")));
    }

    #[test]
    fn harness_names_round_trip() {
        for harness in Harness::ALL {
            assert_eq!(Harness::from_name(harness.name()), Some(harness));
        }
        assert_eq!(Harness::from_name("emacs"), None);
        assert_eq!(Harness::known(), "claude, opencode");
    }
}
