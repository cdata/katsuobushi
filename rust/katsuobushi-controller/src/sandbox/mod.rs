//! The `sandbox` domain: dispatch plus one module per subcommand, so future
//! top-level domains slot in as sibling modules under `src/`.

use crate::{Global, SandboxArgs, SandboxCommand};
use anyhow::Result;

mod attach;
pub mod emit;
mod fetch;
pub mod gfx;
pub mod host;
pub mod instance;
pub mod liveness;
pub mod output;
mod prompt;
pub mod qmp;
pub mod resolve;
mod screenshot;
pub mod spec;
mod start;
mod status;
mod stop;

/// Route a parsed `sandbox` invocation to its subcommand handler.
pub fn dispatch(args: SandboxArgs, global: Global) -> Result<()> {
    let SandboxArgs { config, command } = args;
    match command {
        SandboxCommand::Start {
            agent,
            name,
            prompt,
        } => start::run(&config, agent, name, prompt, global),
        SandboxCommand::Prompt { instance, text } => prompt::run(&config, &instance, text, global),
        SandboxCommand::Status { instance } => status::run(&config, instance, global),
        SandboxCommand::Fetch { instance } => fetch::run(&config, &instance, global),
        SandboxCommand::Stop { remove, instance } => stop::run(&config, remove, &instance, global),
        SandboxCommand::Attach { instance } => attach::run(&config, &instance, global),
        SandboxCommand::Screenshot { instance, path } => {
            screenshot::run(&config, &instance, path, global)
        }
    }
}
