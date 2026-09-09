//! mittens — run coding agents with clean paws.
//!
//! Some coding agents litter the real home directory with hardcoded dotfiles
//! instead of following the XDG Base Directory spec. mittens runs the agent
//! inside a private mount namespace (via bubblewrap) where those paths are
//! redirected to an XDG-compliant state directory, so the real home directory
//! stays free of tool cruft. See README.md for the full story; src/harnesses/
//! for what each supported tool needs; src/engine.rs for the namespace.

use std::ffi::OsString;

mod engine;
mod harnesses;
mod inner;
mod migrate;
mod snap;
mod ssh;
mod util;
mod wipe;

use harnesses::{Ctx, Harness};

const HARNESS_PREFIX: &str = "harness:";

fn main() {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();

    // Hidden entry point re-exec'd inside the namespace; not part of the CLI.
    if args.first().is_some_and(|a| a == inner::SUBCOMMAND) {
        inner::run(&args[1..]);
    }

    // The harness must be selected explicitly, as harness:<name> in the first
    // argument; there is no default.
    let harness = match args.first().and_then(|a| a.to_str()) {
        Some("-h" | "--help") => {
            print!("{}", usage(None));
            return;
        }
        Some("self-update") => {
            if args.len() > 1 {
                fail(anyhow::anyhow!("self-update takes no arguments"));
            }
            self_update()
        }
        Some(arg) if arg.starts_with(HARNESS_PREFIX) => {
            let name = &arg[HARNESS_PREFIX.len()..];
            match Harness::from_name(name) {
                Some(harness) => {
                    args.remove(0);
                    harness
                }
                None => fail(anyhow::anyhow!(
                    "unknown harness \"{name}\" (known harnesses: {})",
                    Harness::known()
                )),
            }
        }
        Some(name) if Harness::from_name(name).is_some() => fail(anyhow::anyhow!(
            "the harness must be selected as harness:<name>; \
             did you mean \"mittens {HARNESS_PREFIX}{name}\"?"
        )),
        _ => fail(anyhow::anyhow!(
            "the first argument must select a harness: {HARNESS_PREFIX}<name>, \
             where <name> is one of: {} (see mittens --help)",
            Harness::known()
        )),
    };
    let ctx = Ctx::resolve(harness);

    // Flags are intercepted only directly after the harness selector;
    // everything else is passed through to the tool unchanged.
    let mode = match args.first().and_then(|a| a.to_str()) {
        Some("-h" | "--help") => {
            print!("{}", usage(Some(&ctx)));
            return;
        }
        Some("--migrate") => {
            if let Err(err) = migrate::run(&ctx) {
                fail(err);
            }
            return;
        }
        Some("--unsafe") => {
            args.remove(0);
            engine::Mode::Unsafe
        }
        Some("--dangerously-skip-pawmissions") => {
            args.remove(0);
            engine::Mode::SkipPawmissions
        }
        _ => engine::Mode::Wrapped,
    };

    // launch() only returns on failure: on success it execs the tool.
    let err = match engine::launch(&ctx, mode, &args) {
        Ok(never) => match never {},
        Err(err) => err,
    };
    fail(err);
}

/// Rebuild and reinstall the binary from the upstream repository via `cargo
/// install`. When the running binary sits in a cargo-style `<root>/bin`
/// directory, the new one is installed over it; otherwise cargo's default
/// install root applies.
fn self_update() -> ! {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new("cargo");
    cmd.args([
        "install",
        "--locked",
        "--force",
        "--git",
        env!("CARGO_PKG_REPOSITORY"),
        env!("CARGO_PKG_NAME"),
    ]);
    let exe = std::env::current_exe().ok();
    if let Some(root) = exe.as_deref().and_then(util::cargo_install_root) {
        cmd.arg("--root").arg(root);
    }
    let err = cmd.exec();
    fail(anyhow::Error::new(err).context("exec cargo install (is cargo installed?)"));
}

fn fail(err: anyhow::Error) -> ! {
    eprintln!("mittens: {err:#}");
    std::process::exit(1);
}

fn usage(ctx: Option<&Ctx>) -> String {
    let generic = format!(
        r#"mittens — run coding agents with their home-dir cruft redirected to XDG

Usage:
  mittens harness:<name> [arguments...]
                                  run the harness inside the namespace;
                                  <name> is one of: {known}, and must
                                  always be given explicitly
  mittens harness:<name> --migrate
                                  move the harness's existing real-home data
                                  (claude: ~/.claude and ~/.claude.json;
                                  opencode: ~/.opencode; pi: ~/.pi) into its
                                  state directory (run this once, with no
                                  sessions of the harness running)
  mittens harness:<name> --unsafe [arguments...]
                                  skip the sandbox: exec the tool directly
                                  with its relocation variable pointing at
                                  the state dir. claude (CLAUDE_CONFIG_DIR)
                                  and pi (PI_CODING_AGENT_DIR) only: it
                                  relies on the tool honoring the variable
                                  instead of a kernel-level redirect, and the
                                  shared agent config is not wired in. claude
                                  additionally keeps its top-level config at
                                  dot-claude/.claude.json (seeded once from
                                  claude.json, independent of wrapped runs
                                  after that)
  mittens harness:<name> --dangerously-skip-pawmissions [arguments...]
                                  inspect the harness's real-home data, count
                                  down, then DELETE it (rm -rf) to clear the
                                  startup guard, and launch anyway. Destroys
                                  data. Only use when you are certain the
                                  real-home copy is disposable leftover.
  mittens self-update             rebuild and reinstall the binary from
                                  {repository} via
                                  cargo install, over the running binary's
                                  own install root
  mittens --help | -h             show this help

harness:<name> and self-update are only recognized as the first argument;
--migrate, --unsafe, --dangerously-skip-pawmissions, --help, and -h only
directly after harness:<name>.
Everything else is passed through to the tool unchanged (so "mittens
harness:claude config get theme" etc. work as expected; for the tool's own
help, run "mittens harness:<name> help").
"#,
        known = Harness::known(),
        repository = env!("CARGO_PKG_REPOSITORY"),
    );
    let Some(ctx) = ctx else {
        return format!(
            "{generic}\nRun mittens harness:<name> --help to see that harness's resolved paths.\n"
        );
    };
    format!(
        r#"{generic}
Harness:             {harness}
State directory:     {state}
Binary:              {bin}
Shared agent config: {agents} (used by the claude and pi wiring)
(Override with MITTENS_STATE_DIR / {bin_envs}
/ MITTENS_AGENTS_DIR.)
"#,
        harness = ctx.harness.name(),
        state = ctx.state.display(),
        bin = ctx.bin.as_ref().map_or("(not found)".into(), |b| b.display().to_string()),
        agents = ctx.agents_cfg.display(),
        bin_envs = Harness::ALL.map(Harness::bin_env).join(" / "),
    )
}
