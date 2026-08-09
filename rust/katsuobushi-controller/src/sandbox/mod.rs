//! The `sandbox` domain: dispatch plus one module per subcommand, so future
//! top-level domains slot in as sibling modules under `src/`.

use crate::{Global, SandboxArgs, SandboxCommand};
use anyhow::Result;

mod attach;
mod deliver;
pub mod directive;
mod dispatch;
pub mod emit;
mod fetch;
pub mod gfx;
pub mod host;
pub mod instance;
pub mod liveness;
mod prompt;
mod prune;
pub mod qmp;
pub mod report_log;
pub mod resolve;
mod screenshot;
pub mod spec;
mod start;
mod status;
mod stop;

/// Whether an orchestration drive should stay armed across an unreported
/// turn-end, given the caller's `--no-until-report`.
///
/// A named function rather than an inline `!flag` so the *polarity* is pinned by
/// a test: inverting it silently is exactly the regression that would restore
/// the old "the command returned, so the work concluded" trap, and no
/// clap-level default test would catch it.
fn armed(no_until_report: bool) -> bool {
    !no_until_report
}

/// Route a parsed `sandbox` invocation to its subcommand handler.
pub fn dispatch(args: SandboxArgs, global: Global) -> Result<()> {
    let SandboxArgs { config, command } = args;
    match command {
        // Orchestration flows stay armed by default: for these, "the command
        // returned" is read as "the work concluded", so an early return on an
        // unreported yield actively misinforms the caller. Interactive
        // `prompt` keeps the opposite default (below) — an operator is watching
        // and can re-prompt. `--until-report` is still accepted on both and is
        // now a no-op; `--no-until-report` restores the old early return.
        SandboxCommand::Start {
            agent,
            name,
            prompt,
            until_report: _,
            no_until_report,
        } => start::run(&config, agent, name, prompt, armed(no_until_report), global),
        SandboxCommand::Dispatch {
            card,
            board_dir,
            force,
            until_report: _,
            no_until_report,
        } => dispatch::run(
            &config,
            &card,
            &board_dir,
            force,
            armed(no_until_report),
            global,
        ),
        SandboxCommand::Prompt {
            instance,
            text,
            until_report,
            redeliver,
        } => prompt::run(&config, &instance, text, until_report, redeliver, global),
        SandboxCommand::Status { instance } => status::run(&config, instance, global),
        SandboxCommand::Fetch { instance } => fetch::run(&config, &instance, global),
        SandboxCommand::Deliver { instance, branch } => {
            deliver::run(&config, &instance, &branch, global)
        }
        SandboxCommand::Stop { remove, instance } => stop::run(&config, remove, &instance, global),
        SandboxCommand::Attach { instance } => attach::run(&config, &instance, global),
        SandboxCommand::Screenshot { instance, path } => {
            screenshot::run(&config, &instance, path, global)
        }
        SandboxCommand::Prune { board_dir } => prune::run(&config, &board_dir, global),
    }
}

#[cfg(test)]
mod tests {
    use super::armed;

    #[test]
    fn orchestration_drives_are_armed_unless_explicitly_opted_out() {
        assert!(
            armed(false),
            "dispatch/prompted-start stay armed by default"
        );
        assert!(!armed(true), "--no-until-report restores the early return");
    }
}
