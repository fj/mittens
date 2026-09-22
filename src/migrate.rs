//! --migrate: move the harness's existing real-home data into its state
//! directory without launching the tool. Wrapped runs do the same thing on
//! their own (see engine::adopt); this is the form you can run deliberately,
//! and the only one that refuses while the tool is live.

use anyhow::{Result, bail};

use crate::harnesses::Ctx;
use crate::relocate;
use crate::util::pgrep;

pub fn run(ctx: &Ctx) -> Result<()> {
    let harness = ctx.harness.name();
    if !pgrep(harness).is_empty() {
        bail!("{harness} processes are running; close all {harness} sessions before migrating");
    }

    let stray = ctx.stray()?;
    if stray.is_empty() {
        println!(
            "nothing to migrate: no {harness} data in the real home ({})",
            ctx.stray_names()
        );
        return Ok(());
    }
    relocate::run(&stray, &mut |step| println!("{}", step.describe(&ctx.home)))?;
    println!("migration complete; run mittens normally from now on");
    Ok(())
}
