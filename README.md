# mittens

Run coding agents with clean paws.

Some coding agents litter the real home directory with hardcoded dotfiles instead of following the [XDG Base Directory spec](https://specifications.freedesktop.org/basedir-spec/latest/). These agents have also refused to implement this specification for various reasons.

`mittens` runs agents inside a private mount namespace via [bubblewrap](https://github.com/containers/bubblewrap). Those paths are redirected to an XDG-compliant state directory, so the real home directory stays free of tool cruft.

## How it works

Inside the namespace, `$HOME` is replaced with a throwaway tmpfs, every real top-level entry of the home directory is bind-mounted back into place, and the state directory is mounted over the tool's hardcoded dot-directory. The mount point is created on the tmpfs and never on disk.

Because the redirect happens at the kernel VFS layer, it applies to the tool and every subprocess it spawns (shell tools, MCP servers, hooks). It does not depend on the tool honoring any environment variable, now or in future versions.

## Harnesses

Everything tool-specific lives in a harness handler (`src/harness.rs`). The first argument selects the harness as `harness:<name>`; there is no default, and mittens refuses to run without an explicit harness:

    mittens harness:claude [claude arguments...]      # Claude Code
    mittens harness:opencode [opencode arguments...]  # opencode
    ...

This lets you alias `mittens harness:X` to `X` in your favorite shell so you don't have to type that each time.

### claude

Claude Code hardcodes `~/.claude` and `~/.claude.json` ([anthropics/claude-code#1455](https://github.com/anthropics/claude-code/issues/1455)). State directory (default: `~/.local/state/claude-home`; the name deliberately avoids `~/.local/state/claude`, which Claude Code's native launcher already uses for its own lock files):

    dot-claude/    what Claude Code sees as ~/.claude
    claude.json    what Claude Code sees as ~/.claude.json

On top of that, a shared, tool-agnostic agent config (default: `~/.config/agents`, following the XDG config spec and the AGENTS.md convention) is bind-mounted into the `~/.claude` view so it is not duplicated in the per-tool state dir:

    ~/.config/agents/agents        -> ~/.claude/agents
    ~/.config/agents/commands      -> ~/.claude/commands
    ~/.config/agents/skills        -> ~/.claude/skills
    ~/.config/agents/hooks         -> ~/.claude/hooks
    ~/.config/agents/output-styles -> ~/.claude/output-styles
    ~/.config/agents/AGENTS.md     -> ~/.claude/CLAUDE.md   (read-only)

Each is mounted only if its source exists, so mittens still runs without the shared config present, and a mountpoint that does not exist yet starts working the moment you create it under the shared config. Machine-specific state (settings.json, plugins, projects, sessions, credentials) stays in the per-tool state dir and is never shared.

`~/.claude.json` is copied into the tmpfs at launch and synced back out on exit, rather than bind-mounted, because Claude Code appears to rewrite it via atomic rename — which would silently detach a file bind-mount. Consequence: when several mittens sessions run at once, the last one to exit wins for claude.json (the dot-claude directory is a shared bind mount and is not affected).

### opencode

opencode is mostly XDG-clean (`~/.config/opencode`, `~/.local/{share,state}/opencode`, `~/.cache/opencode`) but still drops a legacy `~/.opencode` into the real home (global plugin node_modules, bin/). mittens shadows that one path; the XDG directories are left alone. State directory (default: `~/.local/state/opencode-home`):

    dot-opencode/  what opencode sees as ~/.opencode

opencode has no `~/.claude.json` analogue, so nothing is copy-synced. The shared agent config is not wired in either: opencode's global config lives at the real, XDG-proper `~/.config/opencode`, which is visible unmodified inside the namespace — share pieces of `~/.config/agents` into it with plain symlinks, no sandbox required.

When opencode is installed as a snap, the `opencode` on PATH is really the snap dispatcher (`/snap/bin/opencode -> /usr/bin/snap`), which re-execs through snap-confine — and snap-confine refuses to run inside mittens' unprivileged user namespace ("snap-confine has elevated permissions and is not confined but should be"). mittens detects the dispatcher and execs the snap's real binary (`/snap/opencode/current/bin/opencode`) directly instead, with `OPENCODE_DISABLE_AUTOUPDATE=1` set as the snap packaging would have (the snap's squashfs is read-only, so self-update cannot work). An explicit `MITTENS_OPENCODE_BIN` is used verbatim, without this resolution — though a binary under `/snap` still gets the autoupdate opt-out.

## Snap-packaged tools inside the sandbox

The snap-confine refusal above hits every snap-installed tool the agent invokes *inside* the namespace, too — `tofu`, `uv`, `task`, whatever else `/snap/bin` holds — because each of those PATH entries is the same dispatcher. At launch, mittens generates a shim directory (`<state>/snap-bin`) and read-only bind-mounts it over `/snap/bin` inside the namespace:

- For a classic-confined snap, the shim is a small `sh` wrapper that exports the `SNAP*` variables `snap run` would have set (snaps' own launcher scripts dereference `$SNAP`) plus the app's `environment:` from `meta/snap.yaml`, then execs the snap's real command under `/snap/<name>/current` directly. Classic commands are plain executables that run fine without confinement, so behavior matches an unsandboxed run.
- Everything else is replicated unchanged. Strictly confined snaps genuinely need snap-confine's setuid-root privileges, which cannot exist inside an unprivileged user namespace, so those keep failing with the honest snap-confine error rather than something mittens invented.

The shim directory is refreshed on every launch, entry by entry, so concurrently running sessions that have it mounted as their `/snap/bin` are never left with an empty directory.

## ssh inside the sandbox

Only the invoking uid is mapped in the sandbox's user namespace, so every root-owned file appears as `nobody:nogroup` inside it. OpenSSH perm-checks each file it pulls in through an `Include` directive — the owner must be root or the invoking user — so the stock `Include /etc/ssh/ssh_config.d/*.conf` in `/etc/ssh/ssh_config` aborts ssh at startup on any distribution shipping a drop-in there:

    Bad owner or permissions on /etc/ssh/ssh_config.d/20-systemd-ssh-proxy.conf

That is fatal, not a warning, and it hits every ssh invocation inside the namespace: git over ssh, scp, rsync, `ProxyJump`, anything shelling out to ssh. The ownership cannot be repaired from within, since an unprivileged user namespace cannot map root. So at launch mittens walks the `Include` graph of the system-wide config, replicates each included file byte for byte under `<state>/ssh-config` (a mirror of its absolute path, and therefore owned by you), and read-only bind-mounts each replica over the original inside the namespace. What ssh reads is unchanged; only the owner it sees is.

`/etc/ssh/ssh_config` itself is left alone — OpenSSH reads the system-wide file without the ownership check — as is any included file mittens cannot read, which then fails inside the sandbox exactly as it would outside. A config that includes nothing wires nothing. Replicas are refreshed on every launch and pruned when a drop-in goes away.

## Escape hatch

`mittens harness:<name> --unsafe` skips bubblewrap and just execs the tool with an environment variable pointing at the state dir. Only claude supports it: Claude Code documents `CLAUDE_CONFIG_DIR` as relocating every `~/.claude` path, and it also moves the top-level config to `$CLAUDE_CONFIG_DIR/.claude.json` — a different location than the claude.json wrapped runs use, so the unsafe copy is seeded from claude.json once and evolves independently after that. The bind-mount wiring of the shared agent config is also absent in unsafe mode. Unlike the namespace, this depends entirely on Claude Code (and everything it spawns) honoring the environment variable — hence the name. opencode's `OPENCODE_CONFIG_DIR` only relocates config *loading*; nothing relocates `~/.opencode` itself, so opencode refuses `--unsafe`.

## Other commands

    mittens harness:<name> --migrate
        Move the harness's existing real-home data into its state directory.
        Run once, with no sessions of the harness running.

    mittens harness:<name> --dangerously-skip-pawmissions [arguments...]
        Inspect the harness's real-home data, count down for 10 seconds, then
        DELETE it (rm -rf) to clear the startup guard, and launch anyway.
        Destroys data; only for disposable leftovers of unwrapped runs.

## Caveats

- Plain `claude`/`opencode` runs outside mittens will recreate their dotfiles in the real home; mittens refuses to start while any of the harness's real-home paths exist, to prevent silent divergence between the two. Consider aliasing `claude` to `mittens harness:claude` and `opencode` to `mittens harness:opencode` in your shell (if you shadow the real opencode with a wrapper named `opencode`, set `MITTENS_OPENCODE_BIN` so mittens does not resolve the wrapper from PATH and recurse).
- New top-level entries created under `$HOME` inside the namespace land on the tmpfs and vanish on exit. If a tool needs a new persistent `~/.something`, create it in the real home first; it will be bound in on the next launch.
- Only the current uid is mapped in the sandbox's user namespace, so files owned by anyone else (root included) appear as nobody:nogroup inside it. ssh is repaired specifically (see above); any other tool that insists on an ownership of its own config is subject to the same problem.
- Tools' own bwrap-based sandboxing cannot start nested inside mittens under Debian's stock AppArmor policy: the stacked bwrap//&unpriv_bwrap profile denies creating a nested mount namespace. Fixable with a custom AppArmor profile for a private copy of bwrap.

## Environment

    MITTENS_STATE_DIR      override the state directory of the invoked harness
                           (per-invocation — it applies to whichever harness
                           runs, so don't export it globally if you use more
                           than one harness)
    MITTENS_CLAUDE_BIN     override the claude executable
                           (default: ~/.local/bin/claude)
    MITTENS_OPENCODE_BIN   override the opencode executable
                           (default: first opencode on PATH)
    MITTENS_AGENTS_DIR     override the shared agent config directory
                           (default: ${XDG_CONFIG_HOME:-~/.config}/agents;
                           only the claude wiring uses it)

## Installation

Install straight from the repository — cargo clones, builds, and installs the binary without needing a local checkout:

    cargo install --locked --git https://github.com/fj/mittens mittens

The binary lands in cargo's install root (`~/.cargo/bin` by default; override with `--root <dir>` to install to `<dir>/bin/mittens`, e.g. `--root ~/.local` for `~/.local/bin/mittens`). bubblewrap must be present at runtime (`apt install bubblewrap` or your distro's equivalent).

From a local checkout, the same mechanism works with `--path`:

    cargo install --locked --path .

To update:

    mittens self-update

This re-runs the `cargo install` from the repository, reinstalling over the running binary's own install root (any cargo-style `<root>/bin` directory — including `~/.local/bin` if it was installed with `--root ~/.local`; a binary somewhere else updates into cargo's default root instead).
