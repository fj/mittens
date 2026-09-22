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

const FAKE_PI: &str = r#"#!/usr/bin/env bash
echo "ARGS: $*"
echo "PI_CODING_AGENT_DIR=${PI_CODING_AGENT_DIR:-unset}"
"#;

struct Harness {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    fake_bin: PathBuf,
    // Stands in for /snap; not created, so runs are snap-free unless a test
    // populates it.
    snap_root: PathBuf,
    // Stands in for /etc/ssh, so the host's real one never leaks into a test.
    // Populated like a stock Debian install: a system config including a
    // drop-in directory that holds one file.
    etc_ssh: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let fake_bin = tmp.path().join("fake-bin");
        let snap_root = tmp.path().join("snap-root");
        let etc_ssh = tmp.path().join("etc-ssh");
        fs::create_dir_all(home.join("docs")).unwrap();
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(home.join(".ssh/config"), "Host *\n").unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(etc_ssh.join("ssh_config.d")).unwrap();
        fs::write(
            etc_ssh.join("ssh_config"),
            "Include ssh_config.d/*.conf\nHost *\n",
        )
        .unwrap();
        fs::write(etc_ssh.join("ssh_config.d/20-proxy.conf"), "Host .host\n").unwrap();
        let h = Harness {
            _tmp: tmp,
            home,
            state,
            fake_bin,
            snap_root,
            etc_ssh,
        };
        h.script("bwrap", FAKE_BWRAP);
        h.script("claude", FAKE_CLAUDE);
        h.script("opencode", FAKE_OPENCODE);
        h.script("pi", FAKE_PI);
        // Deterministic "no processes running", whatever is live on the host.
        h.script("pgrep", "#!/usr/bin/env bash\nexit 1\n");
        h
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.fake_bin.join(name);
        executable(&path, body);
        path
    }

    fn mittens(&self, args: &[&str]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.fake_bin.display());
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mittens"));
        cmd.args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("MITTENS_STATE_DIR", &self.state)
            .env("MITTENS_CLAUDE_BIN", self.fake_bin.join("claude"))
            .env("MITTENS_OPENCODE_BIN", self.fake_bin.join("opencode"))
            .env("MITTENS_PI_BIN", self.fake_bin.join("pi"))
            .env("MITTENS_AGENTS_DIR", self.home.join(".config/agents"))
            .env("MITTENS_SNAP_ROOT", &self.snap_root)
            .env("MITTENS_ETC_SSH", &self.etc_ssh)
            // Run the countdown/pause paths instantly.
            .env("MITTENS_DELAY_SECS", "0");
        run(&mut cmd)
    }
}

/// Guards the moment a test executable has a writer. `fs::write` holds a write
/// fd, a concurrently forked child inherits it until its own exec, and exec'ing
/// a file some process holds open for writing fails with ETXTBSY — so a test
/// creating its stubs would sporadically break an unrelated test's launch.
/// Writers take this exclusively and spawners share it, so no fork is ever in
/// flight while a write fd is open.
static EXECUTABLES: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Fork under the shared side of that lock. Every spawn in this file goes
/// through here; one that does not reopens the race for everybody.
fn run(cmd: &mut Command) -> Output {
    let _spawning = EXECUTABLES.read().unwrap_or_else(|e| e.into_inner());
    cmd.output().unwrap()
}

/// Create a runnable file under the exclusive side of that lock. Every
/// executable this file writes goes through here, for the same reason.
fn executable(path: &Path, body: &str) {
    let _writing = EXECUTABLES.write().unwrap_or_else(|e| e.into_inner());
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
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

    let out = h.mittens(&["harness:claude", "hello", "--flag", "config"]);
    let text = stdout(&out);
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    // Mount args: home entries bound back, dot-claude bound over ~/.claude,
    // shared agent config wired, ssh drop-ins replaced by their replicas.
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
    // The drop-in the system-wide ssh config includes is overmounted with a
    // replica owned by the invoking user; the original is root-owned on a real
    // system and OpenSSH aborts on its nobody:nogroup owner inside the
    // namespace. The replica is byte-identical.
    let drop_in = h.etc_ssh.join("ssh_config.d/20-proxy.conf");
    let replica = h
        .state
        .join("ssh-config")
        .join(drop_in.strip_prefix("/").unwrap());
    assert!(text.contains(&format!(
        "--ro-bind {} -> {}",
        replica.display(),
        drop_in.display()
    )));
    assert_eq!(
        fs::read_to_string(&replica).unwrap(),
        fs::read_to_string(&drop_in).unwrap()
    );
    // The system-wide file itself is read without an ownership check, so it is
    // not overmounted (matched whole-line: it is a path prefix of the drop-in).
    let system_config = format!("-> {}", h.etc_ssh.join("ssh_config").display());
    assert!(!text.lines().any(|line| line.ends_with(&system_config)));

    // Passthrough of arbitrary args, including flag-looking ones.
    assert!(text.contains("ARGS: hello --flag config"));
    // Fresh state was seeded with an empty JSON object and the tool's
    // rename-rewrite plus backup were synced back out.
    assert!(text.contains("{}"));
    assert_eq!(
        fs::read_to_string(h.state.join("claude.json")).unwrap(),
        "{\"rewritten\":true}\n"
    );
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

    let out = h.mittens(&["harness:opencode", "do-stuff", "-x"]);
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
fn pi_wrapped_run_binds_dot_pi_and_wires_the_agent_config() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".config/agents/skills")).unwrap();
    fs::create_dir_all(h.home.join(".config/agents/prompts")).unwrap();
    fs::write(h.home.join(".config/agents/AGENTS.md"), "# memory\n").unwrap();

    let out = h.mittens(&["harness:pi", "chat", "-p"]);
    let text = stdout(&out);
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    let home = h.home.display().to_string();
    assert!(text.contains(&format!(
        "--bind {}/dot-pi -> {home}/.pi",
        h.state.display()
    )));
    // The shared config lands in pi's global config directory, ~/.pi/agent.
    assert!(text.contains(&format!(
        "--bind {home}/.config/agents/skills -> {home}/.pi/agent/skills"
    )));
    assert!(text.contains(&format!(
        "--bind {home}/.config/agents/prompts -> {home}/.pi/agent/prompts"
    )));
    assert!(text.contains(&format!(
        "--ro-bind {home}/.config/agents/AGENTS.md -> {home}/.pi/agent/AGENTS.md"
    )));
    // Nothing is copy-synced, and wrapped runs leave the variable alone.
    assert!(text.contains("PI_CODING_AGENT_DIR=unset"));
    assert!(text.contains("ARGS: chat -p"));
    assert_no_stray(&h.home, ".pi");
}

#[test]
fn pi_unsafe_points_the_agent_dir_at_the_wrapped_one() {
    let h = Harness::new();

    let out = h.mittens(&["harness:pi", "--unsafe", "hi"]);
    let text = stdout(&out);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // One level into the state dir: PI_CODING_AGENT_DIR relocates ~/.pi/agent,
    // so unsandboxed runs must land on the same files wrapped runs write.
    assert!(
        text.contains(&format!(
            "PI_CODING_AGENT_DIR={}/dot-pi/agent",
            h.state.display()
        )),
        "stdout: {text}"
    );
    assert!(text.contains("ARGS: hi"));
}

#[test]
fn pi_migrate_moves_dot_dir() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".pi/agent/sessions")).unwrap();

    let out = h.mittens(&["harness:pi", "--migrate"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(h.state.join("dot-pi/agent/sessions").is_dir());
    assert!(!h.home.join(".pi").exists());
}

#[test]
fn pi_launch_relocates_a_stray_dot_pi_into_the_state_dir() {
    let h = Harness::new();
    // Everything pi keeps sits one level down, in ~/.pi/agent.
    fs::create_dir_all(h.home.join(".pi/agent/sessions")).unwrap();
    fs::write(h.home.join(".pi/agent/auth.json"), "creds").unwrap();
    // An earlier wrapped run left the agent directory behind, so both levels
    // merge instead of moving whole.
    fs::create_dir_all(h.state.join("dot-pi/agent")).unwrap();
    fs::write(h.state.join("dot-pi/agent/settings.json"), "wrapped").unwrap();

    let out = h.mittens(&["harness:pi", "chat"]);
    let err = stderr(&out);
    assert!(out.status.success(), "stderr: {err}");

    let agent = h.state.join("dot-pi/agent").display().to_string();
    for line in [
        "found ~/.pi in the real home",
        &format!("moved ~/.pi/agent/auth.json -> {agent}/auth.json"),
        &format!("moved ~/.pi/agent/sessions/ -> {agent}/sessions/"),
        "removed empty ~/.pi/agent/",
        "removed empty ~/.pi/",
        "mittens: starting pi…",
    ] {
        assert!(err.contains(line), "missing {line:?} in stderr: {err}");
    }
    assert_eq!(
        fs::read_to_string(h.state.join("dot-pi/agent/settings.json")).unwrap(),
        "wrapped"
    );
    assert!(stdout(&out).contains("ARGS: chat"));
    assert_no_stray(&h.home, ".pi");
}

#[test]
fn launch_relocates_stray_home_data_verbosely_then_starts() {
    let h = Harness::new();
    // Data stranded in the real home by an unwrapped run...
    fs::create_dir_all(h.home.join(".claude/projects")).unwrap();
    fs::write(h.home.join(".claude/settings.json"), "real").unwrap();
    fs::write(h.home.join(".claude.json"), "{\"real\":1}").unwrap();
    fs::write(h.home.join(".claude.json.backup"), "{}").unwrap();
    // ...on top of a state directory wrapped runs already populated.
    fs::create_dir_all(h.state.join("dot-claude")).unwrap();
    fs::write(h.state.join("dot-claude/settings.json"), "wrapped").unwrap();
    fs::write(h.state.join("dot-claude/plugins.json"), "wrapped").unwrap();

    let out = h.mittens(&["harness:claude", "hello"]);
    let err = stderr(&out);
    assert!(out.status.success(), "stderr: {err}");

    // Every stray path is named up front, then every move it took is reported:
    // this moves the user's data unasked, so it has to say what it did.
    let dot = h.state.join("dot-claude").display().to_string();
    for line in [
        "found ~/.claude ~/.claude.json ~/.claude.json.backup in the real home",
        &format!("moved ~/.claude/projects/ -> {dot}/projects/"),
        &format!("moved ~/.claude/settings.json -> {dot}/settings.json"),
        "removed empty ~/.claude/",
        &format!("moved ~/.claude.json -> {}/claude.json", h.state.display()),
        &format!(
            "moved ~/.claude.json.backup -> {}/.claude.json.backup",
            h.state.display()
        ),
        "mittens: starting claude…",
    ] {
        assert!(err.contains(line), "missing {line:?} in stderr: {err}");
    }

    // The real-home copy won the collision; state-dir data outside the overlap
    // survived rather than being replaced wholesale.
    assert_eq!(
        fs::read_to_string(h.state.join("dot-claude/settings.json")).unwrap(),
        "real"
    );
    assert_eq!(
        fs::read_to_string(h.state.join("dot-claude/plugins.json")).unwrap(),
        "wrapped"
    );
    assert!(h.state.join("dot-claude/projects").is_dir());

    // The tool then launched and read the relocated config, not a fresh one.
    let text = stdout(&out);
    assert!(text.contains("ARGS: hello"));
    assert!(text.contains("{\"real\":1}"), "stdout: {text}");
    assert_no_stray(&h.home, ".claude");
}

#[test]
fn unsafe_launch_relocates_before_exec_rather_than_letting_cleanup_delete() {
    let h = Harness::new();
    // An unwrapped run's data, sitting where the post-run cleanup deletes from:
    // it has to be moved to the state dir *before* the tool starts, or it is
    // destroyed rather than adopted.
    fs::create_dir_all(h.home.join(".claude")).unwrap();
    fs::write(h.home.join(".claude/credentials.json"), "the only copy").unwrap();
    fs::write(h.home.join(".claude.json"), "{\"real\":1}").unwrap();

    let out = h.mittens(&["harness:claude", "--unsafe", "hi"]);
    let err = stderr(&out);
    assert!(out.status.success(), "stderr: {err}");
    assert!(
        err.contains("found ~/.claude ~/.claude.json in the real home"),
        "stderr: {err}"
    );

    assert_eq!(
        fs::read_to_string(h.state.join("dot-claude/credentials.json")).unwrap(),
        "the only copy"
    );
    assert_eq!(
        fs::read_to_string(h.state.join("claude.json")).unwrap(),
        "{\"real\":1}"
    );
    // The relocated config is what seeded the unsafe run's own copy.
    assert_eq!(
        fs::read_to_string(h.state.join("dot-claude/.claude.json")).unwrap(),
        "{\"real\":1}"
    );
    assert!(stdout(&out).contains("ARGS: hi"));
    // What the stub then wrote to the real home is cleaned up as before.
    assert_no_stray(&h.home, ".claude");
    assert!(!h.home.join(".claude.json").exists());
}

#[test]
fn a_failed_relocation_aborts_the_launch() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".claude")).unwrap();
    fs::write(h.home.join(".claude/settings.json"), "keep me").unwrap();
    // A state directory nested inside the dot directory it has to receive: the
    // move is a rename of a directory into its own subtree, which the kernel
    // refuses. Every other step of the launch would succeed, so only the
    // relocation's failure can stop it here.
    let state = h.home.join(".claude/state");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mittens"));
    cmd.args(["harness:claude", "hello"])
        .env_clear()
        .env("HOME", &h.home)
        .env("PATH", format!("{}:/usr/bin:/bin", h.fake_bin.display()))
        .env("MITTENS_STATE_DIR", &state)
        .env("MITTENS_CLAUDE_BIN", h.fake_bin.join("claude"))
        .env("MITTENS_AGENTS_DIR", h.home.join(".config/agents"))
        .env("MITTENS_SNAP_ROOT", &h.snap_root)
        .env("MITTENS_ETC_SSH", &h.etc_ssh);
    let out = run(&mut cmd);

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "stderr: {err}");
    assert!(err.contains("moving"), "stderr: {err}");
    // The tool must not start over data still sitting in the real home, where
    // the namespace would shadow it.
    assert!(!stdout(&out).contains("ARGS:"), "stdout: {}", stdout(&out));
    assert_eq!(
        fs::read_to_string(h.home.join(".claude/settings.json")).unwrap(),
        "keep me"
    );
}

#[test]
fn launch_relocates_even_while_the_tool_is_running() {
    let h = Harness::new();
    // Unlike --migrate, a launch does not refuse: the harness's name on the
    // process list cannot tell a wrapped session (which never reads the real
    // home) from an unwrapped one.
    h.script("pgrep", "#!/usr/bin/env bash\necho 4242\n");
    fs::create_dir_all(h.home.join(".pi/agent")).unwrap();
    fs::write(h.home.join(".pi/agent/auth.json"), "creds").unwrap();

    let out = h.mittens(&["harness:pi", "chat"]);
    let err = stderr(&out);
    assert!(out.status.success(), "stderr: {err}");
    assert!(
        err.contains("found ~/.pi in the real home"),
        "stderr: {err}"
    );
    assert_eq!(
        fs::read_to_string(h.state.join("dot-pi/agent/auth.json")).unwrap(),
        "creds"
    );
    assert!(stdout(&out).contains("ARGS: chat"));
}

#[test]
fn launch_with_a_clean_home_says_nothing_about_relocation() {
    let h = Harness::new();
    let out = h.mittens(&["harness:claude", "hi"]);
    let err = stderr(&out);
    assert!(out.status.success(), "stderr: {err}");
    assert!(!err.contains("real home"), "stderr: {err}");
    assert!(!err.contains("moved"), "stderr: {err}");
    assert!(stdout(&out).contains("ARGS: hi"));
}

#[test]
fn migrate_moves_dot_dir_sync_file_and_backups() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".claude/projects")).unwrap();
    fs::write(h.home.join(".claude.json"), "{\"real\":1}").unwrap();
    fs::write(h.home.join(".claude.json.backup"), "{\"old\":1}").unwrap();

    let out = h.mittens(&["harness:claude", "--migrate"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("moved ~/.claude/ ->"));
    assert!(text.contains("moved ~/.claude.json ->"));
    assert!(text.contains("migration complete"));
    assert!(h.state.join("dot-claude/projects").is_dir());
    assert_eq!(
        fs::read_to_string(h.state.join("claude.json")).unwrap(),
        "{\"real\":1}"
    );
    assert_eq!(
        fs::read_to_string(h.state.join(".claude.json.backup")).unwrap(),
        "{\"old\":1}"
    );
    assert!(!h.home.join(".claude").exists());
    assert!(!h.home.join(".claude.json").exists());

    // A second migrate merges into the now-populated state dir instead of
    // refusing; the real-home copy is the later write and takes the collision.
    fs::create_dir_all(h.home.join(".claude")).unwrap();
    fs::write(h.home.join(".claude/settings.json"), "newer").unwrap();
    let out = h.mittens(&["harness:claude", "--migrate"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        fs::read_to_string(h.state.join("dot-claude/settings.json")).unwrap(),
        "newer"
    );
    assert!(h.state.join("dot-claude/projects").is_dir());
    assert!(!h.home.join(".claude").exists());
}

#[test]
fn migrate_with_a_clean_home_reports_nothing_to_do() {
    let h = Harness::new();
    let out = h.mittens(&["harness:pi", "--migrate"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("nothing to migrate: no pi data in the real home (~/.pi)"));
}

#[test]
fn migrate_moves_a_dangling_symlink() {
    // The guard trips on a dangling ~/.claude symlink, so migrate must move
    // it too instead of reporting success while the guard keeps refusing.
    let h = Harness::new();
    std::os::unix::fs::symlink("/nonexistent", h.home.join(".claude")).unwrap();

    let out = h.mittens(&["harness:claude", "--migrate"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("moved ~/.claude ->"));
    // The symlink itself moved: the guard has nothing left to trip on.
    assert!(h.home.join(".claude").symlink_metadata().is_err());
    assert!(
        h.state
            .join("dot-claude")
            .symlink_metadata()
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn migrate_refuses_while_tool_runs() {
    let h = Harness::new();
    h.script("pgrep", "#!/usr/bin/env bash\necho 4242\n");
    let out = h.mittens(&["harness:opencode", "--migrate"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("opencode processes are running"));
}

#[test]
fn opencode_migrate_moves_dot_dir() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".opencode/node_modules")).unwrap();

    let out = h.mittens(&["harness:opencode", "--migrate"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(h.state.join("dot-opencode/node_modules").is_dir());
    assert!(!h.home.join(".opencode").exists());
}

#[test]
fn claude_unsafe_sets_config_dir_and_seeds_top_level_config() {
    let h = Harness::new();
    fs::create_dir_all(&h.state).unwrap();
    fs::write(h.state.join("claude.json"), "{\"wrapped\":1}").unwrap();

    let out = h.mittens(&["harness:claude", "--unsafe", "hi"]);
    let text = stdout(&out);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(text.contains(&format!(
        "CLAUDE_CONFIG_DIR={}/dot-claude",
        h.state.display()
    )));
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
    let out = h.mittens(&["harness:opencode", "--unsafe"]);
    assert_eq!(out.status.code(), Some(1));
    // The reason, not just the refusal: it is what tells the user no
    // environment variable can stand in for the sandbox here.
    assert!(stderr(&out).contains(
        "--unsafe is not supported for opencode: OPENCODE_CONFIG_DIR only \
         relocates config loading, nothing relocates ~/.opencode itself"
    ));
}

#[test]
fn help_shows_harness_specific_values() {
    let h = Harness::new();
    let out = h.mittens(&["harness:claude", "--help"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("Harness:             claude"));

    let out = h.mittens(&["harness:opencode", "-h"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("Harness:             opencode"));
    assert!(text.contains(&h.state.display().to_string()));

    // Help works without a harness too, minus the resolved paths.
    let out = h.mittens(&["--help"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("mittens harness:<name> [arguments...]"));
    assert!(text.contains("mittens self-update"));
    assert!(text.contains("https://github.com/fj/mittens"));
    assert!(!text.contains("State directory:"));
}

#[test]
fn missing_harness_is_rejected() {
    let h = Harness::new();
    // No arguments at all.
    let out = h.mittens(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("the first argument must select a harness"));

    // Arbitrary tool args no longer default to claude.
    let out = h.mittens(&["hello"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("the first argument must select a harness"));
    assert!(err.contains("claude, opencode, pi"));

    // A bare harness name gets a did-you-mean.
    let out = h.mittens(&["opencode", "run"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("did you mean \"mittens harness:opencode\"?"));

    // Flags without a harness are rejected too.
    let out = h.mittens(&["--migrate"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("the first argument must select a harness"));
}

#[test]
fn unknown_harness_is_rejected() {
    let h = Harness::new();
    let out = h.mittens(&["harness:emacs", "hello"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("unknown harness \"emacs\""));
    assert!(err.contains("claude, opencode, pi"));

    // An empty name is an unknown harness, not a passthrough.
    let out = h.mittens(&["harness:"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown harness \"\""));
}

#[test]
fn skip_pawmissions_wipes_stray_data_then_launches() {
    let h = Harness::new();
    fs::create_dir_all(h.home.join(".claude/projects")).unwrap();
    fs::write(h.home.join(".claude/settings.json"), "{}").unwrap();
    fs::write(h.home.join(".claude.json"), "{}").unwrap();
    fs::write(h.home.join(".claude.json.backup"), "{}").unwrap();

    let out = h.mittens(&["harness:claude", "--dangerously-skip-pawmissions", "hello"]);
    let err = stderr(&out);
    assert!(out.status.success(), "stderr: {err}");
    // Inspection listed all three targets, then deleted them.
    assert!(err.contains("slated for deletion"));
    assert_eq!(err.matches("mittens: wiped").count(), 3, "stderr: {err}");
    assert!(err.contains(".claude.json.backup"));
    // The dot dir stays gone (the fake bwrap doesn't isolate, so the stub
    // recreates ~/.claude.json — but it never touches ~/.claude).
    assert!(!h.home.join(".claude").exists());
    // The guard then passed and the tool actually launched, seeing the
    // freshly seeded (not the wiped) config.
    assert!(stdout(&out).contains("ARGS: hello"));
    assert!(stdout(&out).contains("{}"));
}

#[test]
fn skip_pawmissions_with_clean_home_is_a_noop_launch() {
    let h = Harness::new();
    let out = h.mittens(&["harness:claude", "--dangerously-skip-pawmissions", "hi"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("nothing to wipe"));
    assert!(stdout(&out).contains("ARGS: hi"));
}

#[test]
fn migrate_accepts_preexisting_empty_dot_state() {
    let h = Harness::new();
    fs::create_dir_all(h.state.join("dot-claude")).unwrap();
    fs::create_dir_all(h.home.join(".claude")).unwrap();
    fs::write(h.home.join(".claude/settings.json"), "{}").unwrap();

    let out = h.mittens(&["harness:claude", "--migrate"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(h.state.join("dot-claude/settings.json").is_file());
}

#[test]
fn signal_death_maps_to_128_plus_signal_and_still_syncs() {
    let h = Harness::new();
    h.script(
        "claude",
        r#"#!/usr/bin/env bash
echo '{"partial":true}' > "$HOME/.claude.json.tmp"
mv "$HOME/.claude.json.tmp" "$HOME/.claude.json"
kill -TERM $$
"#,
    );
    let out = h.mittens(&["harness:claude", "hello"]);
    assert_eq!(out.status.code(), Some(128 + 15));
    // sync-out still ran after the signal death.
    assert_eq!(
        fs::read_to_string(h.state.join("claude.json")).unwrap(),
        "{\"partial\":true}\n"
    );
}

#[test]
fn inner_rejects_malformed_argv() {
    let out = run(Command::new(env!("CARGO_BIN_EXE_mittens"))
        .args(["__inner", "only-one-arg"])
        .env_clear());
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("__inner expects"));
}

#[test]
fn claude_unsafe_cleans_up_stray_home_data_after_run() {
    let h = Harness::new();
    fs::create_dir_all(&h.state).unwrap();

    let out = h.mittens(&["harness:claude", "--unsafe", "hi"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // The fake claude stub writes ~/.claude.json* to the real home; mittens
    // must clean them up so the next run passes the startup guard.
    assert!(
        !h.home.join(".claude.json").exists(),
        "~/.claude.json not cleaned up"
    );
    assert!(
        !h.home.join(".claude.json.backup").exists(),
        "~/.claude.json.backup not cleaned up"
    );
}

#[test]
fn claude_unsafe_cleanup_removes_the_dot_dir_and_spares_lookalikes() {
    let h = Harness::new();
    fs::create_dir_all(&h.state).unwrap();
    // A tool that recreates its hardcoded dot directory despite the relocation
    // variable: the cleanup must take the directory, not just the sync files.
    h.script(
        "claude",
        r#"#!/usr/bin/env bash
mkdir -p "$HOME/.claude/projects"
echo '{}' > "$HOME/.claude/projects/session.json"
echo '{}' > "$HOME/.claude.json"
"#,
    );
    // Real-home entries that merely look similar must survive: the cleanup
    // matches the dot dir exactly and the sync file by prefix, nothing else.
    fs::write(h.home.join(".claude-notes.md"), "keep me").unwrap();
    fs::create_dir_all(h.home.join(".claude-backups")).unwrap();

    let out = h.mittens(&["harness:claude", "--unsafe", "hi"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(!h.home.join(".claude").exists(), "~/.claude not cleaned up");
    assert!(
        !h.home.join(".claude.json").exists(),
        "~/.claude.json not cleaned up"
    );
    assert_eq!(
        fs::read_to_string(h.home.join(".claude-notes.md")).unwrap(),
        "keep me"
    );
    assert!(
        h.home.join(".claude-backups").is_dir(),
        "~/.claude-backups was deleted"
    );
}

#[test]
fn claude_unsafe_signal_death_maps_to_128_plus_signal_and_still_cleans_up() {
    let h = Harness::new();
    fs::create_dir_all(&h.state).unwrap();
    h.script(
        "claude",
        r#"#!/usr/bin/env bash
echo '{"partial":true}' > "$HOME/.claude.json.tmp"
mv "$HOME/.claude.json.tmp" "$HOME/.claude.json"
kill -TERM $$
"#,
    );
    let out = h.mittens(&["harness:claude", "--unsafe", "hello"]);
    assert_eq!(out.status.code(), Some(128 + 15));
    // cleanup_after_unsafe still ran after the signal death.
    assert!(
        !h.home.join(".claude.json").exists(),
        "~/.claude.json not cleaned up after signal"
    );
}

#[test]
fn claude_unsafe_does_not_reseed_on_later_runs() {
    let h = Harness::new();
    fs::create_dir_all(&h.state).unwrap();
    fs::write(h.state.join("claude.json"), "{\"wrapped\":1}").unwrap();

    let out = h.mittens(&["harness:claude", "--unsafe", "first"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // Stray ~/.claude.json* are cleaned up automatically after the run; the
    // second invocation finds a clean home and passes the guard without any
    // manual intervention.

    // The unsafe copy evolves independently after the one-time seeding.
    fs::write(h.state.join("dot-claude/.claude.json"), "{\"diverged\":1}").unwrap();
    let out = h.mittens(&["harness:claude", "--unsafe", "second"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        fs::read_to_string(h.state.join("dot-claude/.claude.json")).unwrap(),
        "{\"diverged\":1}"
    );
}

#[test]
fn self_update_reinstalls_over_its_own_cargo_root() {
    let h = Harness::new();
    h.script("cargo", "#!/usr/bin/env bash\necho \"CARGO-ARGS: $*\"\n");
    // Run a copy from a cargo-style <root>/bin layout, like `cargo install
    // --root` produces: self-update must target that same root.
    let root = h.home.join("tools");
    fs::create_dir_all(root.join("bin")).unwrap();
    let copy = root.join("bin/mittens");
    {
        let _writing = EXECUTABLES.write().unwrap_or_else(|e| e.into_inner());
        fs::copy(env!("CARGO_BIN_EXE_mittens"), &copy).unwrap();
    }

    let out = run(Command::new(&copy)
        .arg("self-update")
        .env_clear()
        .env("HOME", &h.home)
        .env("PATH", format!("{}:/usr/bin:/bin", h.fake_bin.display())));
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(
        "CARGO-ARGS: install --locked --force --git https://github.com/fj/mittens mittens"
    ));
    assert!(text.contains(&format!("--root {}", root.display())));
}

#[test]
fn self_update_outside_a_cargo_root_uses_the_default() {
    let h = Harness::new();
    h.script("cargo", "#!/usr/bin/env bash\necho \"CARGO-ARGS: $*\"\n");
    // The test binary lives in target/debug, not a <root>/bin directory, so
    // no --root is derived and cargo's own default applies.
    let out = h.mittens(&["self-update"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(
        "CARGO-ARGS: install --locked --force --git https://github.com/fj/mittens mittens"
    ));
    assert!(!text.contains("--root"));
}

#[test]
fn self_update_is_first_argument_only_and_takes_none() {
    let h = Harness::new();
    // After a harness selector it is an ordinary tool argument.
    let out = h.mittens(&["harness:claude", "self-update"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("ARGS: self-update"));

    // Trailing arguments are rejected rather than silently dropped.
    let out = h.mittens(&["self-update", "--force"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("self-update takes no arguments"));
}

#[test]
fn self_update_without_cargo_is_reported() {
    let h = Harness::new();
    // PATH must not include the real cargo's directory for this one.
    let out = run(Command::new(env!("CARGO_BIN_EXE_mittens"))
        .arg("self-update")
        .env_clear()
        .env("HOME", &h.home)
        .env("PATH", &h.fake_bin));
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("exec cargo install (is cargo installed?)"));
}

#[test]
fn missing_bwrap_is_reported() {
    let h = Harness::new();
    fs::remove_file(h.fake_bin.join("bwrap")).unwrap();
    // PATH must not include the real bwrap's directory for this one.
    let out = run(Command::new(env!("CARGO_BIN_EXE_mittens"))
        .args(["harness:claude", "hello"])
        .env_clear()
        .env("HOME", &h.home)
        .env("PATH", &h.fake_bin)
        .env("MITTENS_CLAUDE_BIN", h.fake_bin.join("claude")));
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("bwrap (bubblewrap) is not installed"));
}

#[test]
fn missing_tool_binary_is_reported() {
    let h = Harness::new();
    fs::remove_file(h.fake_bin.join("claude")).unwrap();
    let out = h.mittens(&["harness:claude", "hello"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("claude binary not found at"));
}

#[test]
fn classic_snap_commands_get_shims_over_snap_bin() {
    let h = Harness::new();
    // A fake snapd layout, opentofu-style: the command name (tofu) differs
    // from the snap name, /snap/bin holds the qualified dispatcher entry plus
    // a bare-name alias, and the real command lives at the snap root.
    let bin = h.snap_root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let dispatcher = h.snap_root.join("usr-bin/snap");
    fs::create_dir_all(dispatcher.parent().unwrap()).unwrap();
    executable(&dispatcher, "");
    std::os::unix::fs::symlink(&dispatcher, bin.join("opentofu.tofu")).unwrap();
    std::os::unix::fs::symlink("opentofu.tofu", bin.join("tofu")).unwrap();
    let rev = h.snap_root.join("opentofu/252");
    fs::create_dir_all(rev.join("meta")).unwrap();
    fs::write(
        rev.join("meta/snap.yaml"),
        "name: opentofu\nconfinement: classic\napps:\n  tofu:\n    command: tofu\n    environment:\n      TOFU_FLAVOR: 'firm'\n",
    )
    .unwrap();
    executable(
        &rev.join("tofu"),
        "#!/bin/sh\necho \"REAL-TOFU args=$* SNAP=$SNAP name=$SNAP_NAME rev=$SNAP_REVISION flavor=$TOFU_FLAVOR\"\n",
    );
    std::os::unix::fs::symlink("252", h.snap_root.join("opentofu/current")).unwrap();
    // A strictly confined snap stays a dispatcher symlink.
    std::os::unix::fs::symlink(&dispatcher, bin.join("spotify")).unwrap();
    let strict = h.snap_root.join("spotify/7");
    fs::create_dir_all(strict.join("meta")).unwrap();
    fs::write(
        strict.join("meta/snap.yaml"),
        "name: spotify\nconfinement: strict\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("7", h.snap_root.join("spotify/current")).unwrap();

    let out = h.mittens(&["harness:claude", "hi"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // The generated shim directory is overmounted onto the snap bin dir.
    assert!(stdout(&out).contains(&format!(
        "--ro-bind {}/snap-bin -> {}",
        h.state.display(),
        bin.display()
    )));

    // The alias's shim execs the snap's real command with the environment
    // snap run would have provided.
    let shim = run(Command::new(h.state.join("snap-bin/tofu")).arg("plan"));
    assert!(shim.status.success(), "stderr: {}", stderr(&shim));
    assert_eq!(
        stdout(&shim),
        format!(
            "REAL-TOFU args=plan SNAP={}/opentofu/current name=opentofu rev=252 flavor=firm\n",
            h.snap_root.display()
        )
    );
    // The strict snap's entry replicated the dispatcher symlink.
    assert_eq!(
        fs::read_link(h.state.join("snap-bin/spotify")).unwrap(),
        dispatcher
    );
}
