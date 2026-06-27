//! katsuctl — host-side Katsuobushi sandbox controller (design/katsuctl.md §4).
//!
//! Two-level CLI: `katsuctl <domain> <command>`. Only the `sandbox` domain
//! exists today; the [`Domain`] enum leaves room for siblings, each backed by a
//! module under `src/`. This scaffold wires up parsing and dispatch only —
//! every subcommand stub returns an `unimplemented` error (design §12 phase 1).

mod sandbox;

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "katsuctl", version)]
struct Cli {
    /// Emit machine-readable JSON instead of human output.
    #[arg(long, global = true)]
    json: bool,
    /// Color policy for human output.
    #[arg(long, global = true, value_enum, default_value_t = ColorWhen::Auto)]
    color: ColorWhen,
    #[command(subcommand)]
    domain: Domain,
}

/// Color policy for human-facing output. Global so every future domain inherits
/// it; honored once the subcommands grow real output (design §13).
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand)]
enum Domain {
    /// Control Katsuobushi agent sandboxes.
    Sandbox(SandboxArgs),
    // future: Domain::Foo(...) — siblings here, modules under src/<domain>/
}

#[derive(Args)]
struct SandboxArgs {
    /// Path to the Nix-rendered instance spec (JSON). Required for every
    /// sandbox subcommand — they all need state-dir roots and pinned tool paths.
    #[arg(long)]
    config: PathBuf,
    #[command(subcommand)]
    command: SandboxCommand,
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Boot a new sandbox instance.
    Start {
        /// Run in detached agent mode (vsock control channel) rather than
        /// interactive attach.
        #[arg(long)]
        agent: bool,
        /// Persist this instance under a stable name instead of an ephemeral one.
        #[arg(long)]
        name: Option<String>,
        /// Agent mode: send this prompt once the instance is ready.
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Push a prompt to a running instance and stream its reports.
    Prompt {
        /// Instance name or 1-based index into the sorted listing.
        instance: String,
        /// Prompt text; remaining args are joined with spaces.
        text: Vec<String>,
    },
    /// Show one instance, or a table of all instances when omitted.
    Status {
        /// Instance name or 1-based index; omit for the full table.
        instance: Option<String>,
    },
    /// Fetch the work product (the sandbox branch) from an instance.
    Fetch {
        /// Instance name or 1-based index.
        instance: String,
    },
    /// Stop an instance, optionally removing its persisted state.
    Stop {
        /// Also remove the instance's state directory.
        #[arg(long)]
        remove: bool,
        /// Instance name or 1-based index.
        instance: String,
    },
    /// Attach to an instance's interactive tmux session.
    Attach {
        /// Instance name or 1-based index.
        instance: String,
    },
}

/// Global flags threaded to every subcommand stub.
#[derive(Copy, Clone)]
struct Global {
    #[allow(dead_code)] // consumed once subcommands grow real output (design §13)
    json: bool,
    #[allow(dead_code)]
    color: ColorWhen,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let global = Global {
        json: cli.json,
        color: cli.color,
    };
    match cli.domain {
        Domain::Sandbox(args) => sandbox::dispatch(args, global),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("args should parse")
    }

    /// clap's own consistency checks (duplicate flags, bad defaults, etc.).
    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// An all-digit `instance` is accepted verbatim as a String (it later
    /// resolves as a 1-based index; resolution is not clap's job). See
    /// design/katsuctl.md §4 on index-vs-name.
    #[test]
    fn all_digit_instance_is_a_string() {
        let cli = parse(&[
            "katsuctl",
            "sandbox",
            "--config",
            "/tmp/s.json",
            "status",
            "42",
        ]);
        match cli.domain {
            Domain::Sandbox(SandboxArgs {
                command: SandboxCommand::Status { instance },
                ..
            }) => assert_eq!(instance.as_deref(), Some("42")),
            _ => panic!("expected sandbox status"),
        }
    }

    #[test]
    fn json_and_color_parse() {
        let cli = parse(&[
            "katsuctl", "--json", "--color", "always", "sandbox", "--config", "/x", "status",
        ]);
        assert!(cli.json);
        assert_eq!(cli.color, ColorWhen::Always);
    }

    #[test]
    fn color_defaults_to_auto_and_json_off() {
        let cli = parse(&["katsuctl", "sandbox", "--config", "/x", "status"]);
        assert_eq!(cli.color, ColorWhen::Auto);
        assert!(!cli.json);
    }

    /// Global flags are accepted after the subcommand too (`global = true`).
    #[test]
    fn global_flags_accepted_after_subcommand() {
        let cli = parse(&["katsuctl", "sandbox", "--config", "/x", "status", "--json"]);
        assert!(cli.json);
    }

    #[test]
    fn color_never_parses() {
        let cli = parse(&[
            "katsuctl", "sandbox", "--config", "/x", "--color", "never", "status",
        ]);
        assert_eq!(cli.color, ColorWhen::Never);
    }

    /// Trailing `text` args are collected into a Vec (joined with spaces later).
    #[test]
    fn prompt_text_is_collected() {
        let cli = parse(&[
            "katsuctl", "sandbox", "--config", "/x", "prompt", "my-inst", "hello", "world",
        ]);
        match cli.domain {
            Domain::Sandbox(SandboxArgs {
                command: SandboxCommand::Prompt { instance, text },
                ..
            }) => {
                assert_eq!(instance, "my-inst");
                assert_eq!(text, vec!["hello", "world"]);
            }
            _ => panic!("expected sandbox prompt"),
        }
    }

    #[test]
    fn stop_remove_flag_and_instance() {
        let cli = parse(&[
            "katsuctl", "sandbox", "--config", "/x", "stop", "--remove", "inst-1",
        ]);
        match cli.domain {
            Domain::Sandbox(SandboxArgs {
                command: SandboxCommand::Stop { remove, instance },
                ..
            }) => {
                assert!(remove);
                assert_eq!(instance, "inst-1");
            }
            _ => panic!("expected sandbox stop"),
        }
    }

    #[test]
    fn start_flags_parse() {
        let cli = parse(&[
            "katsuctl",
            "sandbox",
            "--config",
            "/x",
            "start",
            "--agent",
            "--name",
            "foo",
            "--prompt",
            "do the thing",
        ]);
        match cli.domain {
            Domain::Sandbox(SandboxArgs {
                command:
                    SandboxCommand::Start {
                        agent,
                        name,
                        prompt,
                    },
                config,
            }) => {
                assert!(agent);
                assert_eq!(name.as_deref(), Some("foo"));
                assert_eq!(prompt.as_deref(), Some("do the thing"));
                assert_eq!(config, PathBuf::from("/x"));
            }
            _ => panic!("expected sandbox start"),
        }
    }

    /// `--config` is mandatory for the sandbox domain.
    #[test]
    fn missing_config_is_an_error() {
        assert!(Cli::try_parse_from(["katsuctl", "sandbox", "status"]).is_err());
    }
}
