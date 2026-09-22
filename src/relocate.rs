//! Moving the harness's real-home data into its state directory: the step
//! behind --migrate, and the one every launch performs on its own. Which paths
//! move where is the caller's to say (see Ctx::stray); this is only the moving.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::util::{is_real_dir, sorted_entry_names, tilde};

/// One completed step, for reporting.
pub enum Step {
    Moved {
        from: PathBuf,
        to: PathBuf,
        dir: bool,
    },
    /// A directory that merged away entry by entry and was then empty.
    Emptied(PathBuf),
    /// A symlink that already pointed at its own destination.
    Dropped { link: PathBuf, target: PathBuf },
}

impl Step {
    pub fn describe(&self, home: &Path) -> String {
        match self {
            Step::Moved { from, to, dir } => {
                let slash = if *dir { "/" } else { "" };
                format!(
                    "moved {}{slash} -> {}{slash}",
                    tilde(from, home),
                    to.display()
                )
            }
            Step::Emptied(path) => format!("removed empty {}/", tilde(path, home)),
            Step::Dropped { link, target } => format!(
                "removed {}: already a link to {}",
                tilde(link, home),
                target.display()
            ),
        }
    }
}

/// Move each `from` onto its `to`, reporting every step to `report` as it
/// completes — including the steps before a failure, since a half-moved
/// relocation is exactly when the record of what already moved matters.
///
/// Directories that exist on both sides merge entry by entry, so state-dir
/// data outside the overlap survives. Everything else replaces its
/// destination: the real-home copy is the later write, being the one made
/// outside the sandbox after the state dir was last used.
pub fn run(moves: &[(PathBuf, PathBuf)], report: &mut dyn FnMut(&Step)) -> Result<()> {
    for (from, to) in moves {
        take(from, to, report)?;
    }
    Ok(())
}

fn take(src: &Path, dst: &Path, report: &mut dyn FnMut(&Step)) -> Result<()> {
    // A hand-rolled redirect predating mittens — ~/.claude symlinked to the
    // state dir — names its own destination. Moving it would delete the very
    // data it points at, so drop the link and leave that data alone.
    if points_at(src, dst) {
        fs::remove_file(src).with_context(|| format!("removing the link {}", src.display()))?;
        report(&Step::Dropped {
            link: src.to_path_buf(),
            target: dst.to_path_buf(),
        });
        return Ok(());
    }

    if is_real_dir(src) && is_real_dir(dst) {
        for name in sorted_entry_names(src).with_context(|| format!("reading {}", src.display()))? {
            take(&src.join(&name), &dst.join(&name), report)?;
        }
        fs::remove_dir(src).with_context(|| format!("removing the emptied {}", src.display()))?;
        report(&Step::Emptied(src.to_path_buf()));
        return Ok(());
    }

    if dst.symlink_metadata().is_ok() {
        let replaced = if is_real_dir(dst) {
            fs::remove_dir_all(dst)
        } else {
            fs::remove_file(dst)
        };
        replaced.with_context(|| format!("replacing {}", dst.display()))?;
    }
    let parent = dst.parent().expect("a state-dir destination has a parent");
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let dir = is_real_dir(src);
    fs::rename(src, dst).with_context(|| {
        format!(
            "moving {} -> {} (if they are on different filesystems, move it by hand)",
            src.display(),
            dst.display()
        )
    })?;
    report(&Step::Moved {
        from: src.to_path_buf(),
        to: dst.to_path_buf(),
        dir,
    });
    Ok(())
}

/// True when `src` is a symlink resolving to `dst`. Both sides must resolve:
/// two paths that merely fail to canonicalize are not the same file.
fn points_at(src: &Path, dst: &Path) -> bool {
    src.is_symlink()
        && match (fs::canonicalize(src), fs::canonicalize(dst)) {
            (Ok(src), Ok(dst)) => src == dst,
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch home and state dir. Nothing here needs a harness: this module
    /// only moves the pairs it is handed.
    struct Scratch {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        state: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            let state = tmp.path().join("state");
            fs::create_dir_all(&home).unwrap();
            Scratch {
                _tmp: tmp,
                home,
                state,
            }
        }

        /// Relocate `~/<name>` to `<state>/<dest>`, collecting the report.
        fn relocate(&self, name: &str, dest: &str) -> Result<Vec<String>> {
            let moves = [(self.home.join(name), self.state.join(dest))];
            let mut lines = Vec::new();
            run(&moves, &mut |step| lines.push(step.describe(&self.home)))?;
            Ok(lines)
        }
    }

    #[test]
    fn an_absent_destination_takes_the_whole_tree_in_one_move() {
        let s = Scratch::new();
        fs::create_dir_all(s.home.join(".claude/projects")).unwrap();
        fs::write(s.home.join(".claude/settings.json"), "{}").unwrap();

        let lines = s.relocate(".claude", "dot-claude").unwrap();

        // One move for the whole tree, not one per file: nothing to merge with.
        assert_eq!(
            lines,
            vec![format!(
                "moved ~/.claude/ -> {}/",
                s.state.join("dot-claude").display()
            )]
        );
        assert!(s.state.join("dot-claude/projects").is_dir());
        assert!(s.state.join("dot-claude/settings.json").is_file());
        assert!(!s.home.join(".claude").exists());
    }

    #[test]
    fn a_populated_destination_merges_and_the_real_home_wins_collisions() {
        let s = Scratch::new();
        let dot = s.state.join("dot-claude");
        fs::create_dir_all(dot.join("projects")).unwrap();
        fs::write(dot.join("projects/kept.json"), "wrapped").unwrap();
        fs::write(dot.join("settings.json"), "wrapped").unwrap();
        fs::write(dot.join("only-wrapped.json"), "wrapped").unwrap();
        fs::create_dir_all(s.home.join(".claude/projects")).unwrap();
        fs::write(s.home.join(".claude/projects/new.json"), "real").unwrap();
        fs::write(s.home.join(".claude/settings.json"), "real").unwrap();

        let lines = s.relocate(".claude", "dot-claude").unwrap();

        let shown = dot.display().to_string();
        assert_eq!(
            lines,
            vec![
                format!("moved ~/.claude/projects/new.json -> {shown}/projects/new.json"),
                "removed empty ~/.claude/projects/".to_string(),
                format!("moved ~/.claude/settings.json -> {shown}/settings.json"),
                "removed empty ~/.claude/".to_string(),
            ]
        );
        // The colliding file took the real-home copy; everything the merge did
        // not touch is still there.
        assert_eq!(
            fs::read_to_string(dot.join("settings.json")).unwrap(),
            "real"
        );
        assert_eq!(
            fs::read_to_string(dot.join("projects/kept.json")).unwrap(),
            "wrapped"
        );
        assert_eq!(
            fs::read_to_string(dot.join("only-wrapped.json")).unwrap(),
            "wrapped"
        );
        assert!(!s.home.join(".claude").exists());
    }

    #[test]
    fn a_symlink_moves_whole_rather_than_being_descended_into() {
        let s = Scratch::new();
        // A dangling symlink at ~/.claude is stray too, so it has to move
        // rather than be reported as nothing left to do.
        std::os::unix::fs::symlink("/nonexistent", s.home.join(".claude")).unwrap();
        // A real directory on the state side must not tempt the merge into
        // following the link.
        fs::create_dir_all(s.state.join("dot-claude")).unwrap();

        let lines = s.relocate(".claude", "dot-claude").unwrap();

        let dot = s.state.join("dot-claude");
        assert_eq!(lines, vec![format!("moved ~/.claude -> {}", dot.display())]);
        assert!(dot.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(s.home.join(".claude").symlink_metadata().is_err());
    }

    #[test]
    fn a_link_to_its_own_destination_is_dropped_not_moved_onto_it() {
        let s = Scratch::new();
        // A hand-rolled redirect from before mittens: ~/.claude is a symlink
        // to the state dir, which already holds the only copy of the data.
        let dot = s.state.join("dot-claude");
        fs::create_dir_all(&dot).unwrap();
        fs::write(dot.join("settings.json"), "the only copy").unwrap();
        std::os::unix::fs::symlink(&dot, s.home.join(".claude")).unwrap();

        let lines = s.relocate(".claude", "dot-claude").unwrap();

        assert_eq!(
            lines,
            vec![format!(
                "removed ~/.claude: already a link to {}",
                dot.display()
            )]
        );
        // Moving the link would have deleted the directory it names first.
        assert_eq!(
            fs::read_to_string(dot.join("settings.json")).unwrap(),
            "the only copy"
        );
        assert!(s.home.join(".claude").symlink_metadata().is_err());
    }

    #[test]
    fn a_link_pointing_elsewhere_still_moves() {
        let s = Scratch::new();
        let elsewhere = s.home.join("dropbox/claude");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, s.home.join(".claude")).unwrap();

        let lines = s.relocate(".claude", "dot-claude").unwrap();

        let dot = s.state.join("dot-claude");
        assert_eq!(lines, vec![format!("moved ~/.claude -> {}", dot.display())]);
        assert_eq!(fs::read_link(&dot).unwrap(), elsewhere);
    }

    #[test]
    fn the_steps_before_a_failure_are_still_reported() {
        let s = Scratch::new();
        fs::create_dir_all(s.state.join("dot-claude")).unwrap();
        fs::write(s.home.join(".claude.json"), "{}").unwrap();
        fs::create_dir_all(s.home.join(".claude/projects")).unwrap();
        // The second destination's parent is a file, so its create_dir_all
        // fails after the first move has already happened.
        fs::write(s.state.join("wedged"), "not a directory").unwrap();

        let moves = [
            (s.home.join(".claude"), s.state.join("dot-claude")),
            (
                s.home.join(".claude.json"),
                s.state.join("wedged/claude.json"),
            ),
        ];
        let mut lines = Vec::new();
        let result = run(&moves, &mut |step| lines.push(step.describe(&s.home)));

        assert!(result.is_err());
        // Half the data has moved; without this the user could not tell which
        // half, which is exactly when they need to know.
        assert_eq!(
            lines,
            vec![
                format!(
                    "moved ~/.claude/projects/ -> {}/projects/",
                    s.state.join("dot-claude").display()
                ),
                "removed empty ~/.claude/".to_string(),
            ]
        );
        assert!(s.state.join("dot-claude/projects").is_dir());
        assert!(s.home.join(".claude.json").is_file());
    }

    #[test]
    fn no_moves_relocate_nothing() {
        let s = Scratch::new();
        let mut lines: Vec<String> = Vec::new();
        run(&[], &mut |step| lines.push(step.describe(&s.home))).unwrap();

        assert!(lines.is_empty());
        assert!(!s.state.exists());
    }
}
