//! Harness handlers: everything tool-specific lives in a child module here.
//!
//! Each harness defines the endpoints mittens must know about — the binary,
//! which real-home path the tool hardcodes, whether a top-level config file
//! needs copy-sync, whether an unsandboxed fallback exists, and what extra
//! mounts to wire. Everything outside this module is harness-agnostic.
//!
//! A harness is one file implementing HarnessSpec plus one entry in
//! Harness::ALL, the only list of the harnesses mittens knows about.
//! agent_config is the exception: wiring the shared agent config into a
//! tool's global config directory is one policy, shared by the harnesses
//! whose config directory the sandbox shadows.

mod agent_config;
mod claude;
mod opencode;
mod pi;

use std::convert::Infallible;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::util::entries_with_prefix;

/// Whether a tool can run outside the sandbox, and how.
pub enum UnsafeSupport {
    /// The variable that relocates every path the tool hardcodes, and what it
    /// must point at: the state directory the sandbox mounts over the tool's
    /// dot-directory, or `dir` below it when the variable relocates something
    /// deeper. Declaring the variable therefore forces declaring its target,
    /// so wrapped and unsandboxed runs cannot silently address different
    /// files.
    Via {
        var: &'static str,
        dir: Option<&'static str>,
    },
    /// Why the tool has no such variable.
    Refused(&'static str),
}

pub trait HarnessSpec: Sync {
    fn name(&self) -> &'static str;

    /// Top-level home entry the tool hardcodes; shadowed to
    /// `<state>/dot-<name>`.
    fn dot(&self) -> &'static str;

    /// Leaf of the default state directory under `~/.local/state`.
    fn state_leaf(&self) -> &'static str;

    /// Environment variable that overrides the tool's binary.
    fn bin_env(&self) -> &'static str;

    /// Top-level config file the tool rewrites via atomic rename, copy-synced
    /// through the tmpfs (stored host-side as `<state>/<name minus leading
    /// dot>`); None if the tool has no such file.
    fn sync_file(&self) -> Option<&'static str> {
        None
    }

    /// Where the tool's binary lives when bin_env() is unset.
    fn default_bin(
        &self,
        home: &Path,
        snap_root: &Path,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Option<PathBuf>;

    /// --unsafe: how the tool runs outside the sandbox. Refused by default: a
    /// tool qualifies only once it has a variable relocating every path it
    /// hardcodes.
    fn unsafe_support(&self) -> UnsafeSupport {
        UnsafeSupport::Refused("no environment variable relocates the paths it hardcodes")
    }

    /// Seed the state directory before an unsandboxed run.
    fn prepare_unsafe(&self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    /// Harness-specific bwrap arguments, appended after the generic ones.
    fn extra_bwrap_args(&self, _ctx: &Ctx) -> Result<Vec<OsString>> {
        Ok(Vec::new())
    }
}

#[derive(Clone, Copy)]
pub struct Harness(&'static dyn HarnessSpec);

pub const CLAUDE: Harness = Harness(&claude::Claude);
pub const OPENCODE: Harness = Harness(&opencode::Opencode);
pub const PI: Harness = Harness(&pi::Pi);

impl Harness {
    /// Every harness mittens knows about.
    pub const ALL: [Harness; 3] = [CLAUDE, OPENCODE, PI];

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|h| h.name() == name)
    }

    pub fn name(self) -> &'static str {
        self.0.name()
    }

    /// Comma-separated harness names, for error and help text.
    pub fn known() -> String {
        Self::ALL.map(Self::name).join(", ")
    }

    pub fn dot(self) -> &'static str {
        self.0.dot()
    }

    pub fn sync_file(self) -> Option<&'static str> {
        self.0.sync_file()
    }

    fn state_leaf(self) -> &'static str {
        self.0.state_leaf()
    }

    pub fn bin_env(self) -> &'static str {
        self.0.bin_env()
    }

    fn default_bin(
        self,
        home: &Path,
        snap_root: &Path,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Option<PathBuf> {
        self.0.default_bin(home, snap_root, env)
    }

    /// Run the tool outside the sandbox with its relocation variable pointed at
    /// the state dir, then clear whatever it left in the real home. Supervision
    /// lives here so no harness can skip the cleanup its own run depends on.
    pub fn unsafe_exec(self, ctx: &Ctx, bin: &Path, args: &[OsString]) -> Result<Infallible> {
        let (var, dir) = match self.0.unsafe_support() {
            UnsafeSupport::Via { var, dir } => (var, dir),
            UnsafeSupport::Refused(reason) => {
                bail!("--unsafe is not supported for {}: {reason}", self.name())
            }
        };
        let target = match dir {
            Some(dir) => ctx.dot_state().join(dir),
            None => ctx.dot_state(),
        };
        self.0.prepare_unsafe(ctx)?;
        // Ignore SIGINT/SIGQUIT in the parent so Ctrl-C reaches the child but
        // cleanup still runs after the child exits.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        }
        let mut cmd = Command::new(bin);
        cmd.args(args).env(var, target);
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                Ok(())
            });
        }
        let status = cmd
            .spawn()
            .with_context(|| format!("exec {}", bin.display()))?
            .wait()
            .with_context(|| format!("waiting for {}", bin.display()))?;
        cleanup_after_unsafe(ctx);
        std::process::exit(
            status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        );
    }

    pub fn extra_bwrap_args(self, ctx: &Ctx) -> Result<Vec<OsString>> {
        self.0.extra_bwrap_args(ctx)
    }
}

/// Names are unique across ALL, so they identify a harness. Addresses do not:
/// the specs are zero-sized, so two of them can share one address.
impl PartialEq for Harness {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

impl Eq for Harness {}

impl fmt::Debug for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// After a --unsafe run: silently remove any stray real-home paths so the
/// startup guard passes cleanly on the next invocation. Errors are non-fatal
/// warnings — the tool has already exited and we must not obscure its exit code.
fn cleanup_after_unsafe(ctx: &Ctx) {
    let mut paths = Vec::new();
    if ctx.home_dot().symlink_metadata().is_ok() {
        paths.push(ctx.home_dot());
    }
    if let Some(sync) = ctx.harness.sync_file() {
        match entries_with_prefix(&ctx.home, sync) {
            Ok(more) => paths.extend(more),
            Err(err) => {
                eprintln!("mittens: warning: could not enumerate cleanup targets: {err:#}");
                return;
            }
        }
    }
    for path in paths {
        let result = if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if let Err(err) = result
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "mittens: warning: could not clean up {}: {err}",
                path.display()
            );
        }
    }
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
    /// Where the system-wide ssh client config lives (normally /etc/ssh).
    /// MITTENS_ETC_SSH relocates it for the same test-only reason.
    pub etc_ssh: PathBuf,
}

impl Ctx {
    pub fn resolve(harness: Harness) -> Self {
        Self::resolve_with(harness, &|k| std::env::var_os(k))
    }

    pub fn resolve_with(harness: Harness, env: &dyn Fn(&str) -> Option<OsString>) -> Self {
        let home = PathBuf::from(env("HOME").expect("HOME is not set"));
        let state = env("MITTENS_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                env("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/state"))
                    .join(harness.state_leaf())
            });
        let agents_cfg = env("MITTENS_AGENTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                env("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".config"))
                    .join("agents")
            });
        let snap_root = env("MITTENS_SNAP_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/snap".into());
        let etc_ssh = env("MITTENS_ETC_SSH")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/etc/ssh".into());
        let bin = env(harness.bin_env())
            .map(PathBuf::from)
            .or_else(|| harness.default_bin(&home, &snap_root, env));
        Ctx {
            harness,
            home,
            bin,
            state,
            agents_cfg,
            snap_root,
            etc_ssh,
        }
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

/// Stand-in for the process environment, shared by this module's tests and
/// the harness modules'.
#[cfg(test)]
fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| {
        pairs
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| OsString::from(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_defaults_per_harness() {
        let env = env_from(&[("HOME", "/h")]);
        let c = Ctx::resolve_with(CLAUDE, &env);
        assert_eq!(c.state, PathBuf::from("/h/.local/state/claude-home"));
        assert_eq!(
            c.dot_state(),
            PathBuf::from("/h/.local/state/claude-home/dot-claude")
        );
        assert_eq!(
            c.sync_state(),
            Some(PathBuf::from("/h/.local/state/claude-home/claude.json"))
        );
        assert_eq!(c.bin, Some(PathBuf::from("/h/.local/bin/claude")));
        assert_eq!(c.agents_cfg, PathBuf::from("/h/.config/agents"));
        // The host paths every test overrides, so a typo here would disable
        // the snap and ssh wiring on real machines with a green suite.
        assert_eq!(c.snap_root, PathBuf::from("/snap"));
        assert_eq!(c.etc_ssh, PathBuf::from("/etc/ssh"));

        let o = Ctx::resolve_with(OPENCODE, &env);
        assert_eq!(o.state, PathBuf::from("/h/.local/state/opencode-home"));
        assert_eq!(
            o.dot_state(),
            PathBuf::from("/h/.local/state/opencode-home/dot-opencode")
        );
        assert_eq!(o.sync_state(), None);
        assert_eq!(o.bin, None); // no opencode on the empty PATH

        let p = Ctx::resolve_with(PI, &env);
        assert_eq!(p.state, PathBuf::from("/h/.local/state/pi-home"));
        assert_eq!(
            p.dot_state(),
            PathBuf::from("/h/.local/state/pi-home/dot-pi")
        );
        assert_eq!(p.sync_state(), None);
        assert_eq!(p.bin, None); // no pi on the empty PATH
    }

    #[test]
    fn env_overrides_win() {
        let env = env_from(&[
            ("HOME", "/h"),
            ("MITTENS_STATE_DIR", "/elsewhere"),
            ("MITTENS_CLAUDE_BIN", "/opt/claude"),
            ("MITTENS_AGENTS_DIR", "/cfg/agents"),
            ("MITTENS_ETC_SSH", "/cfg/etc-ssh"),
            ("XDG_STATE_HOME", "/xdg-state"),
        ]);
        let c = Ctx::resolve_with(CLAUDE, &env);
        assert_eq!(c.state, PathBuf::from("/elsewhere"));
        assert_eq!(c.bin, Some(PathBuf::from("/opt/claude")));
        assert_eq!(c.agents_cfg, PathBuf::from("/cfg/agents"));
        assert_eq!(c.etc_ssh, PathBuf::from("/cfg/etc-ssh"));

        let env = env_from(&[("HOME", "/h"), ("XDG_STATE_HOME", "/xdg-state")]);
        let c = Ctx::resolve_with(CLAUDE, &env);
        assert_eq!(c.state, PathBuf::from("/xdg-state/claude-home"));
    }

    #[test]
    fn harness_names_round_trip() {
        for harness in Harness::ALL {
            assert_eq!(Harness::from_name(harness.name()), Some(harness));
        }
        assert_ne!(CLAUDE, OPENCODE);
        assert_eq!(Harness::from_name("emacs"), None);
        assert_eq!(Harness::known(), "claude, opencode, pi");
    }

    #[test]
    fn harness_names_are_unique() {
        // from_name returns the first match and equality compares names, so a
        // duplicate would shadow one harness rather than fail to compile.
        let mut names = Harness::ALL.map(Harness::name);
        names.sort_unstable();
        let mut unique = names.to_vec();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "two harnesses share a name: {names:?}"
        );
    }

    /// The least a harness can declare: everything else comes from the trait.
    struct Bare;

    impl HarnessSpec for Bare {
        fn name(&self) -> &'static str {
            "bare"
        }

        fn dot(&self) -> &'static str {
            ".bare"
        }

        fn state_leaf(&self) -> &'static str {
            "bare-home"
        }

        fn bin_env(&self) -> &'static str {
            "MITTENS_BARE_BIN"
        }

        fn default_bin(
            &self,
            _home: &Path,
            _snap_root: &Path,
            _env: &dyn Fn(&str) -> Option<OsString>,
        ) -> Option<PathBuf> {
            None
        }
    }

    #[test]
    fn a_harness_gets_no_sync_file_no_extra_mounts_and_no_unsafe_run() {
        let bare = Harness(&Bare);
        let ctx = Ctx::resolve_with(bare, &env_from(&[("HOME", "/h")]));
        assert_eq!(bare.sync_file(), None);
        assert_eq!(bare.extra_bwrap_args(&ctx).unwrap(), Vec::<OsString>::new());
        // Refusing is the default, so a harness runs unsandboxed only when it
        // says how its data follows.
        let err = bare
            .unsafe_exec(&ctx, Path::new("/bin/true"), &[])
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "--unsafe is not supported for bare: no environment variable \
             relocates the paths it hardcodes"
        );
    }
}
