use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn is_executable(path: &Path) -> bool {
    path.is_file()
        && path
            .metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// PATH lookup, like `command -v`.
pub fn which(name: &str, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let path = env("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// True if `path` ultimately resolves to the snap dispatcher binary (every
/// `/snap/bin` entry is a symlink chain ending at `/usr/bin/snap`). The
/// dispatcher re-execs through snap-confine, which is setuid and refuses to
/// run inside an unprivileged user namespace ("snap-confine has elevated
/// permissions and is not confined but should be") — so it can never work
/// under bwrap.
pub fn is_snap_dispatcher(path: &Path) -> bool {
    fs::canonicalize(path)
        .ok()
        .as_deref()
        .and_then(Path::file_name)
        == Some(OsStr::new("snap"))
}

/// Resolve a PATH hit that is really the snap dispatcher to the app's real
/// binary inside the mounted snap, `<snap_root>/<name>/current/bin/<name>`.
/// Classic snaps' binaries run fine when exec'd directly. None if `found` is
/// not the dispatcher or the snap has no such binary. Deliberately NOT the
/// snap.yaml `command:` resolution src/snap.rs uses for the in-sandbox
/// /snap/bin shims: that command is often a launcher script that dereferences
/// `$SNAP`, which is only set when exec'd through those shims — the harness
/// binary is exec'd bare, so it must be the real executable itself.
pub fn resolve_snap_shim(name: &str, found: &Path, snap_root: &Path) -> Option<PathBuf> {
    if !is_snap_dispatcher(found) {
        return None;
    }
    let real = snap_root.join(name).join("current/bin").join(name);
    is_executable(&real).then_some(real)
}

/// The cargo install root that owns `exe`, if it sits in a cargo-style
/// `<root>/bin` directory (`cargo install` always lays binaries out that
/// way). None for a binary living anywhere else.
pub fn cargo_install_root(exe: &Path) -> Option<&Path> {
    let bin_dir = exe.parent()?;
    if bin_dir.file_name() != Some(OsStr::new("bin")) {
        return None;
    }
    bin_dir.parent()
}

/// True if the file name begins with `prefix`, byte-wise — the one
/// definition of how "~/.claude.json*"-style globs match, shared by the
/// guard, the bind-back skip, the wipe, migrate, and the sync-out so they
/// cannot drift apart.
pub fn name_starts_with(name: &OsStr, prefix: &str) -> bool {
    name.as_bytes().starts_with(prefix.as_bytes())
}

/// Sorted paths of `dir` entries whose names begin with `prefix`.
pub fn entries_with_prefix(dir: &Path, prefix: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut matches: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| name_starts_with(&e.file_name(), prefix))
        .map(|e| e.path())
        .collect();
    matches.sort();
    Ok(matches)
}

/// Sorted names of `dir` entries.
pub fn sorted_entry_names(dir: &Path) -> std::io::Result<Vec<OsString>> {
    let mut names: Vec<OsString> = fs::read_dir(dir)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|e| e.file_name())
        .collect();
    names.sort();
    Ok(names)
}

/// True for a directory, false for a symlink to one: a symlink is a leaf that
/// moves or goes whole, never a tree to descend into.
pub fn is_real_dir(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.is_dir())
}

/// `path` written with the home directory as `~`, for messages.
pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Sleep for `secs`. MITTENS_DELAY_SECS overrides the duration (its only
/// purpose is letting the test suite run the countdown paths instantly).
pub fn pause(secs: u64) {
    let secs = std::env::var("MITTENS_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(secs);
    std::thread::sleep(std::time::Duration::from_secs(secs));
}

/// PIDs of running processes with exactly this name, via pgrep(1).
pub fn pgrep(name: &str) -> Vec<String> {
    let Ok(out) = Command::new("pgrep").args(["-x", name]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Approximate du -sh style humanization.
pub fn human_size(bytes: u64) -> String {
    let mut n = bytes as f64;
    for unit in ["", "K", "M", "G", "T"] {
        if n < 1024.0 {
            return if unit.is_empty() {
                format!("{}", bytes)
            } else if n < 10.0 {
                format!("{n:.1}{unit}")
            } else {
                format!("{n:.0}{unit}")
            };
        }
        n /= 1024.0;
    }
    format!("{n:.0}P")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_shim_resolves_to_the_snap_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let dispatcher = tmp.path().join("usr-bin/snap");
        fs::create_dir_all(dispatcher.parent().unwrap()).unwrap();
        fs::write(&dispatcher, "").unwrap();
        let shim = tmp.path().join("bin/opencode");
        fs::create_dir_all(shim.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&dispatcher, &shim).unwrap();
        let snap_root = tmp.path().join("snap");
        let real = snap_root.join("opencode/current/bin/opencode");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            resolve_snap_shim("opencode", &shim, &snap_root),
            Some(real.clone())
        );

        // A non-dispatcher hit passes through untouched.
        assert_eq!(resolve_snap_shim("opencode", &real, &snap_root), None);

        // A dispatcher whose snap lacks the expected binary resolves to None.
        fs::remove_file(&real).unwrap();
        assert_eq!(resolve_snap_shim("opencode", &shim, &snap_root), None);
    }

    #[test]
    fn cargo_install_root_requires_a_bin_parent() {
        assert_eq!(
            cargo_install_root(Path::new("/x/tools/bin/mittens")),
            Some(Path::new("/x/tools"))
        );
        assert_eq!(
            cargo_install_root(Path::new("/x/target/debug/mittens")),
            None
        );
        assert_eq!(cargo_install_root(Path::new("mittens")), None);
    }

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512");
        assert_eq!(human_size(2048), "2.0K");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0M");
    }
}
