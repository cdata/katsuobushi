//! katsuctl — host-side Katsuobushi sandbox controller.
//!
//! Two-level CLI: `katsuctl <domain> <command>`. Two domains exist: `sandbox`
//! (agent VMs) and `project` (an Obsidian-Kanban-native backlog). The
//! [`Domain`] enum leaves room for more, each backed by a module under `src/`.

mod project;
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
/// it; honored once the subcommands grow real output.
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
    /// Manage an Obsidian-Kanban-native project backlog.
    Project(ProjectArgs),
    // future: Domain::Foo(...) — siblings here, modules under src/<domain>/
}

#[derive(Args)]
struct ProjectArgs {
    /// The board directory (holds BOARD.md and the card notes). Relative to the
    /// cwd unless absolute. Hidden from help: the `project` menu wrapper always
    /// injects it, so a user never types it.
    #[arg(long, default_value = "project/kanban", global = true, hide = true)]
    board_dir: PathBuf,
    #[command(subcommand)]
    command: ProjectCommand,
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// Scaffold the board directory (idempotent; never clobbers existing cards).
    Init,
    /// Mint a new card note and add it to the To-do lane.
    New {
        /// The card title (the `title:` frontmatter field).
        #[arg(long)]
        title: String,
        /// The issue kind.
        #[arg(long = "type", value_enum, default_value_t = KindArg::Feature)]
        kind: KindArg,
        /// Ids this card is blocked by (comma-separated or repeated).
        #[arg(long = "blocked-by", value_delimiter = ',')]
        blocked_by: Vec<String>,
        /// A design-doc reference this card implements (e.g. `PDD005`).
        #[arg(long)]
        design: Option<String>,
        /// Freeform labels (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Insert at the top of To-do (highest priority) rather than the bottom.
        #[arg(long)]
        top: bool,
        /// Card body: literal text, or `-` to read the body from stdin. Omit for
        /// the template skeleton.
        #[arg(long)]
        body: Option<String>,
        /// Skip validation that `--blocked-by` ids exist.
        #[arg(long)]
        force: bool,
    },
    /// View the board or one card, or move a card between lanes.
    Status(StatusArgs),
    /// Reorder a card within its lane (position = priority).
    Prioritize {
        /// Card id or unique prefix.
        id: String,
        /// Move to the top of its lane (highest priority).
        #[arg(long, conflicts_with_all = ["before", "after", "bottom"])]
        top: bool,
        /// Move to the bottom of its lane (lowest priority).
        #[arg(long, conflicts_with_all = ["before", "after"])]
        bottom: bool,
        /// Place immediately before this sibling id.
        #[arg(long, conflicts_with = "after")]
        before: Option<String>,
        /// Place immediately after this sibling id.
        #[arg(long)]
        after: Option<String>,
    },
    /// Check board <-> note consistency.
    Lint {
        /// Apply safe fixes (e.g. prune orphan card lines).
        #[arg(long)]
        fix: bool,
    },
}

/// The `status` surface: view the board (bare, or filtered) or one card, and
/// mutate a card's lane via `set`. The view args and the `set` subcommand are
/// mutually exclusive; card ids are 6-hex, so none collides with `set`.
#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
struct StatusArgs {
    #[command(subcommand)]
    command: Option<StatusCommand>,
    /// Card id or unique prefix. Omit to list the whole board.
    id: Option<String>,
    /// When listing, only cards in this lane.
    #[arg(long, value_enum, conflicts_with = "id")]
    lane: Option<StatusArg>,
    /// When listing, only Available (grabbable) cards: To-do with every
    /// blocker at ready/accepted.
    #[arg(long, conflicts_with_all = ["lane", "id"])]
    available: bool,
}

#[derive(Subcommand)]
enum StatusCommand {
    /// Move a card to a new status (enforces the state machine), or accept the
    /// whole Ready lane with `--accept-all`.
    Set {
        /// Card id or unique prefix. Omit when using `--accept-all`.
        #[arg(required_unless_present = "accept_all")]
        id: Option<String>,
        /// Target status. Omit when using `--accept-all`.
        #[arg(value_enum, required_unless_present = "accept_all")]
        status: Option<StatusArg>,
        /// Accept every card in the Ready lane (ready -> accepted). The
        /// product owner's bulk sign-off; mutually exclusive with id/status.
        #[arg(long, conflicts_with_all = ["id", "status"])]
        accept_all: bool,
        /// Bypass the state-machine transition check. Not meaningful with
        /// --accept-all (ready -> accepted is always legal), so the combo is
        /// rejected rather than silently ignored.
        #[arg(long, conflicts_with = "accept_all")]
        force: bool,
    },
}

/// clap surface for [`project::model::Kind`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum KindArg {
    Feature,
    Bug,
    Chore,
    Docs,
}

/// clap surface for [`project::model::Status`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum StatusArg {
    Todo,
    InProgress,
    NeedsReview,
    Ready,
    Accepted,
    Cancelled,
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
    /// Launch an agent VM to work a project-board card (guards Available-only,
    /// claims it to in-progress, seeds the agent with the card as its directive).
    Dispatch {
        /// Card id or unique prefix.
        card: String,
        /// The project board directory (holds BOARD.md + issues/).
        #[arg(long, default_value = "project/kanban")]
        board_dir: PathBuf,
        /// Dispatch even if the card isn't Available (not To-do, or blocked).
        #[arg(long)]
        force: bool,
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
    /// Capture the instance's composited display to a PNG (requires graphics).
    ///
    /// Runs `grim -` over the existing loopback ssh as the agent user and streams
    /// the PNG back. Note: it captures only the composited sway output — a pure
    /// offscreen Layer-0 workload (its own FBO/swapchain, no surface on the
    /// compositor) screenshots as blank, which is expected, not a bug.
    Screenshot {
        /// Instance name or 1-based index.
        instance: String,
        /// Output path; omit for `screenshot-<instance>-<ts>.png` in the cwd, or
        /// `-` to stream the PNG to stdout.
        path: Option<String>,
    },
}

impl From<KindArg> for project::model::Kind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Feature => project::model::Kind::Feature,
            KindArg::Bug => project::model::Kind::Bug,
            KindArg::Chore => project::model::Kind::Chore,
            KindArg::Docs => project::model::Kind::Docs,
        }
    }
}

impl From<StatusArg> for project::model::Status {
    fn from(s: StatusArg) -> Self {
        use project::model::Status::*;
        match s {
            StatusArg::Todo => Todo,
            StatusArg::InProgress => InProgress,
            StatusArg::NeedsReview => NeedsReview,
            StatusArg::Ready => Ready,
            StatusArg::Accepted => Accepted,
            StatusArg::Cancelled => Cancelled,
        }
    }
}

/// Global flags threaded to every subcommand stub.
#[derive(Copy, Clone)]
struct Global {
    #[allow(dead_code)] // consumed once subcommands grow real output
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
    let result = match cli.domain {
        Domain::Sandbox(args) => sandbox::dispatch(args, global),
        Domain::Project(args) => project::dispatch(args, global),
    };
    // A `Reported` failure was already rendered by the subcommand (e.g. the
    // prompt stream's `Lost` note): exit nonzero without anyhow re-printing.
    if let Err(e) = &result {
        if e.is::<sandbox::output::Reported>() {
            std::process::exit(1);
        }
    }
    result
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
    /// on index-vs-name.
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

    /// `screenshot` takes an instance and an optional path (defaulting later).
    #[test]
    fn screenshot_instance_and_optional_path_parse() {
        let with_path = parse(&[
            "katsuctl",
            "sandbox",
            "--config",
            "/x",
            "screenshot",
            "my-inst",
            "shot.png",
        ]);
        match with_path.domain {
            Domain::Sandbox(SandboxArgs {
                command: SandboxCommand::Screenshot { instance, path },
                ..
            }) => {
                assert_eq!(instance, "my-inst");
                assert_eq!(path.as_deref(), Some("shot.png"));
            }
            _ => panic!("expected sandbox screenshot"),
        }

        let no_path = parse(&[
            "katsuctl",
            "sandbox",
            "--config",
            "/x",
            "screenshot",
            "my-inst",
        ]);
        match no_path.domain {
            Domain::Sandbox(SandboxArgs {
                command: SandboxCommand::Screenshot { instance, path },
                ..
            }) => {
                assert_eq!(instance, "my-inst");
                assert_eq!(path, None);
            }
            _ => panic!("expected sandbox screenshot"),
        }
    }

    /// `--config` is mandatory for the sandbox domain.
    #[test]
    fn missing_config_is_an_error() {
        assert!(Cli::try_parse_from(["katsuctl", "sandbox", "status"]).is_err());
    }

    /// `project status set <id> <to>` routes to the Set subcommand, not the
    /// positional view.
    #[test]
    fn project_status_set_routes_to_set_subcommand() {
        let cli = parse(&["katsuctl", "project", "status", "set", "abc123", "ready"]);
        match cli.domain {
            Domain::Project(ProjectArgs {
                command:
                    ProjectCommand::Status(StatusArgs {
                        command: Some(StatusCommand::Set { id, status, .. }),
                        ..
                    }),
                ..
            }) => {
                assert_eq!(id.as_deref(), Some("abc123"));
                assert_eq!(status, Some(StatusArg::Ready));
            }
            _ => panic!("expected project status set"),
        }
    }

    /// `--accept-all` and `--force` are mutually exclusive: --force is a no-op
    /// on the always-legal ready -> accepted bulk move, so the combo is rejected.
    #[test]
    fn accept_all_and_force_conflict() {
        assert!(Cli::try_parse_from([
            "katsuctl",
            "project",
            "status",
            "set",
            "--accept-all",
            "--force",
        ])
        .is_err());
    }

    /// A bare `project status <id>` is the positional detail view, not a
    /// subcommand — 6-hex ids never collide with `set`.
    #[test]
    fn project_status_id_routes_to_positional_detail() {
        let cli = parse(&["katsuctl", "project", "status", "abc123"]);
        match cli.domain {
            Domain::Project(ProjectArgs {
                command:
                    ProjectCommand::Status(StatusArgs {
                        command: None, id, ..
                    }),
                ..
            }) => assert_eq!(id.as_deref(), Some("abc123")),
            _ => panic!("expected project status positional detail"),
        }
    }

    /// The positional id and the `--lane` filter are mutually exclusive.
    #[test]
    fn project_status_id_and_lane_conflict() {
        assert!(Cli::try_parse_from(
            ["katsuctl", "project", "status", "abc123", "--lane", "todo",]
        )
        .is_err());
    }

    /// `project show` is not a recognized subcommand.
    #[test]
    fn project_show_is_unrecognized() {
        assert!(Cli::try_parse_from(["katsuctl", "project", "show"]).is_err());
    }
}
