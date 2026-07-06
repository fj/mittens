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

/// Quote for interpolation into a shell command line (like bash's ${var@Q}).
pub fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
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
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote(Path::new("/a/b")), "'/a/b'");
        assert_eq!(shell_quote(Path::new("/a'b")), r"'/a'\''b'");
    }

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512");
        assert_eq!(human_size(2048), "2.0K");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0M");
    }
}
