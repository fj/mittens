//! pi: hardcodes ~/.pi, whose only content is the agent config directory
//! ~/.pi/agent — global settings, credentials, sessions, downloaded helper
//! binaries, and the authored config it reads through mounts. Project-local
//! `.pi` directories are pi's own convention inside a repository and are none
//! of mittens' business.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{Ctx, HarnessSpec, UnsafeSupport, agent_config};
use crate::util::which;

/// pi's global config directory, one level below the dot directory the sandbox
/// shadows.
const AGENT_DIR: &str = "agent";

pub struct Pi;

impl HarnessSpec for Pi {
    fn name(&self) -> &'static str {
        "pi"
    }

    fn dot(&self) -> &'static str {
        ".pi"
    }

    fn state_leaf(&self) -> &'static str {
        "pi-home"
    }

    fn bin_env(&self) -> &'static str {
        "MITTENS_PI_BIN"
    }

    // A PATH hit that is really the snap dispatcher is kept, unlike opencode's:
    // the pi snap's command is a launcher script that dereferences $SNAP, so it
    // cannot be exec'd bare. Inside the namespace /snap/bin is the generated
    // shim directory (see src/snap.rs), whose entry supplies exactly that
    // environment before exec'ing the launcher.
    fn default_bin(
        &self,
        _home: &Path,
        _snap_root: &Path,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Option<PathBuf> {
        which("pi", env)
    }

    // The variable relocates the agent directory rather than ~/.pi, so it
    // points one level into the state dir; unsandboxed and wrapped runs then
    // read and write the very same files.
    fn unsafe_support(&self) -> UnsafeSupport {
        UnsafeSupport::Via {
            var: "PI_CODING_AGENT_DIR",
            dir: Some(AGENT_DIR),
        }
    }

    // pi reads its global config from ~/.pi/agent, and takes the memory file
    // under the shared config's own name.
    fn extra_bwrap_args(&self, ctx: &Ctx) -> Result<Vec<OsString>> {
        agent_config::mounts(
            ctx,
            AGENT_DIR,
            &["skills", "prompts", "themes", "tools"],
            "AGENTS.md",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harnesses::{PI, env_from};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_snap_dispatcher_path_hit_is_kept_for_the_in_namespace_shim() {
        let tmp = tempfile::tempdir().unwrap();
        let executable = |path: &Path| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        // PATH finds <bin>/pi, a symlink to the snap dispatcher.
        let dispatcher = tmp.path().join("usr-bin/snap");
        executable(&dispatcher);
        let path_dir = tmp.path().join("bin");
        fs::create_dir_all(&path_dir).unwrap();
        std::os::unix::fs::symlink(&dispatcher, path_dir.join("pi")).unwrap();
        // The snap also holds a bare command where opencode's resolution would
        // look, so this pins pi's choice rather than the absence of a snap.
        let snap_root = tmp.path().join("snap");
        executable(&snap_root.join("pi/current/bin/pi"));

        let pairs = [
            ("HOME", "/h"),
            ("PATH", path_dir.to_str().unwrap()),
            ("MITTENS_SNAP_ROOT", snap_root.to_str().unwrap()),
        ];
        let env = env_from(&pairs);
        let ctx = Ctx::resolve_with(PI, &env);
        // Resolving past the dispatcher would strip the $SNAP environment the
        // pi snap's launcher script dereferences.
        assert_eq!(ctx.bin, Some(path_dir.join("pi")));
    }
}
