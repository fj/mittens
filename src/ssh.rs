//! System-wide ssh config replicas: make ssh work inside the sandbox.
//!
//! Only the invoking uid is mapped in the sandbox's user namespace, so every
//! root-owned file appears as nobody:nogroup inside it. OpenSSH perm-checks
//! each file it pulls in through an `Include` directive — the owner must be
//! root or the invoking user — so the stock
//! `Include /etc/ssh/ssh_config.d/*.conf` in /etc/ssh/ssh_config aborts ssh at
//! startup on any distribution that ships a drop-in there:
//!
//!     Bad owner or permissions on /etc/ssh/ssh_config.d/20-systemd-ssh-proxy.conf
//!
//! That is fatal, not a warning, and it hits every ssh invocation inside the
//! namespace: git over ssh, scp, rsync, ProxyJump, anything shelling out to
//! ssh. The ownership cannot be repaired from within — an unprivileged user
//! namespace cannot map root — so mittens replicates each included file byte
//! for byte under `<state>/ssh-config` (a mirror of its absolute path), where
//! the copies belong to the invoking user, and read-only bind-mounts each copy
//! over the original inside the namespace. What ssh reads is unchanged; only
//! the owner it sees is.
//!
//! /etc/ssh/ssh_config itself is deliberately left alone: OpenSSH reads the
//! system-wide file without the ownership check, so mounting over it would
//! churn mounts without changing behavior. A config that includes nothing —
//! or a distribution with an empty drop-in directory — produces no arguments
//! at all.

use std::collections::BTreeSet;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// OpenSSH's own `Include` recursion limit. A config that exceeds it makes ssh
/// die with "Too many recursive configuration includes" regardless of mittens.
const MAX_INCLUDE_DEPTH: usize = 16;

/// Prefix of the scratch name a replica is written under before being renamed
/// into place; never a replica itself, so pruning must leave it alone.
const TEMPORARY_PREFIX: &str = ".tmp-";

/// Bwrap arguments overmounting every file included by the system-wide ssh
/// config with a replica owned by the invoking user, refreshed under
/// `<state>/ssh-config` as a side effect. Empty when the config includes
/// nothing readable.
pub fn config_args(state: &Path, etc_ssh: &Path) -> Result<Vec<OsString>> {
    let replica_root = state.join("ssh-config");
    let mut args = Vec::new();
    let mut replicas = BTreeSet::new();
    for target in included(&etc_ssh.join("ssh_config"), etc_ssh) {
        let replica = replicate(&replica_root, &target)
            .with_context(|| format!("replicating {}", target.display()))?;
        args.push("--ro-bind".into());
        args.push(replica.clone().into());
        args.push(target.into());
        replicas.insert(replica);
    }
    // Drop-ins come and go with package updates; leftover replicas are inert
    // (nothing mounts them) but would accumulate in the state directory.
    prune(&replica_root, &replicas);
    Ok(args)
}

/// Canonical paths of every readable file the config at `root` pulls in
/// through `Include`, transitively. `base` anchors relative patterns.
/// `root` itself is not included: ssh reads it without the ownership check.
fn included(root: &Path, base: &Path) -> BTreeSet<PathBuf> {
    let (mut seen, mut found) = (BTreeSet::new(), BTreeSet::new());
    walk(root, base, 0, &mut seen, &mut found);
    found
}

fn walk(
    path: &Path,
    base: &Path,
    depth: usize,
    seen: &mut BTreeSet<PathBuf>,
    found: &mut BTreeSet<PathBuf>,
) {
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    // Canonical throughout: a drop-in is routinely a symlink into
    // /usr/lib/systemd, and ssh perm-checks the file it lands on, not the
    // link. That is also the path to mount the replica over, and it collapses
    // two links to one target into a single replica.
    let Ok(path) = fs::canonicalize(path) else {
        return;
    };
    if !path.is_file() || !seen.insert(path.clone()) {
        return;
    }
    // An unreadable file cannot be replicated; leaving the original in place
    // makes ssh fail exactly as it would outside the sandbox.
    let Ok(text) = fs::read(&path) else {
        return;
    };
    if depth > 0 {
        found.insert(path.clone());
    }
    for pattern in include_patterns(&String::from_utf8_lossy(&text)) {
        for target in glob(&anchor(&pattern, base)) {
            walk(&target, base, depth + 1, seen, found);
        }
    }
}

/// The patterns of every `Include` line in a config file. Keywords are
/// case-insensitive and may be separated from their arguments by `=`;
/// arguments may be double-quoted; `#` opens a comment only at the start of a
/// line. Includes are collected regardless of the enclosing Host/Match block,
/// because ssh opens — and perm-checks — even those whose block does not
/// match the host being connected to.
fn include_patterns(text: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with('#') {
            continue;
        }
        let end = line
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(line.len());
        let (keyword, rest) = line.split_at(end);
        if keyword.eq_ignore_ascii_case("include") {
            patterns.extend(split_args(rest.trim_start_matches(['=', ' ', '\t'])));
        }
    }
    patterns
}

/// Split on whitespace, honoring the double quotes ssh_config allows around
/// an argument.
fn split_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut arg = String::new();
    let (mut quoted, mut started) = (false, false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if !quoted && c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut arg));
                    started = false;
                }
            }
            c => {
                arg.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(arg);
    }
    args
}

/// Anchor an Include pattern the way ssh_config(5) does for a system-wide
/// config: a relative pattern resolves against /etc/ssh. (`~` is legal only in
/// a user config — ssh rejects it outright here, and joining it onto the base
/// simply matches nothing.)
fn anchor(pattern: &str, base: &Path) -> String {
    match Path::new(pattern).is_absolute() {
        true => pattern.to_string(),
        false => base.join(pattern).to_string_lossy().into_owned(),
    }
}

/// glob(3) — the same expansion OpenSSH itself applies to Include patterns,
/// rather than an approximation of it. Unmatched patterns yield nothing.
fn glob(pattern: &str) -> Vec<PathBuf> {
    let Ok(pattern) = CString::new(pattern) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    // SAFETY: glob_t is a plain C struct that glob(3) populates on success and
    // globfree releases, including the partial results left behind by an
    // aborted or out-of-memory return. Zeroing it first makes globfree safe
    // even for the returns that touch nothing.
    unsafe {
        let mut glob: libc::glob_t = std::mem::zeroed();
        if libc::glob(pattern.as_ptr(), 0, None, &mut glob) == 0 {
            for i in 0..glob.gl_pathc {
                let path = CStr::from_ptr(*glob.gl_pathv.add(i));
                found.push(PathBuf::from(OsStr::from_bytes(path.to_bytes())));
            }
        }
        libc::globfree(&mut glob);
    }
    found
}

/// Copy `target` to its mirror under `replica_root` (the same absolute path,
/// reparented), which makes the copy the invoking user's. Written to a
/// temporary name and renamed into place so a concurrently starting session
/// can never bind a half-written config; a session already running has the old
/// inode pinned by its mount and is unaffected either way. The source's
/// permission bits ride along, so a genuinely world-writable drop-in keeps
/// failing ssh's check inside the sandbox exactly as it does outside.
fn replicate(replica_root: &Path, target: &Path) -> Result<PathBuf> {
    let replica = replica_root.join(target.strip_prefix("/").unwrap_or(target));
    let (dir, name) = (
        replica.parent().context("replica path has no parent")?,
        replica
            .file_name()
            .context("replica path has no file name")?,
    );
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(temporary_name(name));
    let _ = fs::remove_file(&tmp);
    fs::copy(target, &tmp).with_context(|| format!("copying to {}", tmp.display()))?;
    fs::rename(&tmp, &replica).with_context(|| format!("renaming to {}", replica.display()))?;
    Ok(replica)
}

/// Remove replicas that are no longer included, along with `dir` itself and
/// any directory under it left empty. Best-effort: a stale replica is inert,
/// so failing to remove one must never fail a launch.
fn prune(dir: &Path, keep: &BTreeSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune(&path, keep);
        // A .tmp- entry belongs to a session that is replicating right now;
        // deleting it would break the rename that is about to follow.
        } else if !keep.contains(&path) && !is_temporary(&entry.file_name()) {
            let _ = fs::remove_file(&path);
        }
    }
    let _ = fs::remove_dir(dir); // only succeeds once it is empty
}

/// The scratch name `replicate` writes before renaming into place.
fn temporary_name(name: &OsStr) -> OsString {
    let mut tmp = OsString::from(TEMPORARY_PREFIX);
    tmp.push(name);
    tmp
}

fn is_temporary(name: &OsStr) -> bool {
    name.as_bytes().starts_with(TEMPORARY_PREFIX.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake /etc/ssh plus a state directory, under one tempdir.
    struct Fake {
        _tmp: tempfile::TempDir,
        etc_ssh: PathBuf,
        state: PathBuf,
    }

    impl Fake {
        fn new(ssh_config: &str) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let etc_ssh = tmp.path().join("etc-ssh");
            let state = tmp.path().join("state");
            fs::create_dir_all(etc_ssh.join("ssh_config.d")).unwrap();
            fs::create_dir_all(&state).unwrap();
            fs::write(etc_ssh.join("ssh_config"), ssh_config).unwrap();
            Fake {
                _tmp: tmp,
                etc_ssh,
                state,
            }
        }

        fn drop_in(&self, name: &str, body: &str) -> PathBuf {
            let path = self.etc_ssh.join("ssh_config.d").join(name);
            fs::write(&path, body).unwrap();
            path
        }

        fn args(&self) -> Vec<OsString> {
            config_args(&self.state, &self.etc_ssh).unwrap()
        }

        /// (replica, original) pairs from a `--ro-bind` argument list.
        fn binds(&self) -> Vec<(PathBuf, PathBuf)> {
            let args = self.args();
            assert_eq!(args.len() % 3, 0, "{args:?}");
            args.chunks(3)
                .map(|c| {
                    assert_eq!(c[0], OsString::from("--ro-bind"));
                    (PathBuf::from(&c[1]), PathBuf::from(&c[2]))
                })
                .collect()
        }

        fn replica_root(&self) -> PathBuf {
            self.state.join("ssh-config")
        }
    }

    #[test]
    fn drop_ins_are_replicated_and_mounted_over_the_original() {
        let fake = Fake::new("Include ssh_config.d/*.conf\n\nHost *\n  SendEnv LANG\n");
        let conf = fake.drop_in(
            "20-proxy.conf",
            "Host .host\n  ProxyCommand /bin/proxy %p\n",
        );
        fake.drop_in("ignored.txt", "Host nope\n");

        // Only the glob's matches are wired, and each replica sits at its
        // original absolute path mirrored under the state directory.
        let binds = fake.binds();
        assert_eq!(binds.len(), 1, "{binds:?}");
        let (replica, original) = &binds[0];
        assert_eq!(original, &conf);
        assert_eq!(
            replica,
            &fake.replica_root().join(conf.strip_prefix("/").unwrap())
        );
        // Byte-identical: the replica changes the owner ssh sees, nothing else.
        assert_eq!(fs::read(replica).unwrap(), fs::read(conf).unwrap());
        // The system-wide file itself is read without an ownership check, so
        // overmounting it would only churn mounts.
        assert!(
            !fake
                .args()
                .iter()
                .any(|a| a == &OsString::from(fake.etc_ssh.join("ssh_config")))
        );
    }

    #[test]
    fn replicas_belong_to_the_invoking_user_and_keep_the_source_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let fake = Fake::new("Include ssh_config.d/*.conf\n");
        let conf = fake.drop_in("20-proxy.conf", "Host .host\n");
        // World-writable: ssh rejects that whoever owns the file, so the bits
        // have to ride along or the sandbox would silently accept a config
        // that fails outside it.
        fs::set_permissions(&conf, fs::Permissions::from_mode(0o666)).unwrap();

        let replica = &fake.binds()[0].0;
        let (replica_meta, source_meta) =
            (fs::metadata(replica).unwrap(), fs::metadata(&conf).unwrap());
        // The whole point of the replica: an independent file owned by the
        // invoking user. A hard link or symlink would be byte-identical and
        // still carry the original's root ownership into the namespace.
        assert_eq!(replica_meta.uid(), unsafe { libc::geteuid() });
        assert_ne!(replica_meta.ino(), source_meta.ino());
        assert_eq!(
            replica_meta.permissions().mode(),
            source_meta.permissions().mode()
        );
    }

    #[test]
    fn a_config_that_includes_nothing_produces_no_arguments() {
        let fake = Fake::new("Include ssh_config.d/*.conf\nHost *\n  SendEnv LANG\n");
        // Empty drop-in directory: the glob matches nothing.
        assert!(fake.args().is_empty());
        assert!(
            !fake.replica_root().exists(),
            "an empty replica directory was left behind"
        );

        // No system config at all (a distribution that ships none).
        fs::remove_file(fake.etc_ssh.join("ssh_config")).unwrap();
        assert!(fake.args().is_empty());
    }

    #[test]
    fn includes_are_followed_transitively_and_cycles_terminate() {
        let fake = Fake::new("Include ssh_config.d/first.conf\n");
        let first = fake.drop_in("first.conf", "Include ssh_config.d/second.conf\n");
        // Deliberate cycle: the walk must terminate, not recurse forever.
        let second = fake.drop_in("second.conf", "Include ssh_config.d/first.conf\n");

        let originals: Vec<PathBuf> = fake.binds().into_iter().map(|(_, o)| o).collect();
        assert_eq!(originals, vec![first, second]);
    }

    #[test]
    fn a_symlinked_drop_in_is_replicated_by_its_target() {
        // Debian ships 20-systemd-ssh-proxy.conf as a symlink into
        // /usr/lib/systemd; ssh perm-checks the file it lands on, so that is
        // what has to be replaced.
        let fake = Fake::new("Include ssh_config.d/*.conf\n");
        let real = fake.etc_ssh.join("elsewhere.conf");
        fs::write(&real, "Host .host\n").unwrap();
        let link = fake.etc_ssh.join("ssh_config.d/20-linked.conf");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // A second link to the same target must not produce a second replica.
        let twin = fake.etc_ssh.join("ssh_config.d/30-linked.conf");
        std::os::unix::fs::symlink(&real, &twin).unwrap();

        let binds = fake.binds();
        assert_eq!(binds.len(), 1, "{binds:?}");
        assert_eq!(binds[0].1, real);
    }

    #[test]
    fn unreadable_includes_are_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        // Root reads a 0o000 file happily, so there is nothing to assert.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let fake = Fake::new("Include ssh_config.d/*.conf\n");
        let secret = fake.drop_in("10-secret.conf", "Host secret\n");
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fake.drop_in("20-open.conf", "Host open\n");

        // Nothing can be replicated from a file mittens cannot read; the
        // original stays in place and ssh fails on it exactly as it would
        // outside the sandbox, rather than mittens refusing to launch.
        let originals: Vec<PathBuf> = fake.binds().into_iter().map(|(_, o)| o).collect();
        assert_eq!(originals, vec![readable]);
    }

    #[test]
    fn stale_replicas_are_pruned_on_the_next_launch() {
        let fake = Fake::new("Include ssh_config.d/*.conf\n");
        let conf = fake.drop_in("20-proxy.conf", "Host .host\n");
        let stale = fake.drop_in("10-old.conf", "Host old\n");
        assert_eq!(fake.binds().len(), 2);
        let stale_replica = fake.replica_root().join(stale.strip_prefix("/").unwrap());
        assert!(stale_replica.is_file());

        // A replica another session is in the middle of writing is not a
        // stale replica; deleting it would break the rename about to follow.
        let in_flight = fake.replica_root().join(
            conf.parent()
                .unwrap()
                .strip_prefix("/")
                .unwrap()
                .join(temporary_name(OsStr::new("x"))),
        );
        fs::write(&in_flight, "half-written").unwrap();

        // The drop-in is removed by a package update; its replica goes too,
        // while the surviving one is refreshed in place.
        fs::remove_file(&stale).unwrap();
        fs::write(&conf, "Host .host\n  ProxyCommand /bin/new %p\n").unwrap();
        let binds = fake.binds();
        assert_eq!(binds.len(), 1, "{binds:?}");
        assert!(!stale_replica.exists(), "stale replica survived");
        assert!(
            in_flight.exists(),
            "another session's in-flight replica was pruned"
        );
        assert_eq!(fs::read(&binds[0].0).unwrap(), fs::read(&conf).unwrap());

        // Emptied out entirely, the replica tree leaves no directories behind.
        fs::remove_file(&in_flight).unwrap();
        fs::remove_file(&conf).unwrap();
        assert!(fake.args().is_empty());
        assert!(!fake.replica_root().exists());
    }

    #[test]
    fn include_lines_are_parsed_like_ssh_config() {
        let patterns = include_patterns(
            "# Include commented.conf\n\
             Include plain.conf\n\
             include lowercase.conf\n\
             INCLUDE=equals.conf\n\
             \tInclude  first.conf   second.conf\n\
             Include \"quoted name.conf\"\n\
             Match host nope\n\
             \tInclude inactive.conf\n\
             IncludeNothing not-a-pattern.conf\n\
             HostName include.example.com\n",
        );
        assert_eq!(
            patterns,
            [
                "plain.conf",
                "lowercase.conf",
                "equals.conf",
                "first.conf",
                "second.conf",
                "quoted name.conf",
                // Collected even though its Match block does not apply: ssh
                // opens and perm-checks inactive includes all the same.
                "inactive.conf",
            ]
        );
    }

    #[test]
    fn relative_patterns_anchor_to_the_system_config_directory() {
        let etc_ssh = Path::new("/etc/ssh");
        assert_eq!(
            anchor("ssh_config.d/*.conf", etc_ssh),
            "/etc/ssh/ssh_config.d/*.conf"
        );
        assert_eq!(anchor("/opt/site.conf", etc_ssh), "/opt/site.conf");
        // ~ is legal only in a user config; ssh rejects it in a system one.
        assert_eq!(anchor("~/mine.conf", etc_ssh), "/etc/ssh/~/mine.conf");
    }

    #[test]
    fn glob_expands_wildcards_and_yields_nothing_for_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().display();
        fs::write(tmp.path().join("a.conf"), "").unwrap();
        fs::write(tmp.path().join("b.conf"), "").unwrap();
        fs::write(tmp.path().join("c.txt"), "").unwrap();

        assert_eq!(
            glob(&format!("{dir}/*.conf")),
            vec![tmp.path().join("a.conf"), tmp.path().join("b.conf")]
        );
        assert_eq!(
            glob(&format!("{dir}/a.conf")),
            vec![tmp.path().join("a.conf")]
        );
        assert!(glob(&format!("{dir}/*.missing")).is_empty());
        assert!(glob("relative\0nul").is_empty());
    }
}
