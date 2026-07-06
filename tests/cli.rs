//! End-to-end tests against the compiled binary, with a scratch HOME, stub
//! tool binaries, and a fake `bwrap` that prints the mount args it received
//! and then executes the inner command directly (no real namespace — user
//! namespaces are frequently unavailable in CI/sandboxes, and the mount args
//! themselves are what we assert on).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE_BWRAP: &str = r#"#!/usr/bin/env bash
echo "BWRAP-ARGS-BEGIN"
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    --dev-bind|--bind|--ro-bind) echo "  $1 $2 -> $3"; shift 3 ;;
    --tmpfs) echo "  --tmpfs $2"; shift 2 ;;
    --setenv) echo "  --setenv $2=$3"; export "$2=$3"; shift 3 ;;
    *) echo "  OTHER: $1"; shift ;;
  esac
done
echo "BWRAP-ARGS-END"
exec "$@"
"#;

// Rewrites ~/.claude.json via atomic rename, like the real claude.
const FAKE_CLAUDE: &str = r#"#!/usr/bin/env bash
echo "ARGS: $*"
echo "CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR:-unset}"
echo "GIT_SSH_COMMAND=${GIT_SSH_COMMAND:-unset}"
cat "$HOME/.claude.json" 2>/dev/null || echo "no claude.json"
echo '{"rewritten":true}' > "$HOME/.claude.json.tmp"
mv "$HOME/.claude.json.tmp" "$HOME/.claude.json"
echo '{"backup":true}' > "$HOME/.claude.json.backup"
"#;

const FAKE_OPENCODE: &str = r#"#!/usr/bin/env bash
echo "ARGS: $*"
[ -e "$HOME/.claude.json" ] && echo "SAW-CLAUDE-JSON" || echo "no claude.json (good)"
exit 7
"#;

struct Harness {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    fake_bin: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let fake_bin = tmp.path().join("fake-bin");
        fs::create_dir_all(home.join("docs")).unwrap();
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(home.join(".ssh/config"), "Host *\n").unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        let h = Harness { _tmp: tmp, home, state, fake_bin };
        h.script("bwrap", FAKE_BWRAP);
        h.script("claude", FAKE_CLAUDE);
        h.script("opencode", FAKE_OPENCODE);
        // Deterministic "no processes running", whatever is live on the host.
        h.script("pgrep", "#!/usr/bin/env bash\nexit 1\n");
        h
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.fake_bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn mittens(&self, args: &[&str]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.fake_bin.display());
        Command::new(env!("CARGO_BIN_EXE_mittens"))
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("MITTENS_STATE_DIR", &self.state)
            .env("MITTENS_CLAUDE_BIN", self.fake_bin.join("claude"))
            .env("MITTENS_OPENCODE_BIN", self.fake_bin.join("opencode"))
            .env("MITTENS_AGENTS_DIR", self.home.join(".config/agents"))
            .output()
            .unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_no_stray(home: &Path, dot: &str) {
    // The fake bwrap doesn't isolate, but the stubs only write the sync file
    // (cleaned by inspection) — the dot dir must never appear in real home.
    assert!(!home.join(dot).exists(), "{dot} leaked into the real home");
}

#[test]
fn claude_wrapped_run_syncs_and_passes_args_through() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".config/agents/agents")).unwrap();
    fs::write(h.home.join(".config/agents/AGENTS.md"), "# memory\n").unwrap();

    let out = h.mittens(&["hello", "--flag", "config"]);
    let text = stdout(&out);
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    // Mount args: home entries bound back, dot-claude bound over ~/.claude,
    // shared agent config wired, ssh workaround set.
    let home = h.home.display().to_string();
    assert!(text.contains(&format!("--tmpfs {home}")));
    assert!(text.contains(&format!("--dev-bind {home}/docs -> {home}/docs")));
    assert!(text.contains(&format!(
        "--bind {}/dot-claude -> {home}/.claude",
        h.state.display()
    )));
    assert!(text.contains(&format!(
        "--bind {home}/.config/agents/agents -> {home}/.claude/agents"
    )));
    assert!(text.contains(&format!(
        "--ro-bind {home}/.config/agents/AGENTS.md -> {home}/.claude/CLAUDE.md"
    )));
    assert!(text.contains("--setenv GIT_SSH_COMMAND=ssh -F "));

    // Passthrough of arbitrary args, including flag-looking ones.
    assert!(text.contains("ARGS: hello --flag config"));
    // Fresh state was seeded with an empty JSON object and the tool's
    // rename-rewrite plus backup were synced back out.
    assert!(text.contains("{}"));
    assert_eq!(fs::read_to_string(h.state.join("claude.json")).unwrap(), "{\"rewritten\":true}\n");
    assert_eq!(
        fs::read_to_string(h.state.join(".claude.json.backup")).unwrap(),
        "{\"backup\":true}\n"
    );
    // No wrapped-mode CLAUDE_CONFIG_DIR.
    assert!(text.contains("CLAUDE_CONFIG_DIR=unset"));

    // Cleanup of what the un-namespaced stub wrote to the scratch home; the
    // dot dir itself must not have appeared.
    assert_no_stray(&h.home, ".claude");
}

#[test]
fn opencode_wrapped_run_binds_dot_opencode_only_and_propagates_exit() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".config/agents/agents")).unwrap();

    let out = h.mittens(&["opencode", "do-stuff", "-x"]);
    let text = stdout(&out);

    let home = h.home.display().to_string();
    assert!(text.contains(&format!(
        "--bind {}/dot-opencode -> {home}/.opencode",
        h.state.display()
    )));
    // No shared-agent wiring and no sync file for opencode.
    assert!(!text.contains("CLAUDE.md"));
    assert!(!text.contains(".claude/agents"));
    assert!(text.contains("no claude.json (good)"));
    assert!(text.contains("ARGS: do-stuff -x"));
    // The stub exits 7; mittens must propagate it.
    assert_eq!(out.status.code(), Some(7));
    assert!(!h.state.join("claude.json").exists());
}

#[test]
fn guard_refuses_stray_home_data() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".claude")).unwrap();
    fs::write(h.home.join(".claude.json.backup"), "{}").unwrap();

    let out = h.mittens(&["hello"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1));
    assert!(err.contains("refusing to start: found ~/.claude ~/.claude.json.backup in the real home"));
    assert!(err.contains("run: mittens claude --migrate"));

    // Populated state directory changes the advice.
    fs::create_dir_all(h.state.join("dot-claude")).unwrap();
    fs::write(h.state.join("dot-claude/settings.json"), "{}").unwrap();
    let out = h.mittens(&["hello"]);
    assert!(stderr(&out).contains("state directory"));
    assert!(stderr(&out).contains("leftover from an unwrapped claude run"));
}

#[test]
fn migrate_moves_dot_dir_sync_file_and_backups() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".claude/projects")).unwrap();
    fs::write(h.home.join(".claude.json"), "{\"real\":1}").unwrap();
    fs::write(h.home.join(".claude.json.backup"), "{\"old\":1}").unwrap();

    let out = h.mittens(&["--migrate"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("moved ~/.claude ->"));
    assert!(text.contains("moved ~/.claude.json ->"));
    assert!(text.contains("migration complete"));
    assert!(h.state.join("dot-claude/projects").is_dir());
    assert_eq!(fs::read_to_string(h.state.join("claude.json")).unwrap(), "{\"real\":1}");
    assert_eq!(fs::read_to_string(h.state.join(".claude.json.backup")).unwrap(), "{\"old\":1}");
    assert!(!h.home.join(".claude").exists());
    assert!(!h.home.join(".claude.json").exists());

    // A second migrate with a repopulated home refuses to clobber state.
    fs::create_dir_all(h.home.join(".claude")).unwrap();
    let out = h.mittens(&["--migrate"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("refusing to migrate"));
}

#[test]
fn migrate_refuses_while_tool_runs() {
    let h = Harness::new();
    h.script("pgrep", "#!/usr/bin/env bash\necho 4242\n");
    let out = h.mittens(&["opencode", "--migrate"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("opencode processes are running"));
}

#[test]
fn opencode_migrate_moves_dot_dir() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".opencode/node_modules")).unwrap();

    let out = h.mittens(&["opencode", "--migrate"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(h.state.join("dot-opencode/node_modules").is_dir());
    assert!(!h.home.join(".opencode").exists());
}

#[test]
fn claude_unsafe_sets_config_dir_and_seeds_top_level_config() {
    let h = Harness::new();
    fs::create_dir_all(&h.state).unwrap();
    fs::write(h.state.join("claude.json"), "{\"wrapped\":1}").unwrap();

    let out = h.mittens(&["--unsafe", "hi"]);
    let text = stdout(&out);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(text.contains(&format!("CLAUDE_CONFIG_DIR={}/dot-claude", h.state.display())));
    assert!(text.contains("ARGS: hi"));
    // Seeded once from the wrapped copy.
    assert_eq!(
        fs::read_to_string(h.state.join("dot-claude/.claude.json")).unwrap(),
        "{\"wrapped\":1}"
    );
}

#[test]
fn opencode_unsafe_is_refused() {
    let h = Harness::new();
    let out = h.mittens(&["opencode", "--unsafe"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("--unsafe is not supported for opencode"));
}

#[test]
fn help_shows_service_specific_values() {
    let h = Harness::new();
    let out = h.mittens(&["--help"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("Service:             claude"));

    let out = h.mittens(&["opencode", "-h"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("Service:             opencode"));
    assert!(text.contains(&h.state.display().to_string()));
}

#[test]
fn missing_bwrap_is_reported() {
    let h = Harness::new();
    fs::remove_file(h.fake_bin.join("bwrap")).unwrap();
    // PATH must not include the real bwrap's directory for this one.
    let out = Command::new(env!("CARGO_BIN_EXE_mittens"))
        .arg("hello")
        .env_clear()
        .env("HOME", &h.home)
        .env("PATH", &h.fake_bin)
        .env("MITTENS_CLAUDE_BIN", h.fake_bin.join("claude"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("bwrap (bubblewrap) is not installed"));
}

#[test]
fn missing_tool_binary_is_reported() {
    let h = Harness::new();
    fs::remove_file(h.fake_bin.join("claude")).unwrap();
    let out = h.mittens(&["hello"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("claude binary not found at"));
}
