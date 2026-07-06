//! mittens — run coding agents with clean paws.
//!
//! Some coding agents litter the real home directory with hardcoded dotfiles
//! instead of following the XDG Base Directory spec. mittens runs the agent
//! inside a private mount namespace (via bubblewrap) where those paths are
//! redirected to an XDG-compliant state directory, so the real home directory
//! stays free of tool cruft. See README.md for the full story; src/service.rs
//! for what each supported tool needs; src/engine.rs for the namespace.

use std::ffi::OsString;

mod engine;
mod inner;
mod migrate;
mod service;
mod util;
mod wipe;

use service::{Ctx, Service};

fn main() {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();

    // Hidden entry point re-exec'd inside the namespace; not part of the CLI.
    if args.first().is_some_and(|a| a == inner::SUBCOMMAND) {
        inner::run(&args[1..]);
    }

    // The service name is only recognized as the first argument; anything
    // else means claude, for compatibility with pre-service invocations.
    let svc = match args.first().and_then(|a| a.to_str()).and_then(Service::from_name) {
        Some(svc) => {
            args.remove(0);
            svc
        }
        None => Service::Claude,
    };
    let ctx = Ctx::resolve(svc);

    // Flags are intercepted only directly after the (optional) service name;
    // everything else is passed through to the tool unchanged.
    let mode = match args.first().and_then(|a| a.to_str()) {
        Some("-h" | "--help") => {
            print!("{}", usage(&ctx));
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

fn fail(err: anyhow::Error) -> ! {
    eprintln!("mittens: {err:#}");
    std::process::exit(1);
}

fn usage(ctx: &Ctx) -> String {
    format!(
        r#"mittens — run coding agents with their home-dir cruft redirected to XDG

Usage:
  mittens [service] [arguments...]
                                  run the service inside the namespace;
                                  service is claude or opencode and defaults
                                  to claude
  mittens [service] --migrate     move the service's existing real-home data
                                  (claude: ~/.claude and ~/.claude.json;
                                  opencode: ~/.opencode) into its state
                                  directory (run this once, with no sessions
                                  of the service running)
  mittens [service] --unsafe [arguments...]
                                  skip the sandbox: exec the tool directly
                                  with its relocation variable pointing at
                                  the state dir. claude only, via
                                  CLAUDE_CONFIG_DIR: relies on the tool
                                  honoring the variable instead of a
                                  kernel-level redirect, the shared agent
                                  config is not wired in, and claude's
                                  top-level config lives at
                                  dot-claude/.claude.json (seeded once from
                                  claude.json, independent of wrapped runs
                                  after that)
  mittens [service] --dangerously-skip-pawmissions [arguments...]
                                  inspect the service's real-home data, count
                                  down, then DELETE it (rm -rf) to clear the
                                  startup guard, and launch anyway. Destroys
                                  data. Only use when you are certain the
                                  real-home copy is disposable leftover.
  mittens --help | -h             show this help

The service name is only recognized as the first argument; --migrate,
--unsafe, --dangerously-skip-pawmissions, --help, and -h only directly after
it. Everything else is passed through to the tool unchanged (so "mittens
config get theme" etc. work as expected; for the tool's own help, run
"mittens [service] help").

Service:             {svc}
State directory:     {state}
Binary:              {bin}
Shared agent config: {agents} (used by the claude wiring only)
(Override with MITTENS_STATE_DIR / MITTENS_CLAUDE_BIN / MITTENS_OPENCODE_BIN
/ MITTENS_AGENTS_DIR.)
"#,
        svc = ctx.svc.name(),
        state = ctx.state.display(),
        bin = ctx.bin.as_ref().map_or("(not found)".into(), |b| b.display().to_string()),
        agents = ctx.agents_cfg.display(),
    )
}
