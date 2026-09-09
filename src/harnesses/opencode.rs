//! opencode: mostly XDG-clean, but still drops a legacy ~/.opencode into the
//! real home. Often installed as a snap, which shapes how its binary is found
//! and run.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{Ctx, HarnessSpec, UnsafeSupport};
use crate::util::{resolve_snap_shim, which};

pub struct Opencode;

impl HarnessSpec for Opencode {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn dot(&self) -> &'static str {
        ".opencode"
    }

    fn state_leaf(&self) -> &'static str {
        "opencode-home"
    }

    fn bin_env(&self) -> &'static str {
        "MITTENS_OPENCODE_BIN"
    }

    // A PATH hit may be the snap dispatcher (/snap/bin/opencode ->
    // /usr/bin/snap), whose snap-confine refuses to run inside the
    // unprivileged user namespace; use the snap's real binary instead.
    fn default_bin(
        &self,
        _home: &Path,
        snap_root: &Path,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Option<PathBuf> {
        which("opencode", env).map(|p| resolve_snap_shim("opencode", &p, snap_root).unwrap_or(p))
    }

    fn unsafe_support(&self) -> UnsafeSupport {
        UnsafeSupport::Refused(
            "OPENCODE_CONFIG_DIR only relocates config loading, nothing \
             relocates ~/.opencode itself",
        )
    }

    // opencode's global config is the real, XDG-proper ~/.config/opencode —
    // already visible inside the namespace. Share pieces of ~/.config/agents
    // into it with plain symlinks instead; no mount wiring needed.
    fn extra_bwrap_args(&self, ctx: &Ctx) -> Result<Vec<OsString>> {
        let mut args: Vec<OsString> = Vec::new();
        // The snap wrapper we bypass sets this (the squashfs the binary lives
        // on is read-only, so self-update cannot work); preserve it when
        // running the snap's binary directly. A plain prefix check rather than
        // the dispatcher resolution: anything under /snap is on the read-only
        // squashfs, however it was selected — an explicit MITTENS_OPENCODE_BIN
        // included.
        if ctx.bin.as_deref().is_some_and(|b| b.starts_with("/snap")) {
            args.push("--setenv".into());
            args.push("OPENCODE_DISABLE_AUTOUPDATE".into());
            args.push("1".into());
        }
        Ok(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harnesses::{OPENCODE, env_from};
    use std::fs;

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
        let ctx = Ctx::resolve_with(OPENCODE, &env);
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
        assert_eq!(Ctx::resolve_with(OPENCODE, &env).bin, Some(plain.clone()));

        // An explicit override is used verbatim, dispatcher or not.
        let pairs = [
            ("HOME", "/h"),
            ("PATH", path_dir.to_str().unwrap()),
            ("MITTENS_SNAP_ROOT", snap_root.to_str().unwrap()),
            ("MITTENS_OPENCODE_BIN", "/snap/bin/opencode"),
        ];
        let env = env_from(&pairs);
        let ctx = Ctx::resolve_with(OPENCODE, &env);
        assert_eq!(ctx.bin, Some(PathBuf::from("/snap/bin/opencode")));
    }
}
