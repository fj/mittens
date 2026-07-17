//! Classic-snap shims: make snap-packaged tools work inside the sandbox.
//!
//! On a snapd system every command in /snap/bin is a symlink to the snap
//! dispatcher (/usr/bin/snap), which re-execs through snap-confine — and
//! snap-confine is setuid and refuses to run inside an unprivileged user
//! namespace ("snap-confine has elevated permissions and is not confined but
//! should be"). So every snap-packaged tool invoked inside the namespace
//! fails at startup, even classic-confined ones whose commands are plain
//! executables that run fine when exec'd directly (mittens already exploits
//! exactly that for the opencode harness binary; see util::resolve_snap_shim).
//!
//! mittens therefore generates a shim directory (`<state>/snap-bin`) and
//! read-only bind-mounts it over /snap/bin inside the namespace:
//!
//! - a classic snap's command becomes a small sh wrapper that exports the
//!   SNAP* variables `snap run` would have set — snaps' own wrapper scripts
//!   dereference `$SNAP` — plus the app's `environment:` from meta/snap.yaml,
//!   then execs the real command under `/snap/<snap>/current`;
//! - everything else is replicated unchanged. Strictly confined snaps
//!   genuinely need snap-confine's setuid privileges, which no wiring can
//!   provide inside the user namespace, so their entries keep failing with
//!   the honest snap-confine error rather than something mittens invented.
//!
//! The shim directory is refreshed on every launch entry by entry (write to a
//! temp name, then rename), never wholesale: concurrently running sessions
//! have it bind-mounted as their /snap/bin, and deleting it out from under
//! them would empty theirs too.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::util::{is_executable, is_snap_dispatcher};

/// Bwrap arguments overmounting `<snap_root>/bin` with the generated shim
/// directory, refreshed under `<state>/snap-bin` as a side effect. Empty when
/// there is no snap bin directory or nothing resolved to a wrapper (then the
/// overmount would only churn mounts without changing behavior).
pub fn shim_args(state: &Path, snap_root: &Path) -> Result<Vec<OsString>> {
    let snap_bin = snap_root.join("bin");
    if !snap_bin.is_dir() {
        return Ok(Vec::new());
    }
    let shim_dir = state.join("snap-bin");
    fs::create_dir_all(&shim_dir).with_context(|| format!("creating {}", shim_dir.display()))?;

    let mut names: Vec<OsString> = fs::read_dir(&snap_bin)
        .with_context(|| format!("reading {}", snap_bin.display()))?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|e| e.file_name())
        .collect();
    names.sort();

    let mut wrappers = 0;
    let mut desired: BTreeSet<OsString> = BTreeSet::new();
    for name in &names {
        let Some(shim) = resolve_entry(&snap_bin, name, snap_root) else {
            continue;
        };
        if matches!(shim, Shim::Wrapper(_)) {
            wrappers += 1;
        }
        install(&shim_dir, name, &shim)
            .with_context(|| format!("installing snap shim {}", name.to_string_lossy()))?;
        desired.insert(name.clone());
    }
    // Uninstalled snaps' leftovers would shadow nothing real; drop them.
    for entry in fs::read_dir(&shim_dir)? {
        let entry = entry?;
        if !desired.contains(&entry.file_name()) {
            let _ = fs::remove_file(entry.path());
        }
    }

    if wrappers == 0 {
        return Ok(Vec::new());
    }
    Ok(vec!["--ro-bind".into(), shim_dir.into(), snap_bin.into()])
}

enum Shim {
    /// Generated sh script for a classic snap's command; installed 0755.
    Wrapper(String),
    /// Anything unresolvable: replicate the original symlink target verbatim.
    Symlink(PathBuf),
    /// A regular file sitting in the snap bin directory: copy it through.
    Copy(PathBuf),
}

/// What `<snap_bin>/<name>` should look like in the shim directory; None for
/// entries that are neither symlinks nor regular files.
fn resolve_entry(snap_bin: &Path, name: &OsStr, snap_root: &Path) -> Option<Shim> {
    let path = snap_bin.join(name);
    let Ok(target) = fs::read_link(&path) else {
        return path.is_file().then_some(Shim::Copy(path));
    };
    // Only dispatcher entries are rewritten; a hand-placed or dangling
    // symlink is replicated as-is.
    if !is_snap_dispatcher(&path) {
        return Some(Shim::Symlink(target));
    }
    // snapd names dispatcher entries `<snap>.<app>` (just `<snap>` when the
    // app is named like the snap) and adds aliases as bare-name relative
    // symlinks to the qualified entry (`tofu -> opentofu.tofu`); follow those
    // to learn which snap and app this entry invokes.
    let mut qualified = name.to_string_lossy().into_owned();
    let mut hop = target.clone();
    for _ in 0..8 {
        if hop.is_absolute() || hop.components().count() != 1 {
            break;
        }
        qualified = hop.to_string_lossy().into_owned();
        match fs::read_link(snap_bin.join(&qualified)) {
            Ok(next) => hop = next,
            Err(_) => break,
        }
    }
    let (snap, app) = qualified.split_once('.').unwrap_or((&*qualified, &*qualified));
    match classic_command_wrapper(snap_root, snap, app) {
        Some(script) => Some(Shim::Wrapper(script)),
        None => Some(Shim::Symlink(target)),
    }
}

/// The wrapper script for `<snap>`'s app `<app>`, or None when the snap is
/// not classic, the app or its command is missing, or the command does not
/// resolve to an executable under the snap — in all of which cases the entry
/// is left alone.
fn classic_command_wrapper(snap_root: &Path, snap: &str, app_name: &str) -> Option<String> {
    let snap_dir = snap_root.join(snap).join("current");
    let meta = fs::read_to_string(snap_dir.join("meta/snap.yaml")).ok()?;
    let meta = parse_snap_yaml(&meta);
    if meta.confinement.as_deref() != Some("classic") {
        return None;
    }
    let app = meta.apps.into_iter().find(|a| a.name == app_name)?;
    let command = app.command?;
    let mut tokens = command.split_whitespace();
    // The first token is the executable, relative to the snap root (snapcraft
    // sometimes spells that out as an explicit $SNAP/ prefix).
    let tok0 = tokens.next()?;
    let rel = tok0.strip_prefix("$SNAP/").unwrap_or(tok0);
    if rel.starts_with('$') || !is_executable(&snap_dir.join(rel)) {
        return None;
    }

    let mut script = String::from(
        "#!/bin/sh\n\
         # Generated by mittens; refreshed on every launch — do not edit.\n\
         # Execs the classic snap's command directly: the snap dispatcher's\n\
         # snap-confine cannot run inside the unprivileged user namespace.\n",
    );
    // `current` rather than the revision directory, so the shim keeps working
    // across a snap refresh during a running session.
    script.push_str(&format!("export SNAP=\"{}\"\n", sh_dq(&snap_dir.to_string_lossy())));
    script.push_str(&format!("export SNAP_NAME=\"{}\"\n", sh_dq(snap)));
    script.push_str(&format!("export SNAP_INSTANCE_NAME=\"{}\"\n", sh_dq(snap)));
    if let Some(rev) = fs::read_link(&snap_dir)
        .ok()
        .and_then(|t| t.file_name().map(|f| f.to_string_lossy().into_owned()))
    {
        script.push_str(&format!("export SNAP_REVISION=\"{}\"\n", sh_dq(&rev)));
    }
    // App environment after the SNAP* exports: values routinely reference
    // $SNAP (and $PATH), which the shell expands at run time since sh_dq
    // leaves `$` alone.
    for (key, value) in &app.env {
        if !is_sh_identifier(key) {
            continue;
        }
        script.push_str(&format!("export {key}=\"{}\"\n", sh_dq(value)));
    }
    script.push_str(&format!("exec \"$SNAP/{}\"", sh_dq(rel)));
    for arg in tokens {
        script.push_str(&format!(" \"{}\"", sh_dq(arg)));
    }
    script.push_str(" \"$@\"\n");
    Some(script)
}

fn install(shim_dir: &Path, name: &OsStr, shim: &Shim) -> Result<()> {
    let tmp = shim_dir.join(format!(".tmp-{}", name.to_string_lossy()));
    let _ = fs::remove_file(&tmp);
    match shim {
        Shim::Wrapper(script) => {
            fs::write(&tmp, script)?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        }
        Shim::Symlink(target) => std::os::unix::fs::symlink(target, &tmp)?,
        Shim::Copy(src) => {
            fs::copy(src, &tmp)?;
        }
    }
    fs::rename(&tmp, shim_dir.join(name))?;
    Ok(())
}

/// Escape for a double-quoted sh string. `$` is deliberately left alone so
/// `$SNAP`-style references in commands and environment values keep their
/// snapd expansion semantics.
fn sh_dq(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '"' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn is_sh_identifier(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

struct SnapMeta {
    confinement: Option<String>,
    apps: Vec<SnapApp>,
}

struct SnapApp {
    name: String,
    command: Option<String>,
    env: Vec<(String, String)>,
}

/// The pieces of meta/snap.yaml mittens needs: `confinement` and each app's
/// `command` and `environment`. Line-based on snapcraft's fixed two-space
/// indentation rather than a YAML dependency; keys are only recognized at
/// their expected depth, so free text in block scalars (`description: |`)
/// cannot be mistaken for structure.
fn parse_snap_yaml(text: &str) -> SnapMeta {
    let mut meta = SnapMeta { confinement: None, apps: Vec::new() };
    let mut in_apps = false;
    let mut in_env = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent == 0 {
            in_apps = trimmed == "apps:";
            in_env = false;
            if let Some(v) = trimmed.strip_prefix("confinement:") {
                meta.confinement = Some(v.trim().to_string());
            }
            continue;
        }
        if !in_apps {
            continue;
        }
        if indent == 2 {
            in_env = false;
            if let Some(name) = trimmed.strip_suffix(':')
                && !name.contains(' ')
            {
                meta.apps.push(SnapApp { name: name.to_string(), command: None, env: Vec::new() });
            }
            continue;
        }
        let Some(app) = meta.apps.last_mut() else {
            continue;
        };
        if indent == 4 {
            in_env = trimmed == "environment:";
            if let Some(v) = trimmed.strip_prefix("command:") {
                app.command = Some(v.trim().to_string());
            }
            continue;
        }
        if in_env
            && indent == 6
            && let Some((k, v)) = trimmed.split_once(':')
        {
            app.env.push((k.trim().to_string(), unquote(v.trim()).to_string()));
        }
    }
    meta
}

/// Strip one matching pair of surrounding quotes, as snapcraft emits for
/// values like `'1'`.
fn unquote(s: &str) -> &str {
    for q in ['\'', '"'] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // Modeled on real snap.yamls: opentofu (command named unlike the snap,
    // top-level keys after apps), astral-uv ($SNAP-referencing environment),
    // opencode (quoted env value), codium (command with $SNAP prefix + args).
    const CLASSIC_YAML: &str = "\
name: opentofu
summary: OpenTofu
description: |
  Free text that must not parse as structure:
  confinement: strict
  command: not-a-command
apps:
  tofu:
    command: tofu
    environment:
      TOFU_FLAVOR: 'firm'
      PATH: $SNAP/bin:$PATH
      not-an-identifier: skipped
    aliases:
      - tofu
confinement: classic
grade: stable
";

    #[test]
    fn parses_the_needed_snap_yaml_subset() {
        let meta = parse_snap_yaml(CLASSIC_YAML);
        assert_eq!(meta.confinement.as_deref(), Some("classic"));
        assert_eq!(meta.apps.len(), 1);
        let app = &meta.apps[0];
        assert_eq!(app.name, "tofu");
        assert_eq!(app.command.as_deref(), Some("tofu"));
        assert_eq!(
            app.env,
            vec![
                ("TOFU_FLAVOR".to_string(), "firm".to_string()),
                ("PATH".to_string(), "$SNAP/bin:$PATH".to_string()),
                ("not-an-identifier".to_string(), "skipped".to_string()),
            ]
        );
    }

    #[test]
    fn description_text_cannot_upgrade_a_strict_snap_to_classic() {
        // The dangerous inverse of the CLASSIC_YAML decoy: free text in a
        // block scalar claiming classic confinement must not make a strict
        // snap eligible for wrappers.
        let meta = parse_snap_yaml(
            "name: locked\nconfinement: strict\ndescription: |\n  confinement: classic\napps:\n  locked:\n    command: locked\n",
        );
        assert_eq!(meta.confinement.as_deref(), Some("strict"));
    }

    #[test]
    fn parses_multiple_apps() {
        let meta = parse_snap_yaml(
            "apps:\n  uv:\n    command: bin/uv\n  uvx:\n    command: bin/uvx\nconfinement: classic\n",
        );
        let names: Vec<&str> = meta.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["uv", "uvx"]);
        assert_eq!(meta.apps[1].command.as_deref(), Some("bin/uvx"));
    }

    /// A fake snapd layout under `root`: dispatcher at `root/usr-bin/snap`,
    /// entries in `root/bin`, snaps under `root/<name>/<rev>` with a
    /// `current` symlink.
    struct FakeSnaps {
        root: PathBuf,
    }

    impl FakeSnaps {
        fn new(root: &Path) -> Self {
            fs::create_dir_all(root.join("bin")).unwrap();
            let dispatcher = root.join("usr-bin/snap");
            fs::create_dir_all(dispatcher.parent().unwrap()).unwrap();
            fs::write(&dispatcher, "").unwrap();
            fs::set_permissions(&dispatcher, fs::Permissions::from_mode(0o755)).unwrap();
            FakeSnaps { root: root.to_path_buf() }
        }

        fn dispatcher(&self) -> PathBuf {
            self.root.join("usr-bin/snap")
        }

        fn entry(&self, name: &str) {
            std::os::unix::fs::symlink(self.dispatcher(), self.root.join("bin").join(name))
                .unwrap();
        }

        fn alias(&self, name: &str, qualified: &str) {
            std::os::unix::fs::symlink(qualified, self.root.join("bin").join(name)).unwrap();
        }

        fn snap(&self, name: &str, rev: &str, yaml: &str, commands: &[&str]) {
            let rev_dir = self.root.join(name).join(rev);
            fs::create_dir_all(rev_dir.join("meta")).unwrap();
            fs::write(rev_dir.join("meta/snap.yaml"), yaml).unwrap();
            for cmd in commands {
                let path = rev_dir.join(cmd);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, format!("#!/bin/sh\necho REAL-{cmd} \"$@\"\n")).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            std::os::unix::fs::symlink(rev, self.root.join(name).join("current")).unwrap();
        }
    }

    #[test]
    fn classic_entries_become_wrappers_and_the_rest_replicate() {
        let tmp = tempfile::tempdir().unwrap();
        let snaps = FakeSnaps::new(tmp.path());
        snaps.snap("opentofu", "252", CLASSIC_YAML, &["tofu"]);
        snaps.entry("opentofu.tofu");
        snaps.alias("tofu", "opentofu.tofu");
        snaps.snap(
            "strictly",
            "9",
            "name: strictly\nconfinement: strict\napps:\n  strictly:\n    command: strictly\n",
            &["strictly"],
        );
        snaps.entry("strictly");

        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let args = shim_args(&state, tmp.path()).unwrap();
        let shim_dir = state.join("snap-bin");
        assert_eq!(
            args,
            vec![
                OsString::from("--ro-bind"),
                shim_dir.clone().into(),
                tmp.path().join("bin").into(),
            ]
        );

        // The alias resolved through the qualified entry to a wrapper; both
        // export the SNAP* environment and exec the real command.
        for name in ["tofu", "opentofu.tofu"] {
            let script = fs::read_to_string(shim_dir.join(name)).unwrap();
            assert!(script.starts_with("#!/bin/sh\n"), "{script}");
            assert!(script.contains(&format!(
                "export SNAP=\"{}\"\n",
                tmp.path().join("opentofu/current").display()
            )));
            assert!(script.contains("export SNAP_NAME=\"opentofu\"\n"));
            assert!(script.contains("export SNAP_REVISION=\"252\"\n"));
            assert!(script.contains("export TOFU_FLAVOR=\"firm\"\n"));
            assert!(script.contains("export PATH=\"$SNAP/bin:$PATH\"\n"));
            // Keys that are not sh identifiers cannot be exported.
            assert!(!script.contains("not-an-identifier"), "{script}");
            assert!(script.ends_with("exec \"$SNAP/tofu\" \"$@\"\n"), "{script}");
            assert!(is_executable(&shim_dir.join(name)));
        }

        // The strict snap's dispatcher symlink is replicated verbatim.
        assert_eq!(fs::read_link(shim_dir.join("strictly")).unwrap(), snaps.dispatcher());
    }

    #[test]
    fn non_dispatcher_entries_are_replicated_or_copied() {
        let tmp = tempfile::tempdir().unwrap();
        let snaps = FakeSnaps::new(tmp.path());
        // A wrapper must resolve for the overmount to happen at all.
        snaps.snap("opentofu", "252", CLASSIC_YAML, &["tofu"]);
        snaps.entry("opentofu.tofu");
        let bin = tmp.path().join("bin");
        // Hand-placed symlink past the dispatcher, a dangling symlink, and a
        // plain executable sitting directly in the snap bin directory.
        std::os::unix::fs::symlink("/bin/true", bin.join("hand-placed")).unwrap();
        std::os::unix::fs::symlink("/nonexistent-target", bin.join("dangling")).unwrap();
        fs::write(bin.join("plain"), "#!/bin/sh\necho plain\n").unwrap();
        fs::set_permissions(bin.join("plain"), fs::Permissions::from_mode(0o755)).unwrap();

        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        assert!(!shim_args(&state, tmp.path()).unwrap().is_empty());
        let shim_dir = state.join("snap-bin");
        assert_eq!(fs::read_link(shim_dir.join("hand-placed")).unwrap(), Path::new("/bin/true"));
        assert_eq!(
            fs::read_link(shim_dir.join("dangling")).unwrap(),
            Path::new("/nonexistent-target")
        );
        assert_eq!(fs::read_to_string(shim_dir.join("plain")).unwrap(), "#!/bin/sh\necho plain\n");
        assert!(is_executable(&shim_dir.join("plain")));
    }

    #[test]
    fn command_with_snap_prefix_and_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let snaps = FakeSnaps::new(tmp.path());
        snaps.snap(
            "codium",
            "495",
            "name: codium\nconfinement: classic\napps:\n  codium:\n    command: electron-launch $SNAP/usr/share/codium/bin/codium --no-sandbox\n",
            &["electron-launch"],
        );
        snaps.entry("codium");

        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        shim_args(&state, tmp.path()).unwrap();
        let script = fs::read_to_string(state.join("snap-bin/codium")).unwrap();
        assert!(
            script.ends_with(
                "exec \"$SNAP/electron-launch\" \"$SNAP/usr/share/codium/bin/codium\" \"--no-sandbox\" \"$@\"\n"
            ),
            "{script}"
        );
    }

    #[test]
    fn missing_command_or_binary_leaves_the_dispatcher_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let snaps = FakeSnaps::new(tmp.path());
        // Classic, but the declared command does not exist on disk.
        snaps.snap(
            "ghost",
            "1",
            "name: ghost\nconfinement: classic\napps:\n  ghost:\n    command: bin/ghost\n",
            &[],
        );
        snaps.entry("ghost");
        // Dispatcher entry with no snap directory at all.
        snaps.entry("vanished");

        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        // No wrapper resolved: no overmount, only replicas.
        assert!(shim_args(&state, tmp.path()).unwrap().is_empty());
        for name in ["ghost", "vanished"] {
            let replica = state.join("snap-bin").join(name);
            assert_eq!(fs::read_link(replica).unwrap(), snaps.dispatcher());
        }
    }

    #[test]
    fn refresh_replaces_and_removes_stale_entries_without_wiping() {
        let tmp = tempfile::tempdir().unwrap();
        let snaps = FakeSnaps::new(tmp.path());
        snaps.snap("opentofu", "252", CLASSIC_YAML, &["tofu"]);
        snaps.entry("opentofu.tofu");
        snaps.alias("tofu", "opentofu.tofu");

        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        shim_args(&state, tmp.path()).unwrap();
        let shim_dir = state.join("snap-bin");
        // A leftover from a snap that has since been removed.
        fs::write(shim_dir.join("gone"), "#!/bin/sh\n").unwrap();

        // Uninstall: the qualified entry and the alias disappear.
        fs::remove_file(tmp.path().join("bin/tofu")).unwrap();
        let args = shim_args(&state, tmp.path()).unwrap();
        assert!(!args.is_empty());
        assert!(!shim_dir.join("gone").exists());
        assert!(!shim_dir.join("tofu").exists());
        assert!(shim_dir.join("opentofu.tofu").exists());
    }

    #[test]
    fn no_snap_bin_directory_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        assert!(shim_args(&state, &tmp.path().join("nope")).unwrap().is_empty());
        assert!(!state.join("snap-bin").exists());
    }

    #[test]
    fn sh_dq_escapes_for_double_quotes_but_keeps_dollar() {
        assert_eq!(sh_dq(r#"a"b\c`d$e"#), r#"a\"b\\c\`d$e"#);
    }
}
