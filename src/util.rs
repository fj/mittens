use std::ffi::OsString;
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

/// Quote for interpolation into a shell command line (like bash's ${var@Q}).
pub fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
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
