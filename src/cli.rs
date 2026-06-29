use super::build_info;
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "forklift",
    about = "Manage a jj-native stacked PR workflow",
    version = build_info::VERSION,
    long_version = build_info::LONG_VERSION
)]
pub(crate) struct Cli {
    #[arg(short, long, global = true)]
    pub(crate) verbose: bool,

    #[arg(long, global = true)]
    pub(crate) dry_run: bool,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    Submit(SubmitOptions),
    Sync(SyncOptions),
    Merge(MergeOptions),
    Get(GetOptions),
    Repair(RepairOptions),
    Unfreeze(UnfreezeOptions),
    Status(StatusOptions),
    /// Adopt an existing branch and its open PR into forklift's tracked set.
    Track(TrackOptions),
    /// Open a pull request in your browser.
    Pr(PrOptions),
    /// Open the `jjui` terminal UI, filtered to tracked stacks by default.
    Ui(UiOptions),
    /// Any other subcommand is passed straight through to `jj`.
    #[command(external_subcommand)]
    External(Vec<String>),
}

impl Commands {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Submit(_) => "submit",
            Self::Sync(_) => "sync",
            Self::Merge(_) => "merge",
            Self::Get(_) => "get",
            Self::Repair(_) => "repair",
            Self::Unfreeze(_) => "unfreeze",
            Self::Status(_) => "status",
            Self::Track(_) => "track",
            Self::Pr(_) => "pr",
            Self::Ui(_) => "ui",
            Self::External(_) => "jj",
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct SubmitOptions {
    /// Apply submit without prompting for confirmation.
    #[arg(short, long)]
    pub(crate) yes: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SyncOptions {
    pub(crate) target: Option<String>,

    /// Also run submit after syncing. Sync does not submit by default.
    #[arg(long)]
    pub(crate) submit: bool,

    /// Apply submit without prompting for confirmation when --submit is used.
    #[arg(short, long)]
    pub(crate) yes: bool,
}

#[derive(Debug, Args)]
pub(crate) struct MergeOptions {
    pub(crate) target: Option<String>,

    /// Run sync with submit before merging, using the same optional target.
    #[arg(long)]
    pub(crate) sync: bool,

    /// Merge even if a PR is not approved, overriding the require-approval check.
    #[arg(long)]
    pub(crate) no_require_approval: bool,

    /// Admin override: skip the pre-flight mergeability gate (approval, blocked
    /// status, status checks) so the fast-forward push proceeds anyway. Implies
    /// --no-require-approval. Requires admin rights to push to a protected trunk.
    #[arg(long)]
    pub(crate) admin: bool,
}

#[derive(Debug, Args)]
pub(crate) struct GetOptions {
    pub(crate) target: String,

    /// Do not move the working copy to a new editable change after fetching.
    #[arg(long)]
    pub(crate) no_edit: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RepairOptions {
    pub(crate) target: String,

    /// Apply the repair without prompting for confirmation.
    #[arg(short, long)]
    pub(crate) yes: bool,
}

#[derive(Debug, Args)]
pub(crate) struct UnfreezeOptions {
    pub(crate) target: String,
}

#[derive(Debug, Args)]
pub(crate) struct TrackOptions {
    /// PR number, GitHub PR URL, branch name, or change id prefix to adopt.
    pub(crate) target: String,
}

#[derive(Debug, Args)]
pub(crate) struct PrOptions {
    /// PR number, GitHub PR URL, branch name, or change id prefix.
    /// Defaults to the PR for the current change (`@`).
    pub(crate) target: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct StatusOptions {
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct UiOptions {
    /// Override the default revset filter. Defaults to the tracked-stacks view.
    #[arg(short = 'r', long)]
    pub(crate) revset: Option<String>,

    /// Show every revision instead of only tracked stacks (lets `jjui` use its
    /// own default revset).
    #[arg(long)]
    pub(crate) all: bool,

    /// Extra arguments forwarded verbatim to `jjui`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
}
