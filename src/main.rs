use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use owo_colors::OwoColorize;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use unicode_normalization::UnicodeNormalization;

use forklift::empty_string_to_none;

/// Global toggle controlling whether the user-facing `ui_*` macros emit ANSI
/// color escapes. Set once via [`init_ui`]; defaults to colored output.
static UI_COLOR: OnceLock<bool> = OnceLock::new();

const FORKLIFT_LOG_ENV: &str = "FORKLIFT_LOG";
const FORKLIFT_LOG_STDERR_ENV: &str = "FORKLIFT_LOG_STDERR";
const DEFAULT_FILE_LOG_FILTER: &str = "warn,forklift=debug";
const DEFAULT_STDERR_LOG_FILTER: &str = "info,forklift=debug";

/// Returns whether the user-facing status macros should emit ANSI color.
///
/// Defaults to `true` when [`init_ui`] has not been called so that early output
/// is still styled on terminals.
#[tracing::instrument(skip_all)]
fn ui_color_enabled() -> bool {
    *UI_COLOR.get().unwrap_or(&true)
}

/// Initializes the global color toggle for the `ui_*` status macros.
///
/// Pass the desired color setting; callers typically derive it from
/// `std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()`.
#[tracing::instrument(skip_all)]
fn init_ui(color: bool) {
    let _ = UI_COLOR.set(color);
}

/// Keeps the non-blocking file writer alive until the command exits.
struct TraceLog {
    path: Option<PathBuf>,
    _guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl TraceLog {
    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// Initializes the global `tracing` subscriber.
///
/// By default, trace events are written to a per-run log file using
/// `warn,forklift=debug`. Set `FORKLIFT_LOG` to override the file log filter,
/// or set it to `off`/`false`/`0` to disable file logging. Stderr tracing is
/// opt-in via `FORKLIFT_LOG_STDERR`.
#[tracing::instrument(skip_all)]
fn init_tracing(command_name: &str) -> TraceLog {
    let (file_writer, file_guard, log_path) = if file_logging_enabled() {
        match open_debug_log(command_name) {
            Ok((path, file)) => {
                let (writer, guard) = tracing_appender::non_blocking(file);
                (Some(writer), Some(guard), Some(path))
            }
            Err(error) => {
                eprintln!("warning: failed to create forklift debug log: {error:#}");
                (None, None, None)
            }
        }
    } else {
        (None, None, None)
    };

    let file_filter = env_filter_or_default(FORKLIFT_LOG_ENV, DEFAULT_FILE_LOG_FILTER);
    let file_layer = file_writer.map(|writer| {
        tracing_logfmt::builder()
            .layer()
            .with_writer(writer)
            .with_filter(file_filter)
    });

    let stderr_layer = stderr_filter().map(|filter| {
        tracing_logfmt::builder()
            .layer()
            .with_writer(std::io::stderr)
            .with_filter(filter)
    });

    let initialized = tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .is_ok();

    if initialized {
        TraceLog {
            path: log_path,
            _guard: file_guard,
        }
    } else {
        TraceLog {
            path: None,
            _guard: None,
        }
    }
}

fn file_logging_enabled() -> bool {
    match env::var(FORKLIFT_LOG_ENV) {
        Ok(value) => !is_false_env_value(&value),
        Err(_) => !cfg!(test),
    }
}

fn stderr_filter() -> Option<EnvFilter> {
    let value = env::var(FORKLIFT_LOG_STDERR_ENV).ok()?;
    if is_false_env_value(&value) {
        return None;
    }
    if is_true_env_value(&value) {
        return Some(EnvFilter::new(DEFAULT_STDERR_LOG_FILTER));
    }
    Some(env_filter_from_value(
        FORKLIFT_LOG_STDERR_ENV,
        &value,
        DEFAULT_STDERR_LOG_FILTER,
    ))
}

fn env_filter_or_default(name: &str, default_filter: &str) -> EnvFilter {
    match env::var(name) {
        Ok(value) => env_filter_from_value(name, &value, default_filter),
        Err(_) => EnvFilter::new(default_filter),
    }
}

fn env_filter_from_value(name: &str, value: &str, default_filter: &str) -> EnvFilter {
    EnvFilter::try_new(value).unwrap_or_else(|error| {
        eprintln!("warning: invalid {name}={value:?}; using {default_filter:?}: {error}");
        EnvFilter::new(default_filter)
    })
}

fn is_false_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

fn is_true_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn open_debug_log(command_name: &str) -> Result<(PathBuf, File)> {
    let log_dir = debug_log_dir();
    fs::create_dir_all(&log_dir)
        .with_context(|| format!("create debug log directory {}", log_dir.display()))?;
    let path = log_dir.join(debug_log_filename(command_name));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("create debug log {}", path.display()))?;
    Ok((path, file))
}

fn debug_log_dir() -> PathBuf {
    env::current_dir()
        .ok()
        .and_then(|cwd| discover_jj_repo_dir(&cwd))
        .map(|repo_dir| repo_dir.join(CONFIG_PREFIX).join("logs"))
        .unwrap_or_else(|| xdg_state_home().join("forklift").join("logs"))
}

fn discover_jj_repo_dir(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|path| path.join(".jj").exists())
        .and_then(|workspace_root| resolve_jj_repo_dir(workspace_root).ok())
}

fn xdg_state_home() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(env::temp_dir)
}

fn debug_log_filename(command_name: &str) -> String {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("{timestamp_ms}-{}-{command_name}.log", process::id())
}

/// Emits an "info" status line to stdout.
macro_rules! ui_info {
    ($($arg:tt)*) => {{
        let __msg = format!($($arg)*);
        ui_info_line(&__msg);
    }};
}

/// Emits a "warning" status line to stderr.
macro_rules! ui_warn {
    ($($arg:tt)*) => {{
        let __msg = format!($($arg)*);
        ui_warn_line(&__msg);
    }};
}

/// Width of the right-aligned status verb column. Matches cargo's gutter so
/// output lines up under a familiar `   Compiling ...` shape.
const PROGRESS_VERB_WIDTH: usize = 12;

/// Emits a cargo-style progress line to stderr: a right-aligned bold-green
/// verb in a fixed-width gutter, followed by a message. Append-only — never
/// rewrites or clears lines. Goes to stderr so stdout stays clean for machine
/// output (`status --json`, follow-up command hints).
fn ui_progress(verb: &str, message: &str) {
    // Pad the plain verb first, then color, so ANSI escapes don't throw off the
    // alignment width.
    let padded = format!("{verb:>width$}", width = PROGRESS_VERB_WIDTH);
    if ui_color_enabled() {
        eprintln!("{} {message}", padded.green().bold());
    } else {
        eprintln!("{padded} {message}");
    }
}

/// Emits a red `error:` line to stderr for a human-readable failure headline.
fn ui_error(message: &str) {
    if ui_color_enabled() {
        eprintln!("{} {message}", "error:".red().bold());
    } else {
        eprintln!("error: {message}");
    }
}

/// Emits a dimmed `hint:` line suggesting a safe next command.
fn ui_hint(message: &str) {
    if ui_color_enabled() {
        eprintln!("{} {message}", "hint:".cyan().bold());
    } else {
        eprintln!("hint: {message}");
    }
}

fn ui_info_line(message: &str) {
    let padded = format!("{:>width$}", "Info", width = PROGRESS_VERB_WIDTH);
    if ui_color_enabled() {
        println!("{} {message}", padded.cyan().bold());
    } else {
        println!("{padded} {message}");
    }
}

fn ui_warn_line(message: &str) {
    let padded = format!("{:>width$}", "Warning", width = PROGRESS_VERB_WIDTH);
    if ui_color_enabled() {
        eprintln!("{} {message}", padded.yellow().bold());
    } else {
        eprintln!("{padded} {message}");
    }
}

fn ui_progress_bar(verb: &str, message: &str, total: usize) -> Option<ProgressBar> {
    if total == 0 || !std::io::stderr().is_terminal() {
        return None;
    }
    let progress = ProgressBar::new(total as u64);
    progress.set_draw_target(ProgressDrawTarget::stderr_with_hz(10));
    progress.set_prefix(format!("{verb:>width$}", width = PROGRESS_VERB_WIDTH));
    progress.set_message(message.to_owned());

    let template = if ui_color_enabled() {
        "{prefix:.green.bold} {msg} [{bar:18}] {pos}/{len}"
    } else {
        "{prefix} {msg} [{bar:18}] {pos}/{len}"
    };
    let style = ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ");
    progress.set_style(style);
    progress.set_position(0);
    progress.force_draw();
    Some(progress)
}

fn ui_finish_progress_bar(progress: ProgressBar) {
    progress.force_draw();
    progress.finish();
}

/// Maps an internal recovery-phase id to a cargo-style `(verb, message)` pair
/// for progress output. The verb is a present participle shown in the gutter;
/// the message names what is being acted on. Unknown phases fall back to the
/// raw id so new phases still surface something useful.
fn phase_label(phase: &str) -> (&'static str, &str) {
    match phase {
        "resolve-github" => ("Resolving", "GitHub repository"),
        "resolve-stack" => ("Resolving", "stack"),
        "resolve-stack-comment" => ("Resolving", "stack comment"),
        "resolve-prs" => ("Resolving", "pull requests"),
        "resolve-target" => ("Resolving", "target"),
        "resolve-fetched-heads" => ("Resolving", "fetched heads"),
        "plan-submit" => ("Planning", "submit"),
        "validate-submit-bases" => ("Validating", "submit bases"),
        "validate-frozen" => ("Validating", "frozen bookmarks"),
        "verify-mutable" => ("Verifying", "mutable changes"),
        "verify-merge" => ("Verifying", "merge"),
        "merge-pr-check" => ("Checking", "merge readiness"),
        "status-aliases" => ("Checking", "jj aliases"),
        "fetch-branch" => ("Fetching", "branch"),
        "fetch-stack" => ("Fetching", "stack"),
        "sync-fetch" => ("Fetching", "trunk"),
        "push-refs" => ("Pushing", "bookmarks"),
        "track-branch" => ("Tracking", "branch"),
        "track-blockers" => ("Tracking", "immutable blockers"),
        "stack-comments" => ("Updating", "stack comments"),
        "rebase-stack" => ("Rebasing", "stack"),
        "move-trunk" => ("Moving", "trunk"),
        "merge-push" => ("Merging", "fast-forward push"),
        "merge-refresh-above" => ("Refreshing", "stack above merge"),
        "freeze-stack" => ("Freezing", "stack bookmarks"),
        "sync-frozen" => ("Syncing", "frozen bookmarks"),
        "remove-frozen" => ("Removing", "frozen bookmarks"),
        "reset-working-copy" => ("Resetting", "working copy"),
        "sync-submit" => ("Submitting", "stack"),
        "cleanup-branches" => ("Cleaning", "branches"),
        "cleanup-merged" => ("Cleaning", "merged branches"),
        other => ("Running", other),
    }
}

const CONFIG_PREFIX: &str = "stack";
const DEFAULT_REMOTE: &str = "origin";
const DEFAULT_TRUNK: &str = "main";
const DEFAULT_REQUIRE_APPROVAL: bool = true;
const DEFAULT_BRANCH_PREFIX: &str = "stack";
const DEFAULT_STACK_REVSET: &str = "trunk()..@ & ~::(immutable_heads() | root()) & ~empty()";
const STACK_FIELD_SEPARATOR: char = '\x1f';
const STACK_RECORD_SEPARATOR: char = '\x1e';
const STACK_LOG_TEMPLATE: &str = "json(change_id) ++ \"\\x1f\" ++ json(commit_id) ++ \"\\x1f\" ++ json(parents.map(|c| c.commit_id())) ++ \"\\x1f\" ++ json(description.first_line()) ++ \"\\x1f\" ++ json(description) ++ \"\\x1f\" ++ json(empty) ++ \"\\x1f\" ++ json(conflict) ++ \"\\x1e\"";
const PR_JSON_FIELDS: &str = "number,state,headRefName,baseRefName,headRefOid,baseRefOid,title,body,createdAt,id,author,headRepository";
const MERGE_PR_JSON_FIELDS: &str = "number,state,headRefName,baseRefName,headRefOid,baseRefOid,title,body,createdAt,isDraft,reviewDecision,mergeable,mergeStateStatus,statusCheckRollup,autoMergeRequest";
const PR_API_JQ: &str = "{number,state,merged,headRefName:.head.ref,baseRefName:.base.ref,headRefOid:.head.sha,baseRefOid:.base.sha,title,body,createdAt:.created_at,id:.node_id,headRepository:{id:(.head.repo.id|tostring),node_id:.head.repo.node_id,nameWithOwner:.head.repo.full_name},baseRepository:{id:(.base.repo.id|tostring),node_id:.base.repo.node_id,nameWithOwner:.base.repo.full_name},author:{login:.user.login}}";
const STACK_COMMENT_MARKER: &str = "<!-- stack:v1 -->";
const STACK_COMMENT_JQ: &str =
    ".[] | {id,body,userLogin:.user.login,updatedAt:.updated_at} | @json";
const JJ_CONFIG_FROZEN_ALIAS_KEY: &str = "revset-aliases.\"forklift_frozen_heads()\"";
const JJ_CONFIG_IMMUTABLE_ALIAS_KEY: &str = "revset-aliases.\"immutable_heads()\"";
const JJ_CONFIG_BASE_IMMUTABLE_ALIAS_KEY: &str =
    "revset-aliases.\"forklift_base_immutable_heads()\"";
const JJ_FROZEN_ALIAS_VALUE: &str = "bookmarks(glob:'forklift/frozen/*')";
const JJ_DEFAULT_IMMUTABLE_ALIAS_VALUE: &str = "builtin_immutable_heads()";
const JJ_REQUIRED_IMMUTABLE_ALIAS_VALUE: &str =
    "builtin_immutable_heads() | forklift_frozen_heads()";
const JJ_WRAPPED_IMMUTABLE_ALIAS_VALUE: &str =
    "forklift_base_immutable_heads() | forklift_frozen_heads()";
const BOOKMARK_STATUS_TEMPLATE: &str = "remote ++ \"\\t\" ++ if(tracked, \"tracked\", \"untracked\") ++ \"\\t\" ++ if(conflict, \"conflicted\", \"ok\") ++ \"\\n\"";
const REMOTE_BOOKMARK_TEMPLATE: &str = "name ++ \"\\t\" ++ remote ++ \"\\t\" ++ if(tracked, \"tracked\", \"untracked\") ++ \"\\t\" ++ if(conflict, \"conflicted\", \"ok\") ++ \"\\t\" ++ if(conflict, \"\", normal_target.commit_id()) ++ \"\\n\"";
const LOCAL_BOOKMARK_TEMPLATE: &str = "name ++ \"\\t\" ++ remote ++ \"\\n\"";
const FROZEN_BOOKMARK_TEMPLATE: &str = "name ++ \"\\t\" ++ if(conflict, \"conflicted\", \"ok\") ++ \"\\t\" ++ if(conflict, \"\", normal_target.commit_id()) ++ \"\\n\"";
const FROZEN_BOOKMARK_PREFIX: &str = "forklift/frozen/pr-";

#[derive(Debug, Parser)]
#[command(name = "forklift", about = "Manage a jj-native stacked PR workflow")]
struct Cli {
    #[arg(short, long, global = true)]
    verbose: bool,

    #[arg(long, global = true)]
    dry_run: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Submit(SubmitOptions),
    Sync(SyncOptions),
    Merge(MergeOptions),
    Get(GetOptions),
    Repair(RepairOptions),
    Unfreeze(UnfreezeOptions),
    Status(StatusOptions),
    /// Open a pull request in your browser.
    Pr(PrOptions),
}

impl Commands {
    fn name(&self) -> &'static str {
        match self {
            Self::Submit(_) => "submit",
            Self::Sync(_) => "sync",
            Self::Merge(_) => "merge",
            Self::Get(_) => "get",
            Self::Repair(_) => "repair",
            Self::Unfreeze(_) => "unfreeze",
            Self::Status(_) => "status",
            Self::Pr(_) => "pr",
        }
    }
}

#[derive(Debug, Args)]
struct SubmitOptions {
    /// Apply submit without prompting for confirmation.
    #[arg(short, long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct SyncOptions {
    /// Also run submit after syncing. Sync does not submit by default.
    #[arg(long)]
    submit: bool,

    /// Apply submit without prompting for confirmation when --submit is used.
    #[arg(short, long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct MergeOptions {
    target: Option<String>,

    /// Merge even if a PR is not approved, overriding the require-approval check.
    #[arg(long)]
    no_require_approval: bool,

    /// Admin override: skip the pre-flight mergeability gate (approval, blocked
    /// status, status checks) so the fast-forward push proceeds anyway. Implies
    /// --no-require-approval. Requires admin rights to push to a protected trunk.
    #[arg(long)]
    admin: bool,
}

#[derive(Debug, Args)]
struct GetOptions {
    target: String,

    /// Do not move the working copy to a new editable change after fetching.
    #[arg(long)]
    no_edit: bool,
}

#[derive(Debug, Args)]
struct RepairOptions {
    target: String,

    /// Apply the repair without prompting for confirmation.
    #[arg(short, long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct UnfreezeOptions {
    target: String,
}

#[derive(Debug, Args)]
struct PrOptions {
    /// PR number, GitHub PR URL, branch name, or change id prefix.
    /// Defaults to the PR for the current change (`@`).
    target: Option<String>,
}

#[derive(Debug, Args)]
struct StatusOptions {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppConfig {
    remote: String,
    trunk: String,
    require_approval: bool,
    branch_prefix: String,
}

impl AppConfig {
    #[tracing::instrument(skip_all)]
    fn resolve(runner: &impl CommandRunner) -> Result<Self> {
        let remote = resolve_string_config(runner, "remote", DEFAULT_REMOTE);
        let trunk = resolve_string_config(runner, "trunk", DEFAULT_TRUNK);
        let branch_prefix = resolve_string_config(runner, "branch-prefix", DEFAULT_BRANCH_PREFIX);
        Ok(Self {
            remote: validate_ref_component("remote", remote)?,
            trunk: validate_ref_component("trunk", trunk)?,
            require_approval: resolve_bool_config(
                runner,
                "require-approval",
                DEFAULT_REQUIRE_APPROVAL,
            )?,
            branch_prefix: validate_ref_component("branch-prefix", branch_prefix)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubContext {
    repo: String,
    username: String,
}

impl GitHubContext {
    #[tracing::instrument(skip_all)]
    fn resolve(runner: &impl CommandRunner) -> Result<Self> {
        let repo = gh_run_required(
            runner,
            &[
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "--jq",
                ".nameWithOwner",
            ],
        )
        .context("resolve GitHub repository with gh")?;
        let username = gh_run_required(runner, &["api", "user", "--jq", ".login"])
            .context("resolve GitHub username with gh")?;

        Ok(Self { repo, username })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput>;

    /// Like `run`, but executes with `cwd` as the child process's working
    /// directory. Default impl ignores `cwd` and delegates to `run`, which is
    /// fine for test fakes that don't care about which directory they're
    /// invoked from.
    fn run_in_dir(&self, program: &str, args: &[&str], _cwd: &Path) -> Result<CommandOutput> {
        self.run(program, args)
    }
}

struct SystemRunner;

impl CommandRunner for SystemRunner {
    #[tracing::instrument(skip_all)]
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("run `{}`", display_command(program, args)))?;

        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    #[tracing::instrument(skip_all)]
    fn run_in_dir(&self, program: &str, args: &[&str], cwd: &Path) -> Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("run `{}`", display_command(program, args)))?;

        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[tracing::instrument(skip_all)]
fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    // `run` prints a human-readable failure itself; main only sets the exit code
    // so anyhow doesn't also dump the raw structured error to the terminal.
    match run(cli, &SystemRunner) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

#[tracing::instrument(skip_all)]
fn run(cli: Cli, runner: &impl CommandRunner) -> Result<()> {
    let command_name = cli.command.name();
    let trace_log = init_tracing(command_name);
    init_ui(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());
    let cwd = env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_owned());
    tracing::debug!(
        command = command_name,
        cwd,
        log = trace_log
            .path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<disabled>".to_owned()),
        "command start"
    );

    let result = run_command(cli, runner, &cwd);
    match &result {
        Ok(()) => tracing::debug!(command = command_name, "command complete"),
        Err(error) => {
            tracing::error!(command = command_name, error = %error, "command failed");
            render_cli_error(error, trace_log.path());
        }
    }

    result
}

#[tracing::instrument(skip_all)]
fn run_command(cli: Cli, runner: &impl CommandRunner, cwd: &str) -> Result<()> {
    ensure_jj_repo(runner, cwd).map_err(|error| phase_error("preflight", "jj repo", error))?;
    let config = AppConfig::resolve(runner)
        .map_err(|error| phase_error("resolve-config", "configuration", error))?;
    let diagnostics = Diagnostics {
        verbose: cli.verbose,
        dry_run: cli.dry_run,
    };
    ensure_startup_config(runner, diagnostics)
        .map_err(|error| phase_error("startup-config", "jj config", error))?;

    if cli.verbose {
        eprintln!(
            "resolved config: remote={}, trunk={}, require-approval={}, branch-prefix={}",
            config.remote, config.trunk, config.require_approval, config.branch_prefix
        );
    }

    match cli.command {
        Commands::Submit(options) => {
            let context = resolve_stack_context(runner, DEFAULT_STACK_REVSET)
                .map_err(|error| phase_error("resolve-stack", DEFAULT_STACK_REVSET, error))?;
            if cli.verbose {
                print_github_context(&context.github);
                print_stack(&context.stack);
            }
            let summary = submit_stack(
                runner,
                &config,
                &context,
                options.yes,
                "forklift submit --yes",
                diagnostics,
            )?;
            if cli.verbose {
                eprintln!(
                    "submit: {} pushed, {} created, {} updated, {} unchanged, {} comments created, {} comments updated, {} comments unchanged",
                    summary.pushed_refs,
                    summary.created_prs,
                    summary.updated_prs,
                    summary.unchanged_prs,
                    summary.created_comments,
                    summary.updated_comments,
                    summary.unchanged_comments
                );
            }
            if cli.dry_run {
                ui_progress(
                    "Finished",
                    &format!(
                        "submit (dry run) — {} changes, {} pushes, {} creates, {} updates planned",
                        context.stack.len(),
                        summary.pushed_refs,
                        summary.created_prs,
                        summary.updated_prs
                    ),
                );
                return Ok(());
            }
            ui_progress(
                "Finished",
                &format!(
                    "submit — {} changes, {} pushed, {} created, {} updated",
                    context.stack.len(),
                    summary.pushed_refs,
                    summary.created_prs,
                    summary.updated_prs
                ),
            );
        }
        Commands::Sync(options) => {
            let summary = sync_stack(
                runner,
                &config,
                DEFAULT_STACK_REVSET,
                options.submit,
                options.yes,
                diagnostics,
            )?;
            if cli.dry_run {
                ui_progress(
                    "Finished",
                    &format!(
                        "sync (dry run) — {} roots, submit {}, {} merged branch(es) to clean",
                        summary.rebased_roots,
                        if summary.submit_ran {
                            "planned"
                        } else {
                            "skipped"
                        },
                        summary.cleaned_branches
                    ),
                );
            } else {
                ui_progress(
                    "Finished",
                    &format!(
                        "sync — {} roots rebased, submit {}, {} merged branch(es) cleaned",
                        summary.rebased_roots,
                        if summary.submit_ran { "ran" } else { "skipped" },
                        summary.cleaned_branches
                    ),
                );
            }
        }
        Commands::Merge(options) => {
            let mut merge_config = config.clone();
            if options.no_require_approval || options.admin {
                merge_config.require_approval = false;
            }
            let target_label = options.target.as_deref().unwrap_or(DEFAULT_STACK_REVSET);
            let merge_revset = effective_merge_revset(runner, options.target.as_deref())
                .map_err(|error| phase_error("resolve-merge-target", target_label, error))?;
            let summary = merge_stack(
                runner,
                &merge_config,
                &merge_revset.revset,
                merge_revset.target.as_ref(),
                options.admin,
                diagnostics,
            )?;
            // A targeted merge only re-submits the merged range, so PRs *above*
            // the target still list the now-merged PRs in their stack comments
            // (and their branches were rebased when the merged changes were
            // abandoned). Refresh the full stack so the merged PRs drop out of
            // those comments and the rebased branches are republished. The
            // no-target merge already refreshes remaining PRs each iteration.
            if !cli.dry_run && summary.merged_prs > 0 && options.target.is_some() {
                refresh_stack_above_merge(runner, &config, DEFAULT_STACK_REVSET, diagnostics)
                    .map_err(|error| {
                        phase_error("merge-refresh-above", DEFAULT_STACK_REVSET, error)
                    })?;
            }
            if cli.verbose {
                eprintln!(
                    "merge: {} merged, {} local updates, {} submits, {} branches cleaned",
                    summary.merged_prs,
                    summary.local_updates,
                    summary.submit_runs,
                    summary.cleaned_branches
                );
            }
            if cli.dry_run {
                ui_progress(
                    "Finished",
                    &format!("merge (dry run) — {} PRs checked", summary.checked_prs),
                );
            } else {
                ui_progress(
                    "Finished",
                    &format!(
                        "merge — {} PRs merged, {} branches cleaned",
                        summary.merged_prs, summary.cleaned_branches
                    ),
                );
            }
        }
        Commands::Get(options) => {
            let summary = get_stack(
                runner,
                &config,
                &options.target,
                !options.no_edit,
                diagnostics,
            )?;
            if cli.dry_run {
                ui_progress(
                    "Finished",
                    &format!(
                        "get (dry run) — {} PRs, {} branches planned",
                        summary.prs, summary.fetched_branches
                    ),
                );
            } else {
                ui_progress(
                    "Finished",
                    &format!(
                        "get — {} PRs fetched, {} cache entries written{}",
                        summary.prs,
                        summary.cache_entries,
                        if summary.edited {
                            ", editing new change"
                        } else {
                            ""
                        }
                    ),
                );
            }
        }
        Commands::Repair(options) => {
            let summary =
                repair_stack_comments(runner, &config, &options.target, options.yes, diagnostics)?;
            if cli.dry_run {
                ui_progress(
                    "Finished",
                    &format!(
                        "repair (dry run) — {} open PRs, {} merged PRs to prune, {} comments planned",
                        summary.open_prs, summary.pruned_merged_prs, summary.comments_changed
                    ),
                );
            } else {
                ui_progress(
                    "Finished",
                    &format!(
                        "repair — {} open PRs, {} merged PRs pruned, {} comments changed",
                        summary.open_prs, summary.pruned_merged_prs, summary.comments_changed
                    ),
                );
            }
        }
        Commands::Unfreeze(options) => {
            let pr_number = unfreeze_stack(runner, &config, &options.target, diagnostics)?;
            if cli.dry_run {
                ui_progress(
                    "Finished",
                    &format!("unfreeze (dry run) — PR #{pr_number} planned"),
                );
            } else {
                ui_progress("Finished", &format!("unfreeze — PR #{pr_number} adopted"));
            }
        }
        Commands::Status(options) => {
            let report = status_report(runner, &config, DEFAULT_STACK_REVSET, diagnostics)
                .map_err(|error| phase_error("status", DEFAULT_STACK_REVSET, error))?;
            if options.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).context("serialize status json")?
                );
            } else {
                print_status_report(&report);
            }
        }
        Commands::Pr(options) => {
            let target_label = options.target.as_deref().unwrap_or("@");
            let github = GitHubContext::resolve(runner)
                .map_err(|error| phase_error("resolve-github", target_label, error))?;
            let (number, url) = resolve_pr_url(runner, &github, options.target.as_deref())
                .map_err(|error| phase_error("resolve-pr", target_label, error))?;
            if cli.dry_run {
                ui_progress(
                    "Finished",
                    &format!("pr (dry run) — would open PR #{number} at {url}"),
                );
                return Ok(());
            }
            ui_progress("Opening", &format!("PR #{number} — {url}"));
            open_url(runner, &url).map_err(|error| phase_error("open-pr", &url, error))?;
        }
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn phase_error(phase: &str, object: impl Display, error: anyhow::Error) -> anyhow::Error {
    let object = object.to_string();
    let inner = diagnostic_from_error(&error);
    let mut cli_error = CliError::new(phase_summary(phase, &object))
        .reason(reason_from_error(&error, &inner))
        .resolution(inner.resolution.unwrap_or_else(|| {
            "run `forklift submit --dry-run` to preview the stack state".to_owned()
        }))
        .detail("phase", phase)
        .detail("object", object);
    cli_error.details.extend(inner.details);
    anyhow::Error::new(cli_error)
}

fn reason_from_error(error: &anyhow::Error, diagnostic: &CliError) -> String {
    if error.chain().count() > 1 {
        return format!("{error:#}");
    }
    diagnostic
        .reason
        .clone()
        .unwrap_or_else(|| diagnostic.message.clone())
}

#[derive(Debug, Clone)]
struct CliError {
    message: String,
    reason: Option<String>,
    resolution: Option<String>,
    details: Vec<(&'static str, String)>,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            reason: None,
            resolution: None,
            details: Vec::new(),
        }
    }

    fn reason(mut self, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        if !reason.trim().is_empty() {
            self.reason = Some(reason);
        }
        self
    }

    fn resolution(mut self, resolution: impl Into<String>) -> Self {
        let resolution = resolution.into();
        if !resolution.trim().is_empty() {
            self.resolution = Some(resolution);
        }
        self
    }

    fn detail(mut self, key: &'static str, value: impl Display) -> Self {
        let value = value.to_string();
        if !value.trim().is_empty() {
            self.details.push((key, value));
        }
        self
    }
}

impl Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CliError {}

fn phase_summary(phase: &str, object: &str) -> String {
    match phase {
        "resolve-config" => "could not resolve configuration".to_owned(),
        "startup-config" => "could not prepare jj config".to_owned(),
        "resolve-stack" => format!("could not resolve stack `{object}`"),
        "resolve-merge-target" => format!("could not resolve merge target `{object}`"),
        "merge-refresh-above" => format!("could not refresh stack above `{object}`"),
        "resolve-github" => format!("could not resolve GitHub context for `{object}`"),
        "resolve-pr" => format!("could not resolve PR for `{object}`"),
        "open-pr" => format!("could not open PR URL `{object}`"),
        "status" => format!("could not build status for `{object}`"),
        _ => format!("failed during {phase}"),
    }
}

fn render_cli_error(error: &anyhow::Error, debug_log: Option<&Path>) {
    let mut diagnostic = diagnostic_from_error(error);
    if let Some(path) = debug_log {
        diagnostic
            .details
            .retain(|(key, _)| *key != "debug log" && *key != "log");
        diagnostic
            .details
            .push(("debug log", path.display().to_string()));
    }

    print_error_line(&diagnostic.message);
    print_section("reason", diagnostic.reason.as_deref());
    print_section("resolution", diagnostic.resolution.as_deref());
    print_details(&diagnostic.details);
}

fn diagnostic_from_error(error: &anyhow::Error) -> CliError {
    for cause in error.chain() {
        if let Some(cli_error) = cause.downcast_ref::<CliError>() {
            return cli_error.clone();
        }
    }

    let mut chain = error.chain();
    let message = chain
        .next()
        .map(ToString::to_string)
        .unwrap_or_else(|| "command failed".to_owned());
    let mut diagnostic = diagnostic_from_message(&message);
    let causes = chain.map(ToString::to_string).collect::<Vec<_>>();
    if diagnostic.reason.is_none() && !causes.is_empty() {
        diagnostic.reason = Some(causes.join(": "));
    }
    diagnostic
}

fn diagnostic_from_message(message: &str) -> CliError {
    let mut diagnostic = if let Some(phase) = structured_value(message, "phase=") {
        let object = structured_value(message, "object=").unwrap_or_default();
        CliError::new(phase_summary(&phase, &object)).detail("phase", phase)
    } else if message.contains("failed-command=`") {
        CliError::new("command failed")
    } else if message.contains("failed-api=`") {
        CliError::new("GitHub API request failed")
    } else {
        CliError::new(message.trim())
    };

    if let Some(object) = structured_value(message, "object=") {
        diagnostic = diagnostic.detail("object", object);
    }
    if let Some(command) = backtick_value(message, "failed-command=`") {
        diagnostic = diagnostic.detail("command", command);
    }
    if let Some(api) = backtick_value(message, "failed-api=`") {
        diagnostic = diagnostic.detail("api", api);
    }
    if let Some(reason) = structured_error_reason(message) {
        diagnostic = diagnostic.reason(reason);
    }
    if let Some(command) = backtick_value(message, "safe-next-command=`") {
        diagnostic = diagnostic.resolution(format!("run `{command}`"));
    }

    diagnostic
}

fn structured_error_reason(message: &str) -> Option<String> {
    let start = message.find("error=")? + "error=".len();
    let mut end = message.len();
    for marker in [
        " safe-next-command=",
        " failed-command=",
        " failed-api=",
        " phase=",
        " object=",
    ] {
        if let Some(offset) = message[start..].find(marker) {
            end = end.min(start + offset);
        }
    }
    let reason = message[start..end].trim();
    (!reason.is_empty()).then(|| reason.to_owned())
}

fn structured_value(message: &str, key: &str) -> Option<String> {
    let start = message.find(key)? + key.len();
    let value = message[start..]
        .split_once(' ')
        .map(|(value, _)| value)
        .unwrap_or(&message[start..])
        .trim();
    (!value.is_empty()).then(|| value.trim_matches('`').to_owned())
}

fn backtick_value(message: &str, key: &str) -> Option<String> {
    let start = message.find(key)? + key.len();
    let value = message[start..].split_once('`')?.0.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn print_error_line(message: &str) {
    if ui_color_enabled() {
        eprintln!("{} {}", "error:".red().bold(), message);
    } else {
        eprintln!("error: {message}");
    }
}

fn print_section(label: &str, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    eprintln!();
    if ui_color_enabled() {
        eprintln!("{}", format!("{label}:").cyan().bold());
    } else {
        eprintln!("{label}:");
    }
    for line in value.lines() {
        eprintln!("  {line}");
    }
}

fn print_details(details: &[(&'static str, String)]) {
    if details.is_empty() {
        return;
    }
    eprintln!();
    if ui_color_enabled() {
        eprintln!("{}", "details:".cyan().bold());
    } else {
        eprintln!("details:");
    }
    let width = details
        .iter()
        .map(|(key, _)| key.len() + 1)
        .max()
        .unwrap_or(0);
    for (key, value) in details {
        let label = format!("{key}:");
        eprintln!("  {label:width$} {value}");
    }
}

/// Turns an internal structured error string (the `phase=… object=… error=…
/// safe-next-command=…` breadcrumb form used throughout this binary) into a
/// human-readable headline plus an optional "try this next" hint. The full
/// structured string is still written to the debug log for support.
fn humanize_error(raw: &str) -> (String, Option<String>) {
    let mut message = raw.trim().to_owned();

    // Peel off the trailing `safe-next-command=` hint, if any.
    let hint = message.rfind("safe-next-command=").map(|idx| {
        let value = message[idx + "safe-next-command=".len()..]
            .trim()
            .trim_matches('`')
            .trim()
            .to_owned();
        message.truncate(idx);
        value
    });

    // Unwrap the `phase=/object=/failed-command=/failed-api=` breadcrumb prefixes
    // down to the innermost human message.
    let mut headline = message.trim().to_owned();
    let breadcrumb_keys = ["phase=", "failed-command=", "failed-api="];
    while breadcrumb_keys.iter().any(|key| headline.starts_with(key)) {
        match headline.find(" error=") {
            Some(idx) => headline = headline[idx + " error=".len()..].trim().to_owned(),
            None => break,
        }
    }

    (headline, hint.filter(|value| !value.is_empty()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppContext {
    github: GitHubContext,
    stack: Vec<ResolvedChange>,
    frozen_dependencies: Vec<FrozenDependency>,
}

impl AppContext {
    #[tracing::instrument(skip_all)]
    fn new(github: GitHubContext, stack_resolution: StackResolution) -> Self {
        Self {
            github,
            stack: stack_resolution.owned,
            frozen_dependencies: stack_resolution.frozen_dependencies,
        }
    }
}

#[tracing::instrument(skip_all)]
fn resolve_string_config(runner: &impl CommandRunner, name: &str, default: &str) -> String {
    config_value(runner, "jj", name)
        .or_else(|| config_value(runner, "git", name))
        .unwrap_or_else(|| default.to_owned())
}

/// Validate a configured ref component (remote/trunk/branch-prefix) before it is
/// ever passed to `jj`/`git` as a positional argument. These values come from
/// `jj config`/`git config`, which a cloned or shared repo can poison. Without a
/// shell there is no metacharacter injection, but a value beginning with `-`
/// would be parsed as a flag by the downstream tool (e.g. `--insert-after` to
/// `jj rebase`). Reject anything that is not a plain ref name so it can only ever
/// be interpreted as data.
#[tracing::instrument(skip_all)]
fn validate_ref_component(name: &str, value: String) -> Result<String> {
    let valid = !value.is_empty()
        && !value.starts_with('-')
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'));
    if !valid {
        bail!(
            "invalid {CONFIG_PREFIX}.{name} value `{value}`: must be a plain ref name \
             (letters, digits, `/`, `-`, `_`, `.`; no leading `-`, whitespace, `:`, glob, or `..`)"
        );
    }
    Ok(value)
}

#[tracing::instrument(skip_all)]
fn resolve_bool_config(runner: &impl CommandRunner, name: &str, default: bool) -> Result<bool> {
    match config_value(runner, "jj", name).or_else(|| config_value(runner, "git", name)) {
        Some(value) => parse_bool_config(name, &value),
        None => Ok(default),
    }
}

#[tracing::instrument(skip_all)]
fn config_value(runner: &impl CommandRunner, program: &str, name: &str) -> Option<String> {
    let key = format!("{CONFIG_PREFIX}.{name}");
    let args = match program {
        "jj" => vec!["config", "get", key.as_str()],
        "git" => vec!["config", "--get", key.as_str()],
        _ => return None,
    };

    let output = match program {
        "git" => git_run(runner, &args).ok()?,
        _ => runner.run(program, &args).ok()?,
    };
    if !output.success {
        return None;
    }

    let value = output.stdout.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

#[tracing::instrument(skip_all)]
fn parse_bool_config(name: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => bail!("invalid boolean value for {CONFIG_PREFIX}.{name}: {value}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupConfigAction {
    key: &'static str,
    value: String,
}

#[tracing::instrument(skip_all)]
fn ensure_jj_repo(runner: &impl CommandRunner, cwd: &str) -> Result<()> {
    let output = runner.run("jj", &["root"])?;
    if output.success {
        return Ok(());
    }

    bail!(
        "not inside a jj repository (cwd={cwd}); run jj-stack from within your repo, \
         or initialize one with `jj git init --colocate`"
    );
}

#[tracing::instrument(skip_all)]
fn ensure_startup_config(runner: &impl CommandRunner, diagnostics: Diagnostics) -> Result<()> {
    let frozen = jj_config_optional(runner, JJ_CONFIG_FROZEN_ALIAS_KEY)?;
    let immutable = jj_config_required(runner, JJ_CONFIG_IMMUTABLE_ALIAS_KEY)?;
    let base = jj_config_optional(runner, JJ_CONFIG_BASE_IMMUTABLE_ALIAS_KEY)?;
    let actions = plan_startup_config(frozen.as_deref(), &immutable, base.as_deref())?;

    if actions.is_empty() {
        return Ok(());
    }

    if diagnostics.dry_run {
        for action in actions {
            diagnostics.plan_line(&format!(
                "- set repo jj config {} = {}",
                action.key, action.value
            ));
        }
        return Ok(());
    }

    for action in actions {
        set_jj_repo_config(runner, action.key, &action.value, diagnostics)?;
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn plan_startup_config(
    frozen: Option<&str>,
    immutable: &str,
    base: Option<&str>,
) -> Result<Vec<StartupConfigAction>> {
    let mut actions = Vec::new();

    match frozen {
        Some(value) if value == JJ_FROZEN_ALIAS_VALUE => {}
        Some(value) => bail!(
            "repo config `{JJ_CONFIG_FROZEN_ALIAS_KEY}` is `{value}`, expected `{JJ_FROZEN_ALIAS_VALUE}`; refusing to overwrite custom frozen-heads alias"
        ),
        None => actions.push(StartupConfigAction {
            key: JJ_CONFIG_FROZEN_ALIAS_KEY,
            value: JJ_FROZEN_ALIAS_VALUE.to_owned(),
        }),
    }

    match immutable {
        JJ_DEFAULT_IMMUTABLE_ALIAS_VALUE => {
            actions.push(StartupConfigAction {
                key: JJ_CONFIG_IMMUTABLE_ALIAS_KEY,
                value: JJ_REQUIRED_IMMUTABLE_ALIAS_VALUE.to_owned(),
            });
        }
        JJ_REQUIRED_IMMUTABLE_ALIAS_VALUE => {}
        JJ_WRAPPED_IMMUTABLE_ALIAS_VALUE => {
            if base.is_none() {
                bail!(
                    "repo config `{JJ_CONFIG_IMMUTABLE_ALIAS_KEY}` already wraps `{JJ_CONFIG_BASE_IMMUTABLE_ALIAS_KEY}`, but the base alias is missing; refusing to guess the original immutable revset"
                );
            }
        }
        value if value.contains("forklift_frozen_heads()") => {}
        custom => match base {
            None => {
                actions.push(StartupConfigAction {
                    key: JJ_CONFIG_BASE_IMMUTABLE_ALIAS_KEY,
                    value: custom.to_owned(),
                });
                actions.push(StartupConfigAction {
                    key: JJ_CONFIG_IMMUTABLE_ALIAS_KEY,
                    value: JJ_WRAPPED_IMMUTABLE_ALIAS_VALUE.to_owned(),
                });
            }
            Some(existing_base) => bail!(
                "repo config `{JJ_CONFIG_IMMUTABLE_ALIAS_KEY}` is custom (`{custom}`), but `{JJ_CONFIG_BASE_IMMUTABLE_ALIAS_KEY}` already exists as `{existing_base}`; refusing to wrap ambiguous immutable config"
            ),
        },
    }

    Ok(actions)
}

#[tracing::instrument(skip_all)]
fn jj_config_required(runner: &impl CommandRunner, key: &str) -> Result<String> {
    let args = ["config", "get", key];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    Ok(output.stdout.trim().to_owned())
}

#[tracing::instrument(skip_all)]
fn jj_config_optional(runner: &impl CommandRunner, key: &str) -> Result<Option<String>> {
    let args = ["config", "get", key];
    let output = runner.run("jj", &args)?;
    if !output.success {
        return Ok(None);
    }

    let value = output.stdout.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.to_owned()))
    }
}

#[tracing::instrument(skip_all)]
fn set_jj_repo_config(
    runner: &impl CommandRunner,
    key: &str,
    value: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let toml_value = serde_json::to_string(value).context("quote jj config value")?;
    let args = ["config", "set", "--repo", key, toml_value.as_str()];
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn run_required(runner: &impl CommandRunner, program: &str, args: &[&str]) -> Result<String> {
    let output = runner.run(program, args)?;
    if !output.success {
        bail!(
            "`{}` failed: {}",
            display_command(program, args),
            output.stderr.trim()
        );
    }

    let value = output.stdout.trim();
    if value.is_empty() {
        bail!("`{}` returned empty output", display_command(program, args));
    }

    Ok(value.to_owned())
}

/// Directory of the colocated git repo backing the current jj workspace.
///
/// When invoked from a secondary jj workspace the cwd has no `.git` — only the
/// primary (colocated) workspace does. The resolved `.jj/repo` path is
/// `<primary>/.jj/repo`, so the primary workspace dir is two `parent()` calls
/// up from there.
#[tracing::instrument(level = "trace", skip_all)]
fn git_workspace_root(runner: &impl CommandRunner) -> Result<PathBuf> {
    let repo_dir = resolve_current_jj_repo_dir(runner)?;
    repo_dir
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "derive backing workspace dir from jj repo dir {}",
                repo_dir.display()
            )
        })
}

/// Run `git` against the backing colocated workspace, regardless of which jj
/// workspace the user invoked us from. Secondary jj workspaces are not git
/// worktrees, so there is exactly one `.git` to talk to — the primary's.
fn git_run(runner: &impl CommandRunner, args: &[&str]) -> Result<CommandOutput> {
    let root = git_workspace_root(runner)?;
    runner.run_in_dir("git", args, &root)
}

/// `run_required` for git, targeting the backing colocated workspace.
fn git_run_required(runner: &impl CommandRunner, args: &[&str]) -> Result<String> {
    let output = git_run(runner, args)?;
    if !output.success {
        bail!(
            "`{}` failed: {}",
            display_command("git", args),
            output.stderr.trim()
        );
    }
    let value = output.stdout.trim();
    if value.is_empty() {
        bail!("`{}` returned empty output", display_command("git", args));
    }
    Ok(value.to_owned())
}

/// Run `gh` against the backing colocated workspace. `gh repo view` and other
/// commands without an explicit `--repo` auto-detect the repo from the git
/// remote in the cwd; in a secondary jj workspace there is no `.git`, so we
/// must point gh at the primary.
fn gh_run(runner: &impl CommandRunner, args: &[&str]) -> Result<CommandOutput> {
    let root = git_workspace_root(runner)?;
    runner.run_in_dir("gh", args, &root)
}

/// `run_required` for gh, targeting the backing colocated workspace.
fn gh_run_required(runner: &impl CommandRunner, args: &[&str]) -> Result<String> {
    let output = gh_run(runner, args)?;
    if !output.success {
        bail!(
            "`{}` failed: {}",
            display_command("gh", args),
            output.stderr.trim()
        );
    }
    let value = output.stdout.trim();
    if value.is_empty() {
        bail!("`{}` returned empty output", display_command("gh", args));
    }
    Ok(value.to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedChange {
    change_id: String,
    commit_id: String,
    parent_ids: Vec<String>,
    title: String,
    body: String,
    tree_id: String,
    empty: bool,
    conflict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FrozenDependency {
    bookmark: FrozenBookmark,
    change: ResolvedChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StackResolution {
    owned: Vec<ResolvedChange>,
    frozen_dependencies: Vec<FrozenDependency>,
}

#[tracing::instrument(skip_all)]
fn resolve_stack(runner: &impl CommandRunner, revset: &str) -> Result<Vec<ResolvedChange>> {
    let output = runner.run(
        "jj",
        &[
            "log",
            "--no-graph",
            "--reversed",
            "-r",
            revset,
            "-T",
            STACK_LOG_TEMPLATE,
        ],
    )?;
    if !output.success {
        bail!(
            "`{}` failed: {}",
            display_command(
                "jj",
                &[
                    "log",
                    "--no-graph",
                    "--reversed",
                    "-r",
                    revset,
                    "-T",
                    STACK_LOG_TEMPLATE,
                ],
            ),
            output.stderr.trim()
        );
    }

    parse_stack_log(runner, &output.stdout).context("parse jj stack log")
}

#[tracing::instrument(skip_all)]
fn resolve_stack_context(runner: &impl CommandRunner, revset: &str) -> Result<AppContext> {
    resolve_single_rev(runner, "trunk()")?;
    let frozen_bookmarks = frozen_bookmarks(runner)?;
    let stack = resolve_stack(runner, revset)?;
    validate_stack_shape(&stack, revset)?;
    let stack_resolution = resolve_stack_resolution(runner, stack, frozen_bookmarks)?;
    let github = GitHubContext::resolve(runner)?;

    Ok(AppContext::new(github, stack_resolution))
}

#[tracing::instrument(skip_all)]
fn resolve_single_rev(runner: &impl CommandRunner, rev: &str) -> Result<String> {
    let template = "commit_id ++ \"\\n\"";
    let args = ["log", "--no-graph", "-r", rev, "-T", template];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    let commits = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    match commits.as_slice() {
        [commit] => Ok((*commit).to_owned()),
        [] => bail!("revset `{rev}` resolved to no commits; expected exactly one"),
        _ => bail!(
            "revset `{rev}` resolved to {} commits; expected exactly one",
            commits.len()
        ),
    }
}

#[tracing::instrument(skip_all)]
fn effective_merge_revset(
    runner: &impl CommandRunner,
    target: Option<&str>,
) -> Result<MergeRevset> {
    let Some(target) = target else {
        return Ok(MergeRevset {
            revset: DEFAULT_STACK_REVSET.to_owned(),
            target: None,
        });
    };
    let target = resolve_merge_target(runner, target)?;
    Ok(MergeRevset {
        revset: merge_revset_for_target(&target.commit_id),
        target: Some(target),
    })
}

#[tracing::instrument(skip_all)]
fn merge_revset_for_target(target_commit: &str) -> String {
    format!(
        "trunk()..{} & ~::(immutable_heads() | root()) & ~empty()",
        target_commit
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeRevset {
    revset: String,
    target: Option<MergeTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeTarget {
    input: String,
    commit_id: String,
    pr_number: Option<u64>,
}

impl MergeTarget {
    fn label(&self) -> String {
        self.pr_number
            .map(|number| format!("PR #{number}"))
            .unwrap_or_else(|| format!("merge target `{}`", self.input))
    }
}

#[tracing::instrument(skip_all)]
fn resolve_merge_target(runner: &impl CommandRunner, target: &str) -> Result<MergeTarget> {
    let mut github = GitHubContext::resolve(runner)
        .context("resolve GitHub repository before resolving merge target")?;
    let parsed = parse_get_target(target, &github.repo)?;
    github.repo = parsed.repo().to_owned();

    match &parsed {
        GetTarget::PullRequest { .. } => {
            let pr = resolve_target_pr(runner, &github, parsed, "merge")?;
            Ok(MergeTarget {
                input: target.to_owned(),
                commit_id: pr.head_ref_oid,
                pr_number: Some(pr.number),
            })
        }
        GetTarget::BranchOrChange { .. } => {
            match resolve_target_pr(runner, &github, parsed, "merge") {
                Ok(pr) => Ok(MergeTarget {
                    input: target.to_owned(),
                    commit_id: pr.head_ref_oid,
                    pr_number: Some(pr.number),
                }),
                Err(pr_error) => {
                    let commit_id = resolve_single_rev(runner, target).with_context(|| {
                        format!(
                            "merge target `{target}` did not resolve as an open PR target; PR lookup failed with: {pr_error:#}"
                        )
                    })?;
                    Ok(MergeTarget {
                        input: target.to_owned(),
                        commit_id,
                        pr_number: None,
                    })
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct CacheFile {
    #[serde(default)]
    repos: BTreeMap<String, RepoCache>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct RepoCache {
    #[serde(default)]
    changes: BTreeMap<String, PrCacheEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PrCacheEntry {
    pr_number: u64,
    #[serde(default)]
    pr_node_id: String,
    head_branch: String,
    base_branch: String,
    base_ref: String,
    #[serde(default)]
    head_repo_id: String,
    #[serde(default)]
    head_repo_node_id: String,
    #[serde(default)]
    head_repo_name: String,
    #[serde(default)]
    base_repo_id: String,
    #[serde(default)]
    base_repo_node_id: String,
    #[serde(default)]
    base_repo_name: String,
    head_sha: String,
    base_sha: String,
    #[serde(default)]
    author_login: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: String,
    stack_comment_id: Option<String>,
}

#[derive(Debug, Clone)]
struct CacheStore {
    path: PathBuf,
    cache: CacheFile,
}

impl CacheStore {
    #[tracing::instrument(skip_all, fields(phase = phase))]
    fn load_current_best_effort(
        runner: &impl CommandRunner,
        diagnostics: Diagnostics,
        phase: &str,
    ) -> Result<Self> {
        let repo_dir = resolve_current_jj_repo_dir(runner)?;
        let path = repo_dir.join(CONFIG_PREFIX).join("cache.sqlite");
        match Self::load(path.clone()) {
            Ok(store) => Ok(store),
            Err(error) => {
                diagnostics.warn(format!(
                    "phase={phase} object={} error=failed to read SQLite cache; continuing with live discovery: {error:#}",
                    path.display()
                ));
                Ok(Self::empty(path))
            }
        }
    }

    #[tracing::instrument(skip_all)]
    fn load(path: PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::empty(path));
        }

        let conn = Connection::open(&path)
            .with_context(|| format!("open SQLite cache {}", path.display()))?;
        init_cache_schema(&conn)
            .with_context(|| format!("initialize SQLite cache {}", path.display()))?;
        let mut statement = conn
            .prepare(
                "SELECT repo, change_id, pr_number, pr_node_id, head_branch, base_branch,
                        base_ref, head_repo_id, head_repo_node_id, head_repo_name, base_repo_id,
                        base_repo_node_id, base_repo_name, head_sha, base_sha, author_login,
                        title, body, created_at, stack_comment_id
                   FROM pr_cache",
            )
            .with_context(|| format!("prepare SQLite cache read {}", path.display()))?;
        let rows = statement
            .query_map([], |row| {
                let repo: String = row.get(0)?;
                let change_id: String = row.get(1)?;
                let pr_number: i64 = row.get(2)?;
                let entry = PrCacheEntry {
                    pr_number: pr_number as u64,
                    pr_node_id: row.get(3)?,
                    head_branch: row.get(4)?,
                    base_branch: row.get(5)?,
                    base_ref: row.get(6)?,
                    head_repo_id: row.get(7)?,
                    head_repo_node_id: row.get(8)?,
                    head_repo_name: row.get(9)?,
                    base_repo_id: row.get(10)?,
                    base_repo_node_id: row.get(11)?,
                    base_repo_name: row.get(12)?,
                    head_sha: row.get(13)?,
                    base_sha: row.get(14)?,
                    author_login: row.get(15)?,
                    title: row.get(16)?,
                    body: row.get(17)?,
                    created_at: row.get(18)?,
                    stack_comment_id: row.get(19)?,
                };
                Ok((repo, change_id, entry))
            })
            .with_context(|| format!("query SQLite cache {}", path.display()))?;
        let mut cache = CacheFile::default();
        for row in rows {
            let (repo, change_id, entry) =
                row.with_context(|| format!("read SQLite cache row {}", path.display()))?;
            cache
                .repos
                .entry(repo)
                .or_default()
                .changes
                .insert(change_id, entry);
        }

        Ok(Self { path, cache })
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn empty(path: PathBuf) -> Self {
        Self {
            path,
            cache: CacheFile::default(),
        }
    }

    #[tracing::instrument(skip_all)]
    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create cache directory {}", parent.display()))?;
        }

        let mut conn = Connection::open(&self.path)
            .with_context(|| format!("open SQLite cache {}", self.path.display()))?;
        init_cache_schema(&conn)
            .with_context(|| format!("initialize SQLite cache {}", self.path.display()))?;
        let tx = conn
            .transaction()
            .with_context(|| format!("start SQLite cache transaction {}", self.path.display()))?;
        tx.execute("DELETE FROM pr_cache", [])
            .with_context(|| format!("clear SQLite cache {}", self.path.display()))?;
        for (repo, repo_cache) in &self.cache.repos {
            for (change_id, entry) in &repo_cache.changes {
                tx.execute(
                    "INSERT INTO pr_cache (
                        repo, change_id, pr_number, pr_node_id, head_branch, base_branch,
                        base_ref, head_repo_id, head_repo_node_id, head_repo_name, base_repo_id,
                        base_repo_node_id, base_repo_name, head_sha, base_sha, author_login,
                        title, body, created_at, stack_comment_id
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                              ?15, ?16, ?17, ?18, ?19, ?20)",
                    params![
                        repo,
                        change_id,
                        entry.pr_number as i64,
                        entry.pr_node_id,
                        entry.head_branch,
                        entry.base_branch,
                        entry.base_ref,
                        entry.head_repo_id,
                        entry.head_repo_node_id,
                        entry.head_repo_name,
                        entry.base_repo_id,
                        entry.base_repo_node_id,
                        entry.base_repo_name,
                        entry.head_sha,
                        entry.base_sha,
                        entry.author_login,
                        entry.title,
                        entry.body,
                        entry.created_at,
                        entry.stack_comment_id,
                    ],
                )
                .with_context(|| {
                    format!(
                        "write SQLite cache row {}:{} to {}",
                        repo,
                        change_id,
                        self.path.display()
                    )
                })?;
            }
        }
        tx.commit()
            .with_context(|| format!("commit SQLite cache {}", self.path.display()))
    }

    #[tracing::instrument(skip_all, fields(phase = phase))]
    fn save_best_effort(&self, diagnostics: Diagnostics, phase: &str) -> bool {
        match self.save() {
            Ok(()) => true,
            Err(error) => {
                diagnostics.warn(format!(
                    "phase={phase} object={} error=failed to write SQLite cache; continuing because cache is not authoritative: {error:#}",
                    self.path.display()
                ));
                false
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, fields(repo = repo, change = change_id))]
    fn get_pr(&self, repo: &str, change_id: &str) -> Option<&PrCacheEntry> {
        self.cache
            .repos
            .get(repo)
            .and_then(|repo_state| repo_state.changes.get(change_id))
    }

    #[tracing::instrument(skip_all, fields(repo = repo, change = change_id))]
    fn upsert_pr(&mut self, repo: &str, change_id: &str, entry: PrCacheEntry) {
        self.cache
            .repos
            .entry(repo.to_owned())
            .or_default()
            .changes
            .insert(change_id.to_owned(), entry);
    }
}

#[tracing::instrument(level = "trace", skip_all)]
fn init_cache_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pr_cache (
            repo TEXT NOT NULL,
            change_id TEXT NOT NULL,
            pr_number INTEGER NOT NULL,
            pr_node_id TEXT NOT NULL DEFAULT '',
            head_branch TEXT NOT NULL,
            base_branch TEXT NOT NULL,
            base_ref TEXT NOT NULL,
            head_repo_id TEXT NOT NULL DEFAULT '',
            head_repo_node_id TEXT NOT NULL DEFAULT '',
            head_repo_name TEXT NOT NULL DEFAULT '',
            base_repo_id TEXT NOT NULL DEFAULT '',
            base_repo_node_id TEXT NOT NULL DEFAULT '',
            base_repo_name TEXT NOT NULL DEFAULT '',
            head_sha TEXT NOT NULL,
            base_sha TEXT NOT NULL,
            author_login TEXT NOT NULL DEFAULT '',
            title TEXT NOT NULL DEFAULT '',
            body TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT '',
            stack_comment_id TEXT,
            PRIMARY KEY (repo, change_id)
        );",
    )
    .context("create SQLite cache schema")
}

fn resolve_current_jj_repo_dir(runner: &impl CommandRunner) -> Result<PathBuf> {
    let workspace_root =
        run_required(runner, "jj", &["root"]).context("resolve jj workspace root")?;
    resolve_jj_repo_dir(Path::new(&workspace_root))
}

#[tracing::instrument(skip_all)]
fn resolve_jj_repo_dir(workspace_root: &Path) -> Result<PathBuf> {
    let jj_dir = workspace_root.join(".jj");
    let repo_entry = jj_dir.join("repo");
    let metadata = fs::metadata(&repo_entry)
        .with_context(|| format!("read jj repo entry {}", repo_entry.display()))?;

    let repo_dir = if metadata.is_dir() {
        repo_entry
    } else {
        let pointer = fs::read_to_string(&repo_entry)
            .with_context(|| format!("read jj repo pointer {}", repo_entry.display()))?;
        let pointer = pointer.trim();
        if pointer.is_empty() {
            bail!("jj repo pointer {} is empty", repo_entry.display());
        }

        let target = PathBuf::from(pointer);
        if target.is_absolute() {
            target
        } else {
            jj_dir.join(target)
        }
    };

    fs::canonicalize(&repo_dir)
        .with_context(|| format!("resolve jj repo directory {}", repo_dir.display()))
}

#[tracing::instrument(level = "trace", skip_all)]
fn slugify_title(title: &str) -> String {
    title
        .trim()
        .nfd()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .scan(None, |last_char, ch| {
            if ch == '-' && last_char == &Some('-') {
                Some(None)
            } else {
                *last_char = Some(ch);
                Some(Some(ch))
            }
        })
        .flatten()
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
fn deterministic_head_branch(
    config: &AppConfig,
    change: &ResolvedChange,
    used_branches: &HashSet<String>,
) -> String {
    let slug = match slugify_title(&change.title) {
        slug if slug.is_empty() => "change".to_owned(),
        slug => slug,
    };
    let change_id_prefix = change_id_branch_prefix(&change.change_id);
    let base = format!("{}/{}-{}", config.branch_prefix, slug, change_id_prefix);
    find_unused_head_branch(&base, used_branches)
}

#[tracing::instrument(level = "trace", skip_all)]
fn change_id_branch_prefix(change_id: &str) -> &str {
    change_id
        .char_indices()
        .nth(8)
        .map_or(change_id, |(index, _)| &change_id[..index])
}

#[tracing::instrument(level = "trace", skip_all, fields(base = base))]
fn find_unused_head_branch(base: &str, used_branches: &HashSet<String>) -> String {
    if !used_branches.contains(base) {
        return base.to_owned();
    }

    for index in 1.. {
        let candidate = format!("{base}-{index}");
        if !used_branches.contains(&candidate) {
            return candidate;
        }
    }

    unreachable!("unbounded branch suffix search should find a candidate")
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SubmitSummary {
    pushed_refs: usize,
    created_prs: usize,
    updated_prs: usize,
    unchanged_prs: usize,
    created_comments: usize,
    updated_comments: usize,
    unchanged_comments: usize,
    duplicate_comment_warnings: usize,
}

#[derive(Debug, Clone)]
struct SubmitPlan {
    change: ResolvedChange,
    head_branch: String,
    base_branch: String,
    existing_pr: Option<PrCacheEntry>,
    expected_remote_head: Option<String>,
    push_needed: bool,
    pr_update_needed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitPrAction {
    Submit,
    Update,
    Nothing,
}

impl SubmitPrAction {
    fn progress_verb(self) -> &'static str {
        match self {
            Self::Submit => "Submitted",
            Self::Update => "Updated",
            Self::Nothing => "Nothing",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SyncSummary {
    rebased_roots: usize,
    submit_ran: bool,
    cleaned_branches: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MergeSummary {
    checked_prs: usize,
    merged_prs: usize,
    local_updates: usize,
    submit_runs: usize,
    cleaned_branches: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GetSummary {
    prs: usize,
    fetched_branches: usize,
    cache_entries: usize,
    edited: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RepairSummary {
    open_prs: usize,
    pruned_merged_prs: usize,
    comments_changed: usize,
}

#[derive(Debug, Clone)]
struct RepairPlan {
    open_prs: Vec<GhPr>,
    pruned_merged_prs: Vec<u64>,
}

#[derive(Debug, Clone)]
enum RepairAction {
    UpsertStackComment {
        pr_number: u64,
        removed_prs: Vec<u64>,
        body: String,
    },
}

impl RepairAction {
    fn describe(&self) -> String {
        match self {
            Self::UpsertStackComment {
                pr_number,
                removed_prs,
                ..
            } => {
                let removed = repair_pr_list(removed_prs);
                format!("update stack comment on PR #{pr_number} to remove {removed}")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct StatusReport {
    repo: String,
    username: String,
    remote: String,
    trunk: String,
    branch_prefix: String,
    require_approval: bool,
    startup_aliases: StatusAliasState,
    owned_prs: Vec<StatusOwnedPr>,
    frozen_dependencies: Vec<StatusFrozenDependency>,
    first_owned_base_branch: Option<String>,
    merge_blockers: Vec<String>,
    bookmark_problems: Vec<String>,
    problems: Vec<String>,
    suggested_next_command: String,
}

#[derive(Debug, Clone, Serialize)]
struct StatusAliasState {
    frozen_heads: Option<String>,
    immutable_heads: Option<String>,
    base_immutable_heads: Option<String>,
    actions_needed: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct StatusOwnedPr {
    change_id: String,
    commit_id: String,
    title: String,
    head_branch: String,
    base_branch: String,
    pr_number: Option<u64>,
    action: String,
    bookmark_problem: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct StatusFrozenDependency {
    bookmark: String,
    pr_number: u64,
    change_id: String,
    commit_id: String,
    title: String,
    head_branch: Option<String>,
    state: String,
    problem: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Diagnostics {
    verbose: bool,
    dry_run: bool,
}

impl Diagnostics {
    #[tracing::instrument(level = "trace", skip_all, fields(phase = phase))]
    fn phase(self, phase: &str) {
        tracing::debug!(phase, "recovery phase");
        // Dry runs narrate via the `- would ...` plan lines instead, so we
        // don't double-report with progress output there.
        if !self.dry_run && !matches!(phase, "save-cache" | "write-cache") {
            let (verb, message) = phase_label(phase);
            ui_progress(verb, message);
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn repo_details(self, store: &CacheStore) {
        tracing::debug!(cache = %store.path.display(), "resolved repo details");
    }

    #[tracing::instrument(level = "trace", skip_all, fields(program = program))]
    fn command(self, program: &str, args: &[&str]) {
        tracing::debug!(command = %display_command(program, args), "command");
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn progress_bar(self, verb: &str, message: &str, total: usize) -> Option<ProgressBar> {
        if self.dry_run || total == 0 {
            return None;
        }
        let progress = ui_progress_bar(verb, message, total);
        if progress.is_none() {
            ui_progress(verb, message);
        }
        progress
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn submit_pr_action(
        self,
        repo: &str,
        change: &ResolvedChange,
        action: SubmitPrAction,
        entry: &PrCacheEntry,
    ) {
        if self.dry_run {
            return;
        }
        ui_progress(
            action.progress_verb(),
            &format!(
                "PR #{} {} - {}",
                entry.pr_number,
                github_pr_url(repo, entry.pr_number),
                change.title
            ),
        );
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn warn(self, message: impl Display) {
        tracing::warn!(message = %message, "warning");
    }

    #[tracing::instrument(skip_all)]
    fn print_submit_plan(self, config: &AppConfig, context: &AppContext, plans: &[SubmitPlan]) {
        if !self.verbose && !self.dry_run {
            return;
        }

        self.plan_line("planned mutations:");
        for plan in plans {
            if plan.push_needed {
                self.plan_line(&format!(
                    "- set bookmark {} to {} and push to {}",
                    plan.head_branch, plan.change.commit_id, config.remote
                ));
            } else {
                self.plan_line(&format!(
                    "- leave bookmark {} at {}",
                    plan.head_branch, plan.change.commit_id
                ));
            }

            if plan.push_needed {
                self.plan_line(&format!(
                    "- push bookmark {} to {}/{}",
                    plan.head_branch, config.remote, plan.head_branch
                ));
            } else {
                self.plan_line(&format!(
                    "- leave remote branch {}/{} unchanged at {}",
                    config.remote, plan.head_branch, plan.change.commit_id
                ));
            }

            if plan.push_needed {
                self.plan_line(&format!(
                    "- verify remote lease for {}: expected {}",
                    plan.head_branch,
                    plan.expected_remote_head.as_deref().unwrap_or("<absent>")
                ));
            }

            match &plan.existing_pr {
                None => self.plan_line(&format!(
                    "- create PR for {}: head={} base={}",
                    plan.change.change_id, plan.head_branch, plan.base_branch
                )),
                Some(existing) if plan.pr_update_needed => self.plan_line(&format!(
                    "- update PR #{} for {}: head={} base={}",
                    existing.pr_number, plan.change.change_id, plan.head_branch, plan.base_branch
                )),
                Some(existing) => self.plan_line(&format!(
                    "- leave PR #{} unchanged for {}",
                    existing.pr_number, plan.change.change_id
                )),
            }

            match &plan.existing_pr {
                Some(existing) => self.plan_line(&format!(
                    "- upsert stack comment on PR #{} for {}",
                    existing.pr_number, plan.change.change_id
                )),
                None => self.plan_line(&format!(
                    "- upsert stack comment after creating PR for {}",
                    plan.change.change_id
                )),
            }
        }

        if plans.iter().all(|plan| !plan.push_needed)
            && plans.iter().all(|plan| !plan.pr_update_needed)
            && plans.iter().all(|plan| plan.existing_pr.is_some())
        {
            self.plan_line("- no branch or PR metadata changes");
        }

        if self.verbose {
            tracing::debug!(
                repo = %context.github.repo,
                remote = %config.remote,
                trunk = %config.trunk,
                stack_size = context.stack.len(),
                "resolved repo details"
            );
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn plan_line(self, line: &str) {
        if self.dry_run {
            ui_info!("{line}");
        } else if self.verbose {
            tracing::debug!("{line}");
        }
    }
}

#[tracing::instrument(skip_all, fields(target = target))]
fn get_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: &str,
    auto_edit: bool,
    diagnostics: Diagnostics,
) -> Result<GetSummary> {
    diagnostics.phase("resolve-github");
    let mut github = GitHubContext::resolve(runner)
        .map_err(|error| phase_error("resolve-github", "github", error))?;
    let target = parse_get_target(target, &github.repo)?;
    github.repo = target.repo().to_owned();

    diagnostics.phase("resolve-stack-comment");
    let target_pr = resolve_get_target_pr(runner, &github, target)?;
    let target_pr_number = target_pr.number;
    let comment = latest_stack_comment(runner, &github, target_pr_number, "get")?;
    let mut pr_numbers = comment
        .as_ref()
        .map(|comment| parse_stack_pr_numbers(&comment.body))
        .unwrap_or_default();
    // The stack comment lists PRs top-to-bottom; downstream resolution expects
    // bottom-to-top topology order (trunk-adjacent first).
    pr_numbers.reverse();
    if pr_numbers.is_empty() {
        pr_numbers.push(target_pr_number);
    }

    diagnostics.phase("resolve-prs");
    let mut prs = Vec::new();
    let progress = diagnostics.progress_bar("Fetching", "pull requests", pr_numbers.len());
    for (index, pr_number) in pr_numbers.into_iter().enumerate() {
        prs.push(fetch_pr_by_number(runner, &github, "get", pr_number)?);
        if let Some(progress) = &progress {
            progress.set_position((index + 1) as u64);
        }
    }
    if let Some(progress) = progress {
        ui_finish_progress_bar(progress);
    }
    validate_get_pr_stack(config, &github, target_pr_number, &prs)?;

    diagnostics.phase("fetch-stack");
    fetch_get_branches(runner, config, &prs, diagnostics)?;
    let pr_count = prs.len();

    let Some(top_pr) = prs.last() else {
        bail!("stack comment did not resolve any PRs");
    };
    let top_frozen = frozen_bookmark_name(top_pr.number);
    let next_command = format!("jj new {top_frozen}");
    if diagnostics.dry_run {
        update_get_frozen_bookmarks(runner, &prs, diagnostics)?;
        if auto_edit {
            diagnostics.plan_line(&format!("- move working copy: {next_command}"));
        } else {
            diagnostics.plan_line(&format!(
                "- skip editing: run `{next_command}` to start editing above the fetched stack"
            ));
        }
        diagnostics.plan_line(
            "- live GitHub discovery ran during planning; SQLite cache writes are skipped",
        );
        return Ok(GetSummary {
            prs: pr_count,
            fetched_branches: pr_count,
            cache_entries: 0,
            edited: false,
        });
    }

    diagnostics.phase("resolve-fetched-heads");
    let changes_by_pr = resolve_get_pr_changes(runner, &prs)
        .map_err(|error| phase_error("resolve-fetched-heads", "fetched PR heads", error))?;

    diagnostics.phase("freeze-stack");
    update_get_frozen_bookmarks(runner, &prs, diagnostics)
        .map_err(|error| phase_error("freeze-stack", "frozen bookmarks", error))?;

    diagnostics.phase("write-cache");
    let mut store = CacheStore::load_current_best_effort(runner, diagnostics, "write-cache")
        .map_err(|error| phase_error("write-cache", "cache", error))?;
    let mut cache_entries = 0;
    for pr in prs {
        let change = changes_by_pr
            .get(&pr.number)
            .with_context(|| format!("missing resolved jj change for PR #{}", pr.number))?;
        store.upsert_pr(&github.repo, &change.change_id, pr.into_cache_entry(None));
        cache_entries += 1;
    }
    if !store.save_best_effort(diagnostics, "write-cache") {
        cache_entries = 0;
    }

    let edited = if auto_edit {
        diagnostics.phase("edit-stack");
        edit_get_stack(runner, &top_frozen, diagnostics)
            .map_err(|error| phase_error("edit-stack", &top_frozen, error))?;
        true
    } else {
        ui_info!("skip editing: run `{next_command}` to start editing above the fetched stack");
        false
    };

    Ok(GetSummary {
        prs: pr_count,
        fetched_branches: pr_count,
        cache_entries: cache_entries,
        edited,
    })
}

#[tracing::instrument(skip_all, fields(top = top_frozen))]
fn edit_get_stack(
    runner: &impl CommandRunner,
    top_frozen: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = ["new", top_frozen];
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(target = target))]
fn repair_stack_comments(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: &str,
    yes: bool,
    diagnostics: Diagnostics,
) -> Result<RepairSummary> {
    diagnostics.phase("resolve-github");
    let mut github = GitHubContext::resolve(runner)
        .map_err(|error| phase_error("resolve-github", "github", error))?;
    let target = parse_get_target(target, &github.repo)?;
    github.repo = target.repo().to_owned();

    diagnostics.phase("resolve-stack-comment");
    let target_pr = resolve_get_target_pr(runner, &github, target)?;
    if !target_pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            "repair target PR #{} is {}; choose an open PR whose stack comment should be repaired",
            target_pr.number,
            target_pr.state
        );
    }
    let comment = latest_stack_comment(runner, &github, target_pr.number, "repair")?;
    let mut pr_numbers = comment
        .as_ref()
        .map(|comment| parse_stack_pr_numbers(&comment.body))
        .unwrap_or_default();
    pr_numbers.reverse();
    if pr_numbers.is_empty() {
        pr_numbers.push(target_pr.number);
    }
    if !pr_numbers.contains(&target_pr.number) {
        bail!(
            "stack comment for PR #{} did not include the target PR",
            target_pr.number
        );
    }

    diagnostics.phase("resolve-prs");
    let plan = plan_stack_comment_repair(runner, config, &github, target_pr.number, pr_numbers)?;
    let actions = repair_actions(&github, config, &plan);
    print_repair_action_plan(&plan, &actions, diagnostics);

    let mut summary = RepairSummary {
        open_prs: plan.open_prs.len(),
        pruned_merged_prs: plan.pruned_merged_prs.len(),
        comments_changed: 0,
    };
    if diagnostics.dry_run {
        summary.comments_changed = actions.len();
        return Ok(summary);
    }
    if actions.is_empty() {
        return Ok(summary);
    }
    confirm_repair_stack_comments(&plan, &actions, target_pr.number, yes)?;

    diagnostics.phase("stack-comments");
    for action in &actions {
        if execute_repair_action(runner, &github, action, diagnostics)? {
            summary.comments_changed += 1;
        }
    }

    diagnostics.phase("repair-validate");
    validate_repair_result(runner, config, &github, target_pr.number, &plan)?;

    Ok(summary)
}

#[tracing::instrument(skip_all)]
fn repair_actions(
    github: &GitHubContext,
    config: &AppConfig,
    plan: &RepairPlan,
) -> Vec<RepairAction> {
    if plan.pruned_merged_prs.is_empty() {
        return Vec::new();
    }

    plan.open_prs
        .iter()
        .map(|pr| RepairAction::UpsertStackComment {
            pr_number: pr.number,
            removed_prs: plan.pruned_merged_prs.clone(),
            body: repaired_stack_comment_body(github, &plan.open_prs, pr.number, &config.trunk),
        })
        .collect()
}

#[tracing::instrument(skip_all)]
fn print_repair_action_plan(plan: &RepairPlan, actions: &[RepairAction], diagnostics: Diagnostics) {
    render_repair_action_plan(plan, actions, |line| diagnostics.plan_line(line));
}

fn render_repair_action_plan(
    plan: &RepairPlan,
    actions: &[RepairAction],
    mut emit: impl FnMut(&str),
) {
    let pruned = repair_pr_list(&plan.pruned_merged_prs);

    emit("");
    emit("problems:");
    emit(&format!(
        "  merged PRs still listed in stack comment: {pruned}"
    ));
    emit("");
    emit("actions:");
    if actions.is_empty() {
        emit("  <none>");
    } else {
        for (index, action) in actions.iter().enumerate() {
            emit(&format!("  {}. {}", index + 1, action.describe()));
        }
        emit(&format!(
            "  {}. revalidate repaired stack comment topology",
            actions.len() + 1
        ));
    }
}

fn repair_pr_list(numbers: &[u64]) -> String {
    if numbers.is_empty() {
        "<none>".to_owned()
    } else {
        numbers
            .iter()
            .map(|number| format!("#{number}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn print_submit_action_plan(config: &AppConfig, plans: &[SubmitPlan]) {
    render_submit_action_plan(config, plans, print_submit_plan_line);
}

fn render_submit_action_plan(config: &AppConfig, plans: &[SubmitPlan], mut emit: impl FnMut(&str)) {
    emit("");
    emit("actions:");
    for (index, plan) in plans.iter().enumerate() {
        emit(&format!(
            "  {}. {}",
            index + 1,
            submit_action_description(config, plan)
        ));
    }
    if !plans.is_empty() {
        emit(&format!(
            "  {}. sync stack comments for submitted stack",
            plans.len() + 1
        ));
    }
    emit("");
    emit("------------------------------------------------------------");
}

fn submit_action_description(config: &AppConfig, plan: &SubmitPlan) -> String {
    let remote_branch = format!("{}/{}", config.remote, plan.head_branch);
    let commit = short_commit_id(&plan.change.commit_id);
    let branch_detail = if plan.push_needed {
        format!("push {remote_branch} @ {commit}")
    } else {
        format!("{remote_branch} @ {commit}")
    };

    match &plan.existing_pr {
        None => format!(
            "create new PR `{}`: {}, base {}",
            plan.change.title, branch_detail, plan.base_branch
        ),
        Some(existing) if plan.pr_update_needed => format!(
            "update PR #{} `{}`: {}, base {}",
            existing.pr_number, plan.change.title, branch_detail, plan.base_branch
        ),
        Some(existing) => format!(
            "unchanged PR #{} `{}`: {}",
            existing.pr_number, plan.change.title, branch_detail
        ),
    }
}

fn short_commit_id(commit_id: &str) -> &str {
    commit_id.get(..8).unwrap_or(commit_id)
}

fn confirm_submit_stack(yes: bool, yes_command: &str) -> Result<()> {
    if yes {
        return Ok(());
    }

    if !io::stdin().is_terminal() {
        return Err(CliError::new("submit requires confirmation")
            .reason("submit would update GitHub branches, PRs, or stack comments, but stdin is not a terminal")
            .resolution(format!("rerun with `{yes_command}`"))
            .into());
    }

    eprint!("Apply submit? [y/N] ");
    io::stderr()
        .flush()
        .context("flush submit confirmation prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read submit confirmation")?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        return Ok(());
    }

    Err(CliError::new("submit cancelled")
        .reason("user declined to apply the submit plan")
        .resolution(format!("rerun with `{yes_command}`"))
        .into())
}

fn print_submit_plan_line(line: &str) {
    if ui_color_enabled() {
        if let Some((label, rest)) = line.split_once(':') {
            if label.trim() == "actions" {
                eprintln!("{}:{rest}", label.cyan().bold());
                return;
            }
        }
        if let Some(line) = color_submit_action_line(line) {
            eprintln!("{line}");
            return;
        }
    }
    eprintln!("{line}");
}

fn color_submit_action_line(line: &str) -> Option<String> {
    let after_number = line.trim_start().split_once(". ")?.1;
    let action_end = submit_action_label_end(after_number)?;
    let prefix_len = line.len() - after_number.len();
    let prefix = &line[..prefix_len];
    let action = &after_number[..action_end];
    let rest = &after_number[action_end..];
    let colored = match action {
        action if action.starts_with("unchanged") => action.dimmed().to_string(),
        action if action.starts_with("create") => action.green().to_string(),
        action if action.starts_with("update") => action.yellow().to_string(),
        action if action.starts_with("sync") => action.cyan().to_string(),
        action if action.starts_with("close") || action.starts_with("delete") => {
            action.red().to_string()
        }
        _ => return None,
    };
    Some(format!("{prefix}{colored}{rest}"))
}

fn submit_action_label_end(action_line: &str) -> Option<usize> {
    if action_line.starts_with("create new PR") {
        return Some("create new PR".len());
    }
    for marker in [" PR #", " stack comments"] {
        if let Some(index) = action_line.find(marker) {
            return Some(index);
        }
    }
    action_line.find(':')
}

fn print_repair_plan_line(line: &str) {
    if ui_color_enabled() {
        if let Some((label, rest)) = line.split_once(':') {
            match label.trim() {
                "problems" => {
                    eprintln!("{}:{rest}", label.red().bold());
                    return;
                }
                "actions" => {
                    eprintln!("{}:{rest}", label.cyan().bold());
                    return;
                }
                _ => {}
            }
        }
    }
    eprintln!("{line}");
}

#[tracing::instrument(skip_all)]
fn execute_repair_action(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    action: &RepairAction,
    diagnostics: Diagnostics,
) -> Result<bool> {
    match action {
        RepairAction::UpsertStackComment {
            pr_number, body, ..
        } => match upsert_stack_comment(runner, github, *pr_number, "repair", body, diagnostics)? {
            StackCommentAction::Created(_) | StackCommentAction::Updated(_, _) => Ok(true),
            StackCommentAction::Unchanged(_) => Ok(false),
        },
    }
}

#[tracing::instrument(skip_all)]
fn confirm_repair_stack_comments(
    plan: &RepairPlan,
    actions: &[RepairAction],
    target_pr_number: u64,
    yes: bool,
) -> Result<()> {
    if yes {
        return Ok(());
    }
    render_repair_action_plan(plan, actions, |line| print_repair_plan_line(line));
    eprintln!();
    if !io::stdin().is_terminal() {
        return Err(anyhow::Error::new(
            CliError::new("repair requires confirmation")
                .reason("repair would update GitHub stack comments, but stdin is not a terminal")
                .resolution(format!(
                    "rerun with `forklift repair {target_pr_number} --yes`"
                ))
                .detail("target", format!("#{target_pr_number}"))
                .detail("comments", plan.open_prs.len()),
        ));
    }
    eprint!("Apply repair? [y/N] ");
    io::stderr()
        .flush()
        .context("flush repair confirmation prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read repair confirmation")?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        Ok(())
    } else {
        Err(anyhow::Error::new(
            CliError::new("repair cancelled")
                .reason("confirmation was not accepted")
                .resolution(format!(
                    "rerun with `forklift repair {target_pr_number} --yes`"
                )),
        ))
    }
}

#[tracing::instrument(skip_all)]
fn validate_repair_result(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    target_pr_number: u64,
    plan: &RepairPlan,
) -> Result<()> {
    let comment = latest_stack_comment(runner, github, target_pr_number, "repair-validate")?
        .with_context(|| format!("repaired PR #{target_pr_number} has no stack comment"))?;
    let mut pr_numbers = parse_stack_pr_numbers(&comment.body);
    pr_numbers.reverse();
    let validation_plan =
        plan_stack_comment_repair(runner, config, github, target_pr_number, pr_numbers)
            .context("re-run repair detection on repaired stack comment")?;
    if !validation_plan.pruned_merged_prs.is_empty() {
        bail!(
            "repair validation failed: repaired stack comment still lists merged PR(s): {}",
            repair_pr_list(&validation_plan.pruned_merged_prs)
        );
    }

    let expected = plan.open_prs.iter().map(|pr| pr.number).collect::<Vec<_>>();
    let actual = validation_plan
        .open_prs
        .iter()
        .map(|pr| pr.number)
        .collect::<Vec<_>>();
    if actual != expected {
        bail!(
            "repair validation failed: stack comment lists {:?}, expected {:?}",
            actual,
            expected,
        );
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn plan_stack_comment_repair(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    target_pr_number: u64,
    pr_numbers: Vec<u64>,
) -> Result<RepairPlan> {
    let mut seen = HashSet::new();
    let mut open_prs = Vec::new();
    let mut pruned_merged_prs = Vec::new();
    for pr_number in pr_numbers {
        if !seen.insert(pr_number) {
            bail!("stack comment listed PR #{} more than once", pr_number);
        }
        let pr = fetch_pr_by_number(runner, github, "repair", pr_number)?;
        validate_get_pr_metadata(github, &pr)?;
        if pr.state.eq_ignore_ascii_case("OPEN") {
            open_prs.push(pr);
        } else if pr_was_merged(&pr) {
            pruned_merged_prs.push(pr.number);
        } else {
            return Err(anyhow::Error::new(
                CliError::new("cannot repair stack comment automatically")
                    .reason(format!(
                        "PR #{} is {} but not merged",
                        pr.number, pr.state
                    ))
                    .resolution(format!(
                        "reopen or merge PR #{}, or remove it from the stack comment manually, then run `forklift repair {target_pr_number}`",
                        pr.number
                    ))
                    .detail("target", format!("#{target_pr_number}"))
                    .detail("pr", format!("#{}", pr.number))
                    .detail("state", &pr.state),
            ));
        }
    }

    if !open_prs.iter().any(|pr| pr.number == target_pr_number) {
        bail!(
            "repair would remove target PR #{}; choose an open PR still in the stack",
            target_pr_number
        );
    }
    validate_get_pr_stack(config, github, target_pr_number, &open_prs)?;

    Ok(RepairPlan {
        open_prs,
        pruned_merged_prs,
    })
}

#[tracing::instrument(level = "trace", skip_all, fields(pr = pr.number))]
fn pr_was_merged(pr: &GhPr) -> bool {
    pr.merged || pr.state.eq_ignore_ascii_case("MERGED")
}

#[tracing::instrument(skip_all, fields(target = target))]
fn unfreeze_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: &str,
    diagnostics: Diagnostics,
) -> Result<u64> {
    diagnostics.phase("resolve-github");
    let mut github = GitHubContext::resolve(runner)
        .map_err(|error| phase_error("resolve-github", "github", error))?;
    let target = parse_get_target(target, &github.repo)?;
    github.repo = target.repo().to_owned();

    diagnostics.phase("resolve-target");
    let pr = resolve_get_target_pr(runner, &github, target)
        .map_err(|error| phase_error("resolve-target", "unfreeze target", error))?;
    validate_unfreeze_pr(config, &github, &pr)
        .map_err(|error| phase_error("resolve-target", format!("PR #{}", pr.number), error))?;
    verify_repo_push_permission(runner, &github)
        .map_err(|error| phase_error("resolve-target", &github.repo, error))?;

    diagnostics.phase("validate-frozen");
    let frozen_name = frozen_bookmark_name(pr.number);
    let frozen_bookmark = frozen_bookmarks(runner)
        .map_err(|error| phase_error("validate-frozen", "frozen bookmarks", error))?
        .into_iter()
        .find(|bookmark| bookmark.name == frozen_name);
    let frozen_present = if let Some(frozen_bookmark) = frozen_bookmark {
        if frozen_bookmark.commit_id != pr.head_ref_oid {
            bail!(
                "phase=validate-frozen object={} error=frozen bookmark points at {}, but GitHub PR #{} head is {}; run `forklift sync` first before unfreezing safe-next-command=`forklift sync`",
                frozen_name,
                frozen_bookmark.commit_id,
                pr.number,
                pr.head_ref_oid
            );
        }
        true
    } else {
        ui_warn!(
            "frozen bookmark `{}` is missing; continuing adoption from PR #{} head",
            frozen_name,
            pr.number
        );
        false
    };

    diagnostics.phase("fetch-branch");
    fetch_get_branches(runner, config, std::slice::from_ref(&pr), diagnostics)
        .map_err(|error| phase_error("fetch-branch", &pr.head_ref_name, error))?;

    diagnostics.phase("track-branch");
    track_remote_bookmark(runner, config, &pr.head_ref_name, diagnostics)
        .map_err(|error| phase_error("track-branch", &pr.head_ref_name, error))?;

    diagnostics.phase("track-blockers");
    track_untracked_remote_bookmark_blockers(
        runner,
        config,
        &pr.head_ref_oid,
        &pr.head_ref_name,
        diagnostics,
    )
    .map_err(|error| phase_error("track-blockers", format!("PR #{}", pr.number), error))?;

    diagnostics.phase("remove-frozen");
    if frozen_present {
        delete_bookmark(runner, &frozen_name, diagnostics)
            .map_err(|error| phase_error("remove-frozen", &frozen_name, error))?;
    }

    diagnostics.phase("verify-mutable");
    if !diagnostics.dry_run {
        verify_unfrozen_revision_mutable(runner, config, &pr.head_ref_oid, pr.number)
            .map_err(|error| phase_error("verify-mutable", &pr.head_ref_oid, error))?;
    }

    diagnostics.phase("write-cache");
    if diagnostics.dry_run {
        diagnostics.plan_line("- SQLite cache writes are skipped");
    } else {
        let mut store = CacheStore::load_current_best_effort(runner, diagnostics, "write-cache")
            .map_err(|error| phase_error("write-cache", "cache", error))?;
        let change = resolve_stack(runner, &pr.head_ref_oid)
            .and_then(|stack| {
                let [change] = stack.as_slice() else {
                    bail!(
                        "adopted PR #{} head {} resolved to {} jj changes; expected exactly one",
                        pr.number,
                        pr.head_ref_oid,
                        stack.len()
                    );
                };
                Ok(change.clone())
            })
            .map_err(|error| phase_error("write-cache", format!("PR #{}", pr.number), error))?;
        store.upsert_pr(
            &github.repo,
            &change.change_id,
            pr.clone().into_cache_entry(None),
        );
        store.save_best_effort(diagnostics, "write-cache");
    }

    ui_info!(
        "future submit will update `{}` through tracked jj bookmark `{}`",
        github_pr_url(&github.repo, pr.number),
        pr.head_ref_name
    );

    Ok(pr.number)
}

#[tracing::instrument(skip_all, fields(pr = pr.number))]
fn validate_unfreeze_pr(config: &AppConfig, github: &GitHubContext, pr: &GhPr) -> Result<()> {
    validate_get_pr_metadata(github, pr)?;
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            "unfreeze only supports open PRs; PR #{} is {}",
            pr.number,
            pr.state
        );
    }
    let head_repo = get_pr_repo(pr, "head")?;
    let base_repo = get_pr_repo(pr, "base")?;
    if head_repo.name_with_owner != github.repo {
        bail!(
            "cannot unfreeze fork-backed PR #{}: head repo is `{}`, expected `{}`",
            pr.number,
            head_repo.name_with_owner,
            github.repo
        );
    }
    if base_repo.name_with_owner != github.repo {
        bail!(
            "cannot unfreeze PR #{}: base repo is `{}`, expected `{}`",
            pr.number,
            base_repo.name_with_owner,
            github.repo
        );
    }
    if pr.base_ref_name.is_empty() || pr.head_ref_name.is_empty() {
        bail!(
            "cannot unfreeze PR #{} with empty head/base branch",
            pr.number
        );
    }
    validate_ref_component("head branch", pr.head_ref_name.clone())?;
    validate_ref_component("base branch", pr.base_ref_name.clone())?;
    validate_ref_component("configured remote", config.remote.clone())?;
    Ok(())
}

#[tracing::instrument(skip_all)]
fn verify_repo_push_permission(runner: &impl CommandRunner, github: &GitHubContext) -> Result<()> {
    let endpoint = format!("repos/{}", github.repo);
    let args = ["api", endpoint.as_str(), "--jq", ".permissions.push"];
    let output = gh_run(runner, &args)?;
    if !output.success {
        bail!(
            "`{}` failed while checking push permission: {}",
            display_command("gh", &args),
            output.stderr.trim()
        );
    }
    if output.stdout.trim() != "true" {
        bail!(
            "GitHub actor `{}` does not have push permission to `{}`; cannot unfreeze PR branch for adoption",
            github.username,
            github.repo
        );
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(branch = %branch))]
fn track_remote_bookmark(
    runner: &impl CommandRunner,
    config: &AppConfig,
    branch: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = [
        "bookmark",
        "track",
        "--remote",
        config.remote.as_str(),
        branch,
    ];
    if diagnostics.dry_run {
        diagnostics.plan_line(&format!("- {}", display_command("jj", &args)));
        return Ok(());
    }
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(bookmark = %bookmark))]
fn delete_bookmark(
    runner: &impl CommandRunner,
    bookmark: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = ["bookmark", "delete", bookmark];
    if diagnostics.dry_run {
        diagnostics.plan_line(&format!("- {}", display_command("jj", &args)));
        return Ok(());
    }
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteBookmark {
    name: String,
    remote: String,
    tracked: bool,
    conflicted: bool,
    commit_id: String,
}

#[tracing::instrument(skip_all)]
fn remote_bookmarks(runner: &impl CommandRunner) -> Result<Vec<RemoteBookmark>> {
    let args = [
        "bookmark",
        "list",
        "--all-remotes",
        "-T",
        REMOTE_BOOKMARK_TEMPLATE,
    ];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let mut bookmarks = Vec::new();
    for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 5 {
            bail!("parse remote bookmark row `{line}`: expected 5 tab-separated fields");
        }
        let remote = fields[1].trim();
        if remote.is_empty() {
            continue;
        }
        bookmarks.push(RemoteBookmark {
            name: fields[0].to_owned(),
            remote: remote.to_owned(),
            tracked: fields[2] == "tracked",
            conflicted: fields[3] == "conflicted",
            commit_id: fields[4].trim().to_owned(),
        });
    }
    Ok(bookmarks)
}

#[tracing::instrument(skip_all, fields(rev = %rev))]
fn untracked_remote_bookmark_blockers(
    runner: &impl CommandRunner,
    config: &AppConfig,
    rev: &str,
    already_tracked_branch: &str,
) -> Result<Vec<RemoteBookmark>> {
    let mut blockers = Vec::new();
    for bookmark in remote_bookmarks(runner)? {
        if bookmark.remote != config.remote
            || bookmark.tracked
            || bookmark.conflicted
            || bookmark.name == already_tracked_branch
            || bookmark.commit_id.is_empty()
        {
            continue;
        }
        if git_commit_is_ancestor(runner, rev, &bookmark.commit_id)? {
            blockers.push(bookmark);
        }
    }
    blockers.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(blockers)
}

#[tracing::instrument(skip_all, fields(rev = %rev))]
fn track_untracked_remote_bookmark_blockers(
    runner: &impl CommandRunner,
    config: &AppConfig,
    rev: &str,
    already_tracked_branch: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let blockers = untracked_remote_bookmark_blockers(runner, config, rev, already_tracked_branch)?;
    if blockers.is_empty() {
        return Ok(());
    }

    for blocker in blockers {
        ui_warn!(
            "remote bookmark `{}@{}` keeps the target immutable; tracking it before adoption",
            blocker.name,
            blocker.remote
        );
        track_remote_bookmark(runner, config, &blocker.name, diagnostics)?;
    }

    Ok(())
}

#[tracing::instrument(skip_all, fields(rev = %rev))]
fn verify_revision_mutable(runner: &impl CommandRunner, rev: &str) -> Result<()> {
    let args = ["log", "--no-graph", "-r", rev, "-T", "immutable"];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    let immutable = output.stdout.trim();
    if immutable == "false" {
        return Ok(());
    }
    bail!(
        "revision {rev} is still immutable after removing the frozen bookmark; remaining immutable blocker may be another frozen bookmark, tag, trunk, or untracked remote bookmark"
    )
}

#[tracing::instrument(skip_all, fields(rev = %rev, pr = pr_number))]
fn verify_unfrozen_revision_mutable(
    runner: &impl CommandRunner,
    config: &AppConfig,
    rev: &str,
    pr_number: u64,
) -> Result<()> {
    match verify_revision_mutable(runner, rev) {
        Ok(()) => Ok(()),
        Err(_) => Err(anyhow::Error::new(diagnose_unfrozen_revision_immutable(
            runner, config, rev, pr_number,
        )?)),
    }
}

#[tracing::instrument(skip_all, fields(rev = %rev, pr = pr_number))]
fn diagnose_unfrozen_revision_immutable(
    runner: &impl CommandRunner,
    config: &AppConfig,
    rev: &str,
    pr_number: u64,
) -> Result<CliError> {
    if git_commit_is_ancestor(runner, rev, &format!("{}@{}", config.trunk, config.remote))? {
        return Ok(CliError::new(format!(
            "cannot unfreeze PR #{pr_number} because it is already reachable from trunk {}",
            config.trunk
        ))
        .reason(format!(
            "PR #{pr_number} resolves to {rev}, which is already contained in `{}`.",
            config.trunk
        ))
        .resolution(format!(
            "run `forklift sync` or stop trying to adopt PR #{pr_number}"
        )));
    }

    let blockers = untracked_remote_bookmark_blockers(runner, config, rev, "")?;
    if !blockers.is_empty() {
        let labels = blockers
            .iter()
            .take(8)
            .map(|bookmark| format!("`{}@{}`", bookmark.name, bookmark.remote))
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if blockers.len() > 8 {
            format!(" and {} more", blockers.len() - 8)
        } else {
            String::new()
        };
        return Ok(CliError::new(format!(
            "cannot unfreeze PR #{pr_number} because untracked remote bookmarks still make it immutable"
        ))
        .reason(format!(
            "PR #{pr_number} resolves to {rev}, but it is still an ancestor of untracked remote bookmark(s): {labels}{suffix}."
        ))
        .resolution("track or delete the listed remote bookmarks, then rerun `forklift unfreeze`"));
    }

    Ok(CliError::new(format!(
        "cannot unfreeze PR #{pr_number} because it is still immutable"
    ))
    .reason(format!(
        "PR #{pr_number} resolves to {rev}, but jj still reports that revision as immutable after removing the frozen bookmark."
    ))
    .resolution(
        "inspect `immutable_heads()` for another blocker such as a tag, custom immutable alias, or another frozen namespace",
    ))
}

#[tracing::instrument(skip_all)]
fn fetch_get_branches(
    runner: &impl CommandRunner,
    config: &AppConfig,
    prs: &[GhPr],
    diagnostics: Diagnostics,
) -> Result<()> {
    let mut args = vec![
        "git".to_owned(),
        "fetch".to_owned(),
        "--remote".to_owned(),
        config.remote.clone(),
    ];
    for pr in prs {
        args.push("--branch".to_owned());
        args.push(pr.head_ref_name.clone());
    }
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    if diagnostics.dry_run {
        diagnostics.plan_line(&format!("- {}", display_command("jj", &arg_refs)));
        return Ok(());
    }

    diagnostics.command("jj", &arg_refs);
    let output = runner.run("jj", &arg_refs)?;
    if !output.success {
        let branches = prs
            .iter()
            .map(|pr| format!("#{} `{}`", pr.number, pr.head_ref_name))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "failed-command=`{}` error=failed to fetch PR head branches {branches}; an open PR branch may have been deleted or renamed: {}",
            display_command("jj", &arg_refs),
            output.stderr.trim()
        );
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(target = target_pr_number))]
fn validate_get_pr_stack(
    config: &AppConfig,
    github: &GitHubContext,
    target_pr_number: u64,
    prs: &[GhPr],
) -> Result<()> {
    if prs.is_empty() {
        bail!("get resolved an empty PR stack");
    }
    if !prs.iter().any(|pr| pr.number == target_pr_number) {
        bail!("stack comment for PR #{target_pr_number} did not include the target PR");
    }

    let mut seen = HashSet::new();
    for (index, pr) in prs.iter().enumerate() {
        if !seen.insert(pr.number) {
            bail!("stack comment listed PR #{} more than once", pr.number);
        }
        validate_get_pr_metadata(github, pr)?;
        if !pr.state.eq_ignore_ascii_case("OPEN") {
            bail!(
                "get only supports open PRs; PR #{} is {}",
                pr.number,
                pr.state
            );
        }

        let head_repo = get_pr_repo(pr, "head")?;
        let base_repo = get_pr_repo(pr, "base")?;
        if head_repo.name_with_owner != github.repo {
            bail!(
                "fork-backed PR #{} is unsupported for get: head repo is `{}`, expected `{}`",
                pr.number,
                head_repo.name_with_owner,
                github.repo
            );
        }
        if base_repo.name_with_owner != github.repo {
            bail!(
                "fork-backed PR #{} is unsupported for get: base repo is `{}`, expected `{}`",
                pr.number,
                base_repo.name_with_owner,
                github.repo
            );
        }

        if index == 0 {
            if pr.base_ref_name != config.trunk {
                bail!(
                    "stack topology mismatch for bottom PR #{}: base branch is `{}`, expected trunk `{}`",
                    pr.number,
                    pr.base_ref_name,
                    config.trunk
                );
            }
        } else {
            let previous = &prs[index - 1];
            let previous_head_repo = get_pr_repo(previous, "head")?;
            if base_repo.name_with_owner != previous_head_repo.name_with_owner
                || pr.base_ref_name != previous.head_ref_name
            {
                bail!(
                    "stack topology mismatch for PR #{}: base is `{}/{}`, expected previous PR #{} head `{}/{}`",
                    pr.number,
                    base_repo.name_with_owner,
                    pr.base_ref_name,
                    previous.number,
                    previous_head_repo.name_with_owner,
                    previous.head_ref_name
                );
            }
            if pr.base_ref_oid != previous.head_ref_oid {
                bail!(
                    "stack topology mismatch for PR #{}: base SHA is {}, expected previous PR #{} head SHA {}",
                    pr.number,
                    pr.base_ref_oid,
                    previous.number,
                    previous.head_ref_oid
                );
            }
        }
    }

    Ok(())
}

#[tracing::instrument(skip_all, fields(pr = pr.number))]
fn validate_get_pr_metadata(github: &GitHubContext, pr: &GhPr) -> Result<()> {
    if pr.number == 0 {
        bail!(
            "get PR metadata from {} has invalid PR number 0",
            github.repo
        );
    }
    for (field, value) in [
        ("id", pr.id.as_str()),
        ("state", pr.state.as_str()),
        ("headRefName", pr.head_ref_name.as_str()),
        ("baseRefName", pr.base_ref_name.as_str()),
        ("headRefOid", pr.head_ref_oid.as_str()),
        ("baseRefOid", pr.base_ref_oid.as_str()),
        ("title", pr.title.as_str()),
        ("createdAt", pr.created_at.as_str()),
    ] {
        if value.trim().is_empty() {
            bail!(
                "get PR metadata for {}/{} is missing required field `{}`",
                github.repo,
                pr.number,
                field
            );
        }
    }
    let author = pr.author.as_ref().with_context(|| {
        format!(
            "get PR metadata for {}/{} is missing required field `author`",
            github.repo, pr.number
        )
    })?;
    if author.login.trim().is_empty() {
        bail!(
            "get PR metadata for {}/{} is missing required field `author.login`",
            github.repo,
            pr.number
        );
    }
    for role in ["head", "base"] {
        let repo = get_pr_repo(pr, role)?;
        for (field, value) in [
            ("id", repo.id.as_str()),
            ("node_id", repo.node_id.as_str()),
            ("nameWithOwner", repo.name_with_owner.as_str()),
        ] {
            if value.trim().is_empty() {
                bail!(
                    "get PR metadata for {}/{} is missing required field `{}Repository.{}`",
                    github.repo,
                    pr.number,
                    role,
                    field
                );
            }
        }
    }
    Ok(())
}

#[tracing::instrument(level = "trace", skip_all, fields(pr = pr.number, role = %role))]
fn get_pr_repo<'a>(pr: &'a GhPr, role: &str) -> Result<&'a GhRepository> {
    match role {
        "head" => pr
            .head_repository
            .as_ref()
            .with_context(|| format!("PR #{} is missing head repository metadata", pr.number)),
        "base" => pr
            .base_repository
            .as_ref()
            .with_context(|| format!("PR #{} is missing base repository metadata", pr.number)),
        _ => unreachable!("invalid PR repository role"),
    }
}

#[tracing::instrument(skip_all)]
fn resolve_get_pr_changes(
    runner: &impl CommandRunner,
    prs: &[GhPr],
) -> Result<BTreeMap<u64, ResolvedChange>> {
    let mut changes = BTreeMap::new();
    for pr in prs {
        let stack = resolve_stack(runner, &pr.head_ref_oid).with_context(|| {
            format!("resolve fetched PR #{} head {}", pr.number, pr.head_ref_oid)
        })?;
        let [change] = stack.as_slice() else {
            bail!(
                "fetched PR #{} head {} resolved to {} jj changes; expected exactly one",
                pr.number,
                pr.head_ref_oid,
                stack.len()
            );
        };
        if change.commit_id != pr.head_ref_oid {
            bail!(
                "fetched PR #{} head resolved to {}, expected {}",
                pr.number,
                change.commit_id,
                pr.head_ref_oid
            );
        }
        if change.conflict {
            bail!(
                "fetched PR #{} head is conflicted at {} ({})",
                pr.number,
                change.change_id,
                change.commit_id
            );
        }
        changes.insert(pr.number, change.clone());
    }
    Ok(changes)
}

#[tracing::instrument(skip_all)]
fn update_get_frozen_bookmarks(
    runner: &impl CommandRunner,
    prs: &[GhPr],
    diagnostics: Diagnostics,
) -> Result<()> {
    if diagnostics.dry_run {
        for pr in prs {
            let bookmark = frozen_bookmark_name(pr.number);
            diagnostics.plan_line(&format!(
                "- set frozen bookmark {bookmark} -> {}",
                pr.head_ref_oid
            ));
        }
        return Ok(());
    }

    let existing = frozen_bookmarks(runner)?
        .into_iter()
        .map(|bookmark| (bookmark.pr_number, bookmark))
        .collect::<BTreeMap<_, _>>();
    for pr in prs {
        if let Some(bookmark) = existing.get(&pr.number)
            && bookmark.commit_id != pr.head_ref_oid
            && !git_commit_is_ancestor(runner, &bookmark.commit_id, &pr.head_ref_oid)?
        {
            bail!(
                "frozen bookmark `{}` points at {}, which is not an ancestor of fetched PR #{} head {}; refusing divergent collaborator rewrite. Delete or move the frozen bookmark manually after inspecting the rewrite.",
                bookmark.name,
                bookmark.commit_id,
                pr.number,
                pr.head_ref_oid
            );
        }
    }

    for pr in prs {
        let bookmark = frozen_bookmark_name(pr.number);
        let args = [
            "bookmark",
            "set",
            bookmark.as_str(),
            "-r",
            pr.head_ref_oid.as_str(),
        ];
        diagnostics.command("jj", &args);
        let output = runner.run("jj", &args)?;
        if !output.success {
            bail!(
                "failed-command=`{}` error={}",
                display_command("jj", &args),
                output.stderr.trim()
            );
        }
    }

    Ok(())
}

#[tracing::instrument(level = "trace", skip_all, fields(pr = pr_number))]
fn frozen_bookmark_name(pr_number: u64) -> String {
    format!("{FROZEN_BOOKMARK_PREFIX}{pr_number}")
}

#[tracing::instrument(level = "trace", skip_all)]
fn git_commit_is_ancestor(
    runner: &impl CommandRunner,
    ancestor: &str,
    descendant: &str,
) -> Result<bool> {
    let args = ["merge-base", "--is-ancestor", ancestor, descendant];
    let output = git_run(runner, &args)?;
    Ok(output.success)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GetTarget {
    PullRequest { repo: String, number: u64 },
    BranchOrChange { repo: String, value: String },
}

impl GetTarget {
    #[tracing::instrument(level = "trace", skip_all)]
    fn repo(&self) -> &str {
        match self {
            Self::PullRequest { repo, .. } | Self::BranchOrChange { repo, .. } => repo,
        }
    }
}

#[tracing::instrument(skip_all, fields(target = %target))]
fn parse_get_target(target: &str, default_repo: &str) -> Result<GetTarget> {
    let target = target.trim();
    if target.is_empty() {
        bail!(
            "get target must be a PR number, GitHub pull request URL, branch name, or change id prefix"
        );
    }

    if let Ok(number) = target.parse::<u64>() {
        return Ok(GetTarget::PullRequest {
            repo: default_repo.to_owned(),
            number,
        });
    }

    let Some(after_host) = target.split("github.com/").nth(1) else {
        return Ok(GetTarget::BranchOrChange {
            repo: default_repo.to_owned(),
            value: normalize_get_branch_target(target),
        });
    };
    let parts = after_host.split('/').collect::<Vec<_>>();
    if parts.len() < 4 || parts[2] != "pull" {
        bail!(
            "get target must be a PR number, GitHub pull request URL, branch name, or change id prefix"
        );
    }
    let number = parts[3]
        .split(|ch: char| !ch.is_ascii_digit())
        .next()
        .unwrap_or_default()
        .parse::<u64>()
        .with_context(|| format!("parse PR number from `{target}`"))?;
    Ok(GetTarget::PullRequest {
        repo: format!("{}/{}", parts[0], parts[1]),
        number,
    })
}

#[tracing::instrument(level = "trace", skip_all, fields(target = %target))]
fn normalize_get_branch_target(target: &str) -> String {
    target
        .strip_prefix("refs/heads/")
        .unwrap_or(target)
        .to_owned()
}

#[tracing::instrument(skip_all)]
fn resolve_get_target_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    target: GetTarget,
) -> Result<GhPr> {
    resolve_target_pr(runner, github, target, "get")
}

/// Resolve a `pr` command target into `(pr_number, browser_url)`.
///
/// A bare PR number or PR URL needs no network round-trip; branch/change-id
/// targets (and the default current-change lookup) are resolved through `gh`.
#[tracing::instrument(skip_all)]
fn resolve_pr_url(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    target: Option<&str>,
) -> Result<(u64, String)> {
    match target {
        Some(target) => match parse_get_target(target, &github.repo)? {
            GetTarget::PullRequest { repo, number } => Ok((number, github_pr_url(&repo, number))),
            branch @ GetTarget::BranchOrChange { .. } => {
                let pr = resolve_target_pr(runner, github, branch, "pr")?;
                Ok((pr.number, github_pr_url(&github.repo, pr.number)))
            }
        },
        None => {
            let change_id = current_change_id(runner)?;
            let pr = lookup_get_target_pr(runner, github, &change_id, "pr")?;
            Ok((pr.number, github_pr_url(&github.repo, pr.number)))
        }
    }
}

/// Change id of the current working-copy commit (`@`).
#[tracing::instrument(skip_all)]
fn current_change_id(runner: &impl CommandRunner) -> Result<String> {
    let template = "change_id ++ \"\\n\"";
    let args = ["log", "--no-graph", "-r", "@", "-T", template];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    output
        .stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .context("resolve current change id from `@`")
}

/// Open `url` in the user's default browser via the platform opener.
#[tracing::instrument(skip_all, fields(url = %url))]
fn open_url(runner: &impl CommandRunner, url: &str) -> Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let output = runner.run(program, &args)?;
    if !output.success {
        bail!(
            "failed to open browser: failed-command=`{}` error={}",
            display_command(program, &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

#[tracing::instrument(skip_all)]
fn resolve_target_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    target: GetTarget,
    purpose: &str,
) -> Result<GhPr> {
    match target {
        GetTarget::PullRequest { number, .. } => {
            fetch_pr_by_number(runner, github, purpose, number)
        }
        GetTarget::BranchOrChange { value, .. } => {
            lookup_get_target_pr(runner, github, &value, purpose)
        }
    }
}

#[tracing::instrument(skip_all, fields(target = %target))]
fn lookup_get_target_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    target: &str,
    purpose: &str,
) -> Result<GhPr> {
    let prs = list_open_prs(runner, github, purpose)?;
    let branch_matches = prs
        .iter()
        .filter(|pr| pr.head_ref_name == target)
        .cloned()
        .collect::<Vec<_>>();
    if !branch_matches.is_empty() {
        return one_get_target_match(target, "branch", branch_matches, purpose);
    }

    let Some(prefix) = change_prefix_get_target(target) else {
        bail!(
            "{purpose} target `{target}` did not match an open PR branch; pass a PR number, PR URL, exact branch name, or at least 8 chars of the jj change id"
        );
    };
    let change_matches = prs
        .into_iter()
        .filter(|pr| head_branch_matches_change_prefix(&pr.head_ref_name, &prefix))
        .collect::<Vec<_>>();
    one_get_target_match(target, "change id prefix", change_matches, purpose)
}

#[tracing::instrument(skip_all, fields(purpose = %purpose))]
fn list_open_prs(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    purpose: &str,
) -> Result<Vec<GhPr>> {
    let args = [
        "pr",
        "list",
        "--repo",
        github.repo.as_str(),
        "--state",
        "open",
        "--json",
        PR_JSON_FIELDS,
        "--limit",
        "200",
    ];
    let output = gh_run(runner, &args)?;
    if !output.success {
        bail!(
            "`{}` failed while listing open PRs for {}: {}",
            display_command("gh", &args),
            purpose,
            output.stderr.trim()
        );
    }

    serde_json::from_str::<Vec<GhPr>>(&output.stdout)
        .with_context(|| format!("parse open PR list while resolving {purpose} target"))
}

#[tracing::instrument(level = "trace", skip_all, fields(target = %target, kind = %kind))]
fn one_get_target_match(
    target: &str,
    kind: &str,
    matches: Vec<GhPr>,
    purpose: &str,
) -> Result<GhPr> {
    match matches.as_slice() {
        [] => bail!("{purpose} target `{target}` did not match an open PR {kind}"),
        [pr] => Ok(pr.clone()),
        _ => {
            let refs = matches
                .iter()
                .map(|pr| format!("#{} `{}`", pr.number, pr.head_ref_name))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("{purpose} target `{target}` matched multiple open PRs by {kind}: {refs}")
        }
    }
}

#[tracing::instrument(level = "trace", skip_all, fields(target = %target))]
fn change_prefix_get_target(target: &str) -> Option<String> {
    if target.chars().count() < 8 || !target.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some(change_id_branch_prefix(target).to_owned())
}

#[tracing::instrument(level = "trace", skip_all, fields(branch = %branch, prefix = %prefix))]
fn head_branch_matches_change_prefix(branch: &str, prefix: &str) -> bool {
    let exact_suffix = format!("-{prefix}");
    if branch.ends_with(&exact_suffix) {
        return true;
    }

    let numbered_suffix = format!("-{prefix}-");
    branch
        .rsplit_once(&numbered_suffix)
        .is_some_and(|(_, suffix)| {
            !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
        })
}

#[tracing::instrument(skip_all, fields(pr = pr_number, change = %change_id))]
fn latest_stack_comment(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    change_id: &str,
) -> Result<Option<GhStackComment>> {
    let mut comments = list_stack_comments(runner, github, pr_number, change_id)?
        .into_iter()
        .filter(|comment| comment.body.contains(STACK_COMMENT_MARKER))
        .collect::<Vec<_>>();
    comments.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(comments.pop())
}

#[tracing::instrument(level = "trace", skip_all)]
fn parse_stack_pr_numbers(body: &str) -> Vec<u64> {
    let mut numbers = Vec::new();
    for line in body.lines() {
        let Some(after_pull) = line.split("/pull/").nth(1) else {
            continue;
        };
        let digits = after_pull
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        let Ok(number) = digits.parse::<u64>() else {
            continue;
        };
        if !numbers.contains(&number) {
            numbers.push(number);
        }
    }
    numbers
}

#[tracing::instrument(skip_all, fields(revset = %revset))]
fn status_report(
    runner: &impl CommandRunner,
    config: &AppConfig,
    revset: &str,
    diagnostics: Diagnostics,
) -> Result<StatusReport> {
    diagnostics.phase("status-aliases");
    let startup_aliases = status_alias_state(runner)?;

    diagnostics.phase("resolve-stack");
    let context = resolve_stack_context(runner, revset)?;
    let store = CacheStore::load_current_best_effort(runner, diagnostics, "status")?;
    let mut used_head_branches = HashSet::new();
    let mut previous_head_branch = None;
    let mut owned_prs = Vec::new();
    let mut bookmark_problems = Vec::new();
    let mut merge_blockers = Vec::new();
    let mut problems = Vec::new();

    let frozen_dependencies =
        status_frozen_dependencies(runner, &context.github, &context.frozen_dependencies);
    for dependency in &frozen_dependencies {
        if let Some(problem) = &dependency.problem {
            problems.push(problem.clone());
        }
    }
    if let Some(last) = frozen_dependencies.last() {
        previous_head_branch = last.head_branch.clone();
    }
    let first_owned_base_branch = previous_head_branch
        .clone()
        .or_else(|| Some(config.trunk.clone()));

    for change in &context.stack {
        let base_branch = previous_head_branch
            .clone()
            .unwrap_or_else(|| config.trunk.clone());
        match resolve_submit_head_branch(
            runner,
            config,
            &mut used_head_branches,
            &store,
            &context,
            change,
            diagnostics,
        ) {
            Ok((head_branch, existing_pr, expected_remote_head)) => {
                let action = match &existing_pr {
                    None => "create".to_owned(),
                    Some(entry) => {
                        let push_needed =
                            expected_remote_head.as_deref() != Some(change.commit_id.as_str());
                        if push_needed || pr_metadata_changed(entry, &base_branch, change) {
                            "update".to_owned()
                        } else {
                            "unchanged".to_owned()
                        }
                    }
                };
                previous_head_branch = Some(head_branch.clone());
                owned_prs.push(StatusOwnedPr {
                    change_id: change.change_id.clone(),
                    commit_id: change.commit_id.clone(),
                    title: change.title.clone(),
                    head_branch,
                    base_branch,
                    pr_number: existing_pr.as_ref().map(|entry| entry.pr_number),
                    action,
                    bookmark_problem: None,
                });
            }
            Err(error) => {
                let message = error.to_string();
                bookmark_problems.push(message.clone());
                problems.push(message.clone());
                let head_branch =
                    deterministic_head_branch(config, change, &mut used_head_branches);
                previous_head_branch = Some(head_branch.clone());
                owned_prs.push(StatusOwnedPr {
                    change_id: change.change_id.clone(),
                    commit_id: change.commit_id.clone(),
                    title: change.title.clone(),
                    head_branch,
                    base_branch,
                    pr_number: None,
                    action: "blocked".to_owned(),
                    bookmark_problem: Some(message),
                });
            }
        }
    }

    if let Some((change, owned)) = context.stack.first().zip(owned_prs.first()) {
        match owned.pr_number {
            Some(pr_number) => {
                match fetch_pr_for_merge(runner, &context.github, &change.change_id, pr_number) {
                    Ok(pr) => {
                        if let Err(error) =
                            validate_merge_frozen_dependencies(runner, config, &context, &pr)
                        {
                            merge_blockers.push(error.to_string());
                        } else {
                            let entry = pr.clone().into_cache_entry(None);
                            if let Err(error) =
                                validate_pr_ready_for_merge(config, change, &entry, &pr, false)
                            {
                                merge_blockers.push(error.to_string());
                            }
                        }
                    }
                    Err(error) => merge_blockers.push(error.to_string()),
                }
            }
            None => merge_blockers.push("run `forklift submit` before merging".to_owned()),
        }
    }
    problems.extend(merge_blockers.iter().cloned());

    let suggested_next_command =
        suggested_status_next_command(&owned_prs, &frozen_dependencies, &merge_blockers, &problems);

    Ok(StatusReport {
        repo: context.github.repo,
        username: context.github.username,
        remote: config.remote.clone(),
        trunk: config.trunk.clone(),
        branch_prefix: config.branch_prefix.clone(),
        require_approval: config.require_approval,
        startup_aliases,
        owned_prs,
        frozen_dependencies,
        first_owned_base_branch,
        merge_blockers,
        bookmark_problems,
        problems,
        suggested_next_command,
    })
}

#[tracing::instrument(skip_all)]
fn status_alias_state(runner: &impl CommandRunner) -> Result<StatusAliasState> {
    let frozen_heads = jj_config_optional(runner, JJ_CONFIG_FROZEN_ALIAS_KEY)?;
    let immutable_heads = Some(jj_config_required(runner, JJ_CONFIG_IMMUTABLE_ALIAS_KEY)?);
    let base_immutable_heads = jj_config_optional(runner, JJ_CONFIG_BASE_IMMUTABLE_ALIAS_KEY)?;
    let actions_needed = match immutable_heads.as_deref() {
        Some(immutable) => plan_startup_config(
            frozen_heads.as_deref(),
            immutable,
            base_immutable_heads.as_deref(),
        )?
        .into_iter()
        .map(|action| format!("set {} = {}", action.key, action.value))
        .collect(),
        None => Vec::new(),
    };
    Ok(StatusAliasState {
        frozen_heads,
        immutable_heads,
        base_immutable_heads,
        actions_needed,
    })
}

#[tracing::instrument(skip_all)]
fn status_frozen_dependencies(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    dependencies: &[FrozenDependency],
) -> Vec<StatusFrozenDependency> {
    dependencies
        .iter()
        .map(|dependency| {
            match fetch_pr_by_number(
                runner,
                github,
                &dependency.change.change_id,
                dependency.bookmark.pr_number,
            ) {
                Ok(pr) => {
                    let problem = if pr.head_ref_oid != dependency.change.commit_id {
                        Some(format!(
                            "frozen dependency `{}` is stale: local {} but GitHub PR #{} head is {}; run `forklift sync`",
                            dependency.bookmark.name,
                            dependency.change.commit_id,
                            pr.number,
                            pr.head_ref_oid
                        ))
                    } else if !pr.state.eq_ignore_ascii_case("OPEN")
                        && !pr.state.eq_ignore_ascii_case("MERGED")
                    {
                        Some(format!(
                            "frozen dependency `{}` PR #{} is `{}`; run `forklift sync`",
                            dependency.bookmark.name, pr.number, pr.state
                        ))
                    } else {
                        None
                    };
                    StatusFrozenDependency {
                        bookmark: dependency.bookmark.name.clone(),
                        pr_number: dependency.bookmark.pr_number,
                        change_id: dependency.change.change_id.clone(),
                        commit_id: dependency.change.commit_id.clone(),
                        title: dependency.change.title.clone(),
                        head_branch: Some(pr.head_ref_name),
                        state: pr.state,
                        problem,
                    }
                }
                Err(error) => StatusFrozenDependency {
                    bookmark: dependency.bookmark.name.clone(),
                    pr_number: dependency.bookmark.pr_number,
                    change_id: dependency.change.change_id.clone(),
                    commit_id: dependency.change.commit_id.clone(),
                    title: dependency.change.title.clone(),
                    head_branch: None,
                    state: "UNKNOWN".to_owned(),
                    problem: Some(error.to_string()),
                },
            }
        })
        .collect()
}

#[tracing::instrument(skip_all)]
fn suggested_status_next_command(
    owned_prs: &[StatusOwnedPr],
    frozen_dependencies: &[StatusFrozenDependency],
    merge_blockers: &[String],
    problems: &[String],
) -> String {
    if problems
        .iter()
        .any(|problem| problem.contains("forklift sync") || problem.contains("frozen dependency"))
    {
        return "forklift sync".to_owned();
    }
    if owned_prs
        .iter()
        .any(|owned| matches!(owned.action.as_str(), "create" | "update" | "blocked"))
    {
        return "forklift submit".to_owned();
    }
    if !merge_blockers.is_empty() {
        return "resolve merge blockers".to_owned();
    }
    if !frozen_dependencies.is_empty() {
        return "forklift merge".to_owned();
    }
    "forklift merge".to_owned()
}

#[tracing::instrument(skip_all)]
fn print_status_report(report: &StatusReport) {
    ui_info!("repo: {}", report.repo);
    ui_info!("user: {}", report.username);
    ui_info!(
        "config: remote={}, trunk={}, branch-prefix={}, require-approval={}",
        report.remote,
        report.trunk,
        report.branch_prefix,
        report.require_approval
    );
    ui_info!(
        "startup aliases: frozen={}, immutable={}",
        report
            .startup_aliases
            .frozen_heads
            .as_deref()
            .unwrap_or("<missing>"),
        report
            .startup_aliases
            .immutable_heads
            .as_deref()
            .unwrap_or("<missing>")
    );
    if !report.frozen_dependencies.is_empty() {
        ui_info!("frozen dependencies:");
        for dependency in &report.frozen_dependencies {
            ui_info!(
                "- PR #{} {} {} ({})",
                dependency.pr_number,
                dependency.bookmark,
                dependency
                    .head_branch
                    .as_deref()
                    .unwrap_or("<unknown-branch>"),
                dependency.state
            );
        }
    }
    ui_info!("owned stack:");
    for owned in &report.owned_prs {
        let pr = owned
            .pr_number
            .map(|number| format!("#{number}"))
            .unwrap_or_else(|| "new".to_owned());
        ui_info!(
            "- {} {} -> {} ({})",
            pr,
            owned.head_branch,
            owned.base_branch,
            owned.action
        );
    }
    if let Some(base) = &report.first_owned_base_branch {
        ui_info!("first owned base: {base}");
    }
    if !report.bookmark_problems.is_empty() {
        ui_warn!("bookmark problems:");
        for problem in &report.bookmark_problems {
            ui_warn!("- {problem}");
        }
    }
    if !report.merge_blockers.is_empty() {
        ui_warn!("merge blockers:");
        for blocker in &report.merge_blockers {
            ui_warn!("- {blocker}");
        }
    }
    ui_info!("next: {}", report.suggested_next_command);
}

/// Merge a stack of stacked GitHub PRs into trunk by fast-forwarding trunk over
/// the whole stack in a single push, preserving every commit.
///
/// # Why this model (the core requirement)
///
/// Stacked PRs exist to keep history granular — one reviewable PR per logical
/// change. The non-negotiable requirement is that merging **preserves those
/// individual commits**: no squash, no rebase, no merge commits. We get that by
/// re-pointing each PR's base branch to `trunk` and then fast-forwarding `trunk`
/// over the top of the stack with one `jj git push`. GitHub marks a PR merged
/// once its head commit is reachable from its *base* branch, so a single FF push
/// of `trunk` (now the base of every PR) auto-merges the entire stack by
/// reachability — atomically, in order, with the commits intact.
///
/// A strict fast-forward is the load-bearing safety property: `trunk` must be an
/// ancestor of the stack top, so the push replays the existing commits with no
/// three-way merge and therefore no possible conflict.
///
/// # Steps, in order, and why each exists
///
/// 1. **resolve-stack** — resolve `trunk()`, the frozen bookmarks, and the linear
///    stack, then `validate_stack_shape` (rejects empties, conflicts, merge
///    commits, multiple roots, and forks). We need a provably linear, ordered
///    chain; anything else can't be fast-forwarded safely.
/// 2. **resolve-github / merge-frozen-check** — resolve the repo+user and verify
///    the bottom PR's frozen dependencies are satisfied (a stale or unmerged
///    frozen dep means the stack isn't actually ready to land).
/// 3. **merge-pr-check** (three passes, so per-PR GitHub waits overlap):
///    - *Pass 1*: resolve each PR; if its base isn't `trunk`, re-point it with a
///      `gh api PATCH base=<trunk>`. Firing **all** the PATCHes up front lets
///      GitHub's (async) mergeability recompute for each PR run concurrently
///      instead of in series.
///    - *Pass 2*: settle mergeability — re-pointing invalidates GitHub's cached
///      `mergeable`, so the first read returns `UNKNOWN`. Poll all still-unknown
///      PRs together with one shared exponential-backoff sleep per round
///      (`settle_candidates_mergeability`), turning sum-of-waits into
///      max-of-waits. Skipped entirely in dry-run.
///    - *Pass 3*: `validate_pr_ready_for_merge` for each PR (open, not draft,
///      head == local commit == cache, base == trunk, approved unless
///      `--admin`/`--no-require-approval`, no auto-merge, mergeable,
///      mergeStateStatus, status checks unless `--admin`).
/// 4. **merge-push** — `fast_forward_trunk_over_stack`: hard-check that remote
///    `trunk` is an ancestor of the stack top (else bail: "run sync first"), set
///    the local `trunk` bookmark to the top commit, and push **once**.
/// 5. **verify-merge** — poll until GitHub has marked every PR `MERGED` (the
///    reachability merge is applied asynchronously after the push). This is the
///    safety net: if a PR doesn't flip, we fail loudly rather than leaving
///    `trunk` advanced with PRs silently still open.
/// 6. **cleanup-branches** — `cleanup_merged_branches`: for each merged head
///    branch, refuse if an open PR still bases on it (the cascade-close guard —
///    see "do not do"), delete the local bookmark, push all deletions in one
///    batched `jj git push`, then `forget --include-remotes` to reconcile the
///    tracking refs GitHub's auto-delete leaves dangling. All best-effort: the
///    merge already succeeded, so cleanup never fails it.
/// 7. **reset-working-copy** — `jj new <trunk>`: leave the working copy on a
///    fresh empty change atop the new trunk.
///
/// # What we deliberately do NOT do, and why
///
/// - **No squash merge** (`gh pr merge --squash`). It collapses the stack into a
///   single commit, destroying the per-PR history that is the entire point of
///   stacking. (This was the original implementation; it was removed.)
/// - **No per-PR GitHub merge / merge button.** That produces squashes or merge
///   commits and forces sequential API merges, each waiting on the previous. The
///   FF-by-reachability model lands the whole stack with one push.
/// - **No `--delete-branch` on merge.** Deleting a branch a stacked PR is based
///   on cascade-*closes* that PR (this actually happened — it closed PR #5164).
///   We re-point every base to `trunk` first, so by deletion time nothing bases
///   on a stack branch, and we delete branches ourselves after verifying.
/// - **No rebase/abandon of merged changes.** The FF model leaves the commits in
///   place; there is nothing to rebase or abandon. (The old squash flow did
///   abandon+rebase; removed.)
/// - **No merge queue.** `mergeStateStatus == QUEUED` bails — this workflow only
///   does direct fast-forward.
///
/// # Alternatives considered (and why not)
///
/// - *Squash each PR bottom-up via the API*: simple, but destroys history and is
///   slow (sequential, each merge waits). Rejected — history preservation is a
///   hard requirement.
/// - *Drop the mergeability gate under `--admin`* (rely solely on the FF ancestor
///   check): assessed safe-with-care — the FF check already guarantees a
///   conflict-free push and `mergeStateStatus == DIRTY` independently catches
///   conflicts — but to remove the pass-2 wait it must *also* tolerate a
///   transient `mergeStateStatus == UNKNOWN`. Not adopted yet; it would let admin
///   merges skip the recompute wait entirely.
/// - *One repo-wide `gh pr list` for the cascade guard*: `gh pr list` defaults to
///   30 results, so a repo-wide list can silently drop an open PR and re-trigger
///   the cascade-close incident — and the fake-gh test harness wouldn't catch it.
///   Rejected unless done with `--paginate` plus a >30-PR regression test.
/// - *GraphQL batch fetch of all PRs*: would cut gh round-trips, but GraphQL's
///   nested shape doesn't match the flat `GhMergePr`, and it would force a
///   parallel deserialization path plus a rewrite of the entire arg-vector-keyed
///   fake-gh fixture set. Deferred — blast radius outweighs the win.
/// - *Threaded/parallel jj pushes*: concurrent `jj` contends on the op-log lock.
///   Rejected in favor of a single batched `jj git push --bookmark …`.
/// - *Skip the deletion push and rely on GitHub's "auto-delete head branch on
///   merge"*: leaks branches on repos without that setting and leaves jj's view
///   inconsistent. Rejected; instead we push the deletion *and* forget the
///   tracking refs.
///
/// # Performance shape
///
/// The dominant cost is GitHub's *server-side async* work — recomputing
/// mergeability after each re-point, and applying the reachability merge after
/// the push. That can only be overlapped, not avoided (short of skipping the
/// gate under `--admin`). We overlap it by batching the re-point PATCHes and
/// polling all PRs together with shared exponential backoff, and we batch every
/// gh/jj call we can (one FF push, one branch-deletion push).
///
/// `admin` bypasses branch-protection (`BLOCKED`), required-status-check, and
/// approval gates for operators force-pushing past protection.
fn diagnose_empty_targeted_merge(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: &MergeTarget,
    narrowed_revset: &str,
    frozen_bookmarks: &[FrozenBookmark],
) -> Result<()> {
    let target_label = target.label();
    let trunk = resolve_single_rev(runner, "trunk()")?;
    if git_commit_is_ancestor(runner, &target.commit_id, &trunk)? {
        return Err(CliError::new(format!(
            "cannot merge {target_label} because it is already reachable from trunk {}",
            config.trunk
        ))
        .reason(format!(
            "{target_label} resolves to {}, which is already in `{}`.",
            target.commit_id, config.trunk
        ))
        .resolution("choose an unmerged owned PR, or run `forklift sync` to clean up local state")
        .into());
    }

    let target_range_revset = format!("trunk()..{} & ~empty()", target.commit_id);
    let target_range = resolve_stack(runner, &target_range_revset)?;
    let immutable_target_revset = format!("{} & ::(immutable_heads() | root())", target.commit_id);
    let immutable_target = resolve_stack(runner, &immutable_target_revset)?;
    if !immutable_target.is_empty() {
        if let Some(bookmark) =
            frozen_bookmark_covering_target(runner, &target.commit_id, frozen_bookmarks)?
        {
            return Err(CliError::new(format!(
                "cannot merge {target_label} because it is frozen in this checkout"
            ))
            .reason(format!(
                "{target_label} resolves to {}, but that commit is covered by frozen bookmark `{}`. Frozen revisions are treated as dependencies, so forklift merge only considers owned mutable changes and the target range is empty.",
                target.commit_id, bookmark.name
            ))
            .resolution(format!(
                "unfreeze or get ownership of {target_label} before merging it"
            ))
            .into());
        }

        return Err(CliError::new(format!(
            "cannot merge {target_label} because it is immutable in this checkout"
        ))
        .reason(format!(
            "{target_label} resolves to {}, but that commit is excluded by `immutable_heads()`.",
            target.commit_id
        ))
        .resolution("choose an owned mutable stack change before merging it")
        .into());
    }

    if !target_range.is_empty() {
        return Err(CliError::new(format!(
            "{target_label} is outside the active stack"
        ))
        .reason(format!(
            "{target_label} resolves to {}, but it is not part of the owned mutable stack selected from `@`.",
            target.commit_id
        ))
        .resolution(format!(
            "move to the stack containing {target_label}, then run `forklift merge {}`",
            target.input
        ))
        .into());
    }

    Err(CliError::new(format!(
        "cannot merge {target_label} because the target range is empty"
    ))
    .reason(format!(
        "{target_label} resolves to {}, but `{narrowed_revset}` produced no owned non-empty changes.",
        target.commit_id
    ))
    .resolution("choose an owned mutable non-empty PR before merging it")
    .into())
}

fn frozen_bookmark_covering_target<'a>(
    runner: &impl CommandRunner,
    target_commit: &str,
    frozen_bookmarks: &'a [FrozenBookmark],
) -> Result<Option<&'a FrozenBookmark>> {
    for bookmark in frozen_bookmarks {
        if git_commit_is_ancestor(runner, target_commit, &bookmark.commit_id)? {
            return Ok(Some(bookmark));
        }
    }
    Ok(None)
}

#[tracing::instrument(skip_all, fields(revset = %revset))]
fn merge_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    revset: &str,
    target: Option<&MergeTarget>,
    admin: bool,
    diagnostics: Diagnostics,
) -> Result<MergeSummary> {
    let mut summary = MergeSummary::default();

    diagnostics.phase("resolve-stack");
    resolve_single_rev(runner, "trunk()")
        .map_err(|error| phase_error("resolve-stack", "trunk()", error))?;
    let frozen_bookmarks = frozen_bookmarks(runner)
        .map_err(|error| phase_error("resolve-stack", "frozen-bookmarks", error))?;
    let stack = resolve_stack(runner, revset)
        .map_err(|error| phase_error("resolve-stack", revset, error))?;
    if stack.is_empty() {
        if let Some(target) = target {
            diagnose_empty_targeted_merge(runner, config, target, revset, &frozen_bookmarks)?;
        }
        return Ok(summary);
    }
    validate_stack_shape(&stack, revset)
        .map_err(|error| phase_error("resolve-stack", revset, error))?;
    let stack_resolution = resolve_stack_resolution(runner, stack, frozen_bookmarks)
        .map_err(|error| phase_error("resolve-stack", "frozen-dependencies", error))?;

    let github = GitHubContext::resolve(runner)
        .map_err(|error| phase_error("resolve-github", "github", error))?;
    let context = AppContext::new(github, stack_resolution);
    if diagnostics.verbose {
        print_github_context(&context.github);
        print_stack(&context.stack);
    }

    // Validate frozen dependencies against the bottom owned PR.
    let bottom = context
        .stack
        .first()
        .with_context(|| format!("phase=resolve-stack object={revset} empty stack"))?;
    let (_, bottom_pr) = resolve_merge_pr(runner, config, &context, bottom, diagnostics)
        .map_err(|error| phase_error("merge-pr-lookup", &bottom.change_id, error))?;
    validate_merge_frozen_dependencies(runner, config, &context, &bottom_pr).map_err(|error| {
        phase_error(
            "merge-frozen-check",
            format!("PR #{}", bottom_pr.number),
            error,
        )
    })?;

    // Resolve and validate every PR in the stack. Re-point each PR's base to
    // trunk so a single fast-forward push of trunk auto-merges all of them:
    // GitHub only marks a PR merged once its head lands in its *base* branch.
    let mut pr_numbers: Vec<u64> = Vec::new();
    // Every stack branch ends up fully merged into trunk, so collect their head
    // branches to delete once the merge is verified.
    let mut merged_branches: Vec<String> = Vec::new();

    // Pass 1: resolve every PR and fire all the base re-point PATCHes up front.
    // Re-pointing invalidates GitHub's cached mergeability; batching the PATCHes
    // lets those recomputes overlap instead of waiting on each PR in series.
    let mut candidates: Vec<MergeCandidate> = Vec::new();
    let checking_progress = diagnostics.progress_bar("Checking", "PRs", context.stack.len());
    for (index, change) in context.stack.iter().enumerate() {
        let (entry, mut pr) = match resolve_merge_pr(runner, config, &context, change, diagnostics)
        {
            Ok(resolved) => resolved,
            Err(error) => {
                if let Some(progress) = checking_progress {
                    ui_finish_progress_bar(progress);
                }
                return Err(phase_error("merge-pr-lookup", &change.change_id, error));
            }
        };
        if pr.state.eq_ignore_ascii_case("MERGED") {
            merged_branches.push(pr.head_ref_name.clone());
            if let Some(progress) = &checking_progress {
                progress.set_position((index + 1) as u64);
            }
            continue;
        }
        let mut needs_settle = false;
        if !pr.base_ref_name.eq_ignore_ascii_case(&config.trunk) {
            if let Err(error) = repoint_pr_base(
                runner,
                &context.github,
                entry.pr_number,
                &config.trunk,
                diagnostics,
            ) {
                if let Some(progress) = checking_progress {
                    ui_finish_progress_bar(progress);
                }
                return Err(phase_error(
                    "merge-repoint-base",
                    format!("PR #{}", entry.pr_number),
                    error,
                ));
            }
            if diagnostics.dry_run {
                // The PATCH was only planned; reflect the intended base in memory
                // (a re-fetch would still report the old base and fail validation).
                pr.base_ref_name = config.trunk.clone();
            } else {
                // The real PATCH executed; pass 2 re-fetches to pick up the new
                // base and the recomputed mergeability.
                needs_settle = true;
            }
            summary.local_updates += 1;
        } else if !diagnostics.dry_run && mergeability_unknown(&pr) {
            // Already on trunk but mergeability is still settling.
            needs_settle = true;
        }
        candidates.push(MergeCandidate {
            change,
            entry,
            pr,
            needs_settle,
        });
        if let Some(progress) = &checking_progress {
            progress.set_position((index + 1) as u64);
        }
    }
    if let Some(progress) = checking_progress {
        ui_finish_progress_bar(progress);
    }

    // Pass 2: settle mergeability for all re-pointed / unknown PRs together, with
    // one shared backoff sleep per round (a no-op in dry-run, where nothing was
    // flagged for settling).
    settle_candidates_mergeability(runner, &context.github, &mut candidates, diagnostics)
        .map_err(|error| phase_error("merge-pr-lookup", "stack", error))?;

    // Pass 3: validate each PR and record it for the push.
    for candidate in &candidates {
        validate_pr_ready_for_merge(
            config,
            candidate.change,
            &candidate.entry,
            &candidate.pr,
            admin,
        )
        .map_err(|error| {
            phase_error(
                "merge-pr-check",
                format!("PR #{}", candidate.entry.pr_number),
                error,
            )
        })?;
        pr_numbers.push(candidate.entry.pr_number);
        merged_branches.push(candidate.pr.head_ref_name.clone());
        summary.checked_prs += 1;
    }

    if pr_numbers.is_empty() {
        return Ok(summary);
    }

    let top = context
        .stack
        .last()
        .with_context(|| format!("phase=resolve-stack object={revset} empty stack"))?;

    if diagnostics.dry_run {
        diagnostics.plan_line(&format!(
            "- fast-forward trunk `{}` to {} ({})",
            config.trunk, top.change_id, top.commit_id
        ));
        diagnostics.plan_line(&format!(
            "- push trunk `{}` to {} in a single push; GitHub auto-merges {} PR(s) by reachability",
            config.trunk,
            config.remote,
            pr_numbers.len()
        ));
        for pr_number in &pr_numbers {
            diagnostics.plan_line(&format!("- expect PR #{pr_number} to be marked merged"));
        }
        for branch in &merged_branches {
            diagnostics.plan_line(&format!(
                "- delete merged branch `{branch}` locally and on `{}`",
                config.remote
            ));
        }
        return Ok(summary);
    }

    // Fast-forward trunk over the whole stack and push once. This preserves the
    // individual commits (no squash).
    diagnostics.phase("merge-push");
    fast_forward_trunk_over_stack(runner, config, &top.commit_id, diagnostics)
        .map_err(|error| phase_error("merge-push", &config.trunk, error))?;
    summary.submit_runs += 1;

    // GitHub marks each PR merged asynchronously once its head lands in trunk.
    verify_prs_merged(runner, &context.github, &pr_numbers, diagnostics)
        .map_err(|error| phase_error("verify-merge", &context.github.repo, error))?;
    summary.merged_prs = pr_numbers.len();

    // Delete the now-merged stack branches (local + remote). All bases were
    // re-pointed to trunk above, so this never cascade-closes an open PR. The
    // merge already succeeded, so treat cleanup as best-effort: a failure here
    // (e.g. GitHub already auto-deleted the head branch) must not fail the merge.
    summary.cleaned_branches += cleanup_merged_branches(
        runner,
        config,
        &context.github,
        &merged_branches,
        diagnostics,
    );

    // Leave the working copy on a fresh empty change on top of the new trunk.
    diagnostics.phase("reset-working-copy");
    reset_working_copy_to_trunk(runner, config, diagnostics)
        .map_err(|error| phase_error("reset-working-copy", &config.trunk, error))?;

    Ok(summary)
}

/// Re-point a PR's base branch to `trunk` so that a fast-forward push of trunk
/// is recognized by GitHub as merging the PR (GitHub keys auto-merge off the
/// PR's base branch, not just any branch the head lands in).
fn repoint_pr_base(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    trunk: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let endpoint = format!("repos/{}/pulls/{}", github.repo, pr_number);
    let base_arg = format!("base={trunk}");
    let args = [
        "api",
        "-X",
        "PATCH",
        endpoint.as_str(),
        "-f",
        base_arg.as_str(),
    ];
    if diagnostics.dry_run {
        diagnostics.plan_line(&format!("- {}", display_command("gh", &args)));
        return Ok(());
    }
    diagnostics.command("gh", &args);
    let output = gh_run(runner, &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("gh", &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

/// Fast-forward the local trunk bookmark over the merged stack and push it once.
/// Refuses anything but a strict fast-forward over the current remote tip.
fn fast_forward_trunk_over_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    top_commit: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let remote_git_ref = remote_git_ref(config);
    let remote = git_rev_parse(runner, &remote_git_ref)?;
    let is_ancestor = git_run(
        runner,
        &["merge-base", "--is-ancestor", remote.as_str(), top_commit],
    )?;
    if !is_ancestor.success {
        bail!(
            "trunk `{}` cannot fast-forward to {}: remote {} is not an ancestor; run `forklift sync` first",
            config.trunk,
            top_commit,
            remote
        );
    }

    // jj's default `git.auto-local-bookmark = false` leaves a fetched remote
    // trunk bookmark untracked, and `jj bookmark set <trunk>` below then creates
    // a *separate* non-tracking local bookmark. The subsequent push would fail
    // with "Non-tracking remote bookmark <trunk>@<remote> exists". Repair it
    // here (with a warning) instead of bailing.
    ensure_trunk_tracked(runner, config, diagnostics)?;

    let set_args = ["bookmark", "set", config.trunk.as_str(), "-r", top_commit];
    diagnostics.command("jj", &set_args);
    let set = runner.run("jj", &set_args)?;
    if !set.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &set_args),
            set.stderr.trim()
        );
    }

    let push_args = [
        "git",
        "push",
        "--remote",
        config.remote.as_str(),
        "--bookmark",
        config.trunk.as_str(),
    ];
    diagnostics.command("jj", &push_args);
    let push = runner.run("jj", &push_args)?;
    if !push.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &push_args),
            push.stderr.trim()
        );
    }
    Ok(())
}

/// Ensure the local trunk bookmark is tracking `<trunk>@<remote>` before we push
/// it. With jj's default `git.auto-local-bookmark = false`, a fetched remote
/// trunk bookmark is left untracked and forklift's own `jj bookmark set <trunk>`
/// creates a non-tracking local bookmark, so the fast-forward push would abort
/// with "Non-tracking remote bookmark <trunk>@<remote> exists". Rather than fail
/// the merge over a recoverable local-state quirk, auto-track it and warn.
#[tracing::instrument(skip_all)]
fn ensure_trunk_tracked(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let status = remote_bookmark_status(runner, config, &config.trunk)?;
    if status.tracked {
        return Ok(());
    }
    ui_warn!(
        "trunk `{}@{}` was untracked; auto-tracking it so the merge can fast-forward push (jj's default git.auto-local-bookmark=false leaves it untracked)",
        config.trunk,
        config.remote
    );
    diagnostics.warn(format!(
        "trunk `{}@{}` was untracked before merge push; auto-tracking",
        config.trunk, config.remote
    ));
    track_remote_bookmark(runner, config, &config.trunk, diagnostics)
}

/// Poll GitHub until every PR is marked merged. GitHub applies the
/// reachability-based merge asynchronously after the push, so we retry.
/// Shared poll tuning for the merge GitHub-settling loops. Polls start short so a
/// quick recompute returns fast, then back off exponentially up to a cap. Across
/// `POLL_MAX_ATTEMPTS` this keeps the worst-case wait in a ~60-90s budget while
/// reading immediately on the common fast-settle case.
const POLL_INITIAL_DELAY_MS: u64 = 500;
const POLL_MAX_DELAY_MS: u64 = 4000;
// 20 attempts with the backoff schedule above caps the worst-case wait at ~75s
// (0.5+1+2+4 then 4s each), in the same budget as the old flat 30×2s=60s while
// returning much faster on the common quick-settle case.
const POLL_MAX_ATTEMPTS: usize = 20;

/// Sleep `delay_ms`, then return the next (doubled, capped) backoff delay.
fn poll_backoff_sleep(delay_ms: u64) -> u64 {
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    delay_ms.saturating_mul(2).min(POLL_MAX_DELAY_MS)
}

fn verify_prs_merged(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_numbers: &[u64],
    diagnostics: Diagnostics,
) -> Result<()> {
    let mut pending: Vec<u64> = pr_numbers.to_vec();
    let total = pending.len();
    let progress = diagnostics.progress_bar("Verifying", "merged PRs", total);
    let mut delay_ms = POLL_INITIAL_DELAY_MS;
    for attempt in 0..POLL_MAX_ATTEMPTS {
        pending.retain(|&pr_number| !pr_is_merged(runner, github, pr_number).unwrap_or(false));
        if let Some(progress) = &progress {
            progress.set_position((total - pending.len()) as u64);
        }
        if pending.is_empty() {
            if let Some(progress) = progress {
                ui_finish_progress_bar(progress);
            }
            return Ok(());
        }
        if diagnostics.verbose {
            let message = format!(
                "waiting for GitHub to mark {} PR(s) merged (attempt {}/{POLL_MAX_ATTEMPTS})",
                pending.len(),
                attempt + 1
            );
            if let Some(progress) = &progress {
                progress.suspend(|| eprintln!("{message}"));
            } else {
                eprintln!("{message}");
            }
        }
        delay_ms = poll_backoff_sleep(delay_ms);
    }
    if let Some(progress) = progress {
        ui_finish_progress_bar(progress);
    }
    bail!(
        "PRs not marked merged after push: {}; their head commits are in trunk but GitHub has not closed them",
        pending
            .iter()
            .map(|n| format!("#{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn pr_is_merged(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
) -> Result<bool> {
    let pr_arg = pr_number.to_string();
    let args = [
        "pr",
        "view",
        pr_arg.as_str(),
        "--repo",
        github.repo.as_str(),
        "--json",
        "state",
        "--jq",
        ".state",
    ];
    let output = gh_run(runner, &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("gh", &args),
            output.stderr.trim()
        );
    }
    Ok(output.stdout.trim().eq_ignore_ascii_case("MERGED"))
}

/// Leave the working copy on a fresh empty change on top of the new trunk tip.
fn reset_working_copy_to_trunk(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = ["new", config.trunk.as_str()];
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

/// Return true if any *open* PR uses `branch` as its base. Deleting such a
/// branch would cascade-close that PR (a stacked PR closes when its base branch
/// is deleted), so callers must refuse to clean up a branch that still anchors
/// an open PR.
fn open_pr_bases_on_branch(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    branch: &str,
) -> Result<bool> {
    let args = [
        "pr",
        "list",
        "--repo",
        github.repo.as_str(),
        "--base",
        branch,
        "--state",
        "open",
        "--json",
        "number",
    ];
    let output = gh_run(runner, &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("gh", &args),
            output.stderr.trim()
        );
    }
    let prs: Vec<serde_json::Value> = serde_json::from_str(output.stdout.trim())
        .with_context(|| format!("parse open PRs based on `{branch}`"))?;
    Ok(!prs.is_empty())
}

/// Delete a fully-merged stack branch locally and on the remote.
///
/// Refuses (and reports) if an open PR still bases on the branch, so cleanup
/// can never cascade-close a downstream PR. Returns whether the branch was
/// actually removed.
/// Delete fully-merged stack branches: the local bookmark for each, then ONE
/// batched `jj git push` that propagates every deletion to the remote in a single
/// invocation (jj pushes a deleted tracked bookmark as a remote delete).
///
/// Refuses any branch an open PR still bases on (the cascade-close guard — a
/// stacked PR closes when its base branch is deleted). Best-effort throughout: a
/// failed guard/delete/push warns rather than erroring, since the merge or sync
/// it follows has already succeeded. Returns how many branches were removed.
fn cleanup_merged_branches(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    branches: &[String],
    diagnostics: Diagnostics,
) -> usize {
    let mut to_push: Vec<&str> = Vec::new();
    let mut cleaned = 0;
    let progress = diagnostics.progress_bar("Cleaning", "branches", branches.len());
    for (index, branch) in branches.iter().enumerate() {
        match open_pr_bases_on_branch(runner, github, branch) {
            Ok(true) => {
                diagnostics.warn(format!(
                    "skipping cleanup of `{branch}`: an open PR still targets it as its base"
                ));
                if let Some(progress) = &progress {
                    progress.set_position((index + 1) as u64);
                }
                continue;
            }
            Ok(false) => {}
            Err(error) => {
                diagnostics.warn(format!(
                    "could not check open PRs basing on `{branch}`, leaving it: {error:#}"
                ));
                if let Some(progress) = &progress {
                    progress.set_position((index + 1) as u64);
                }
                continue;
            }
        }
        if diagnostics.dry_run {
            diagnostics.plan_line(&format!(
                "- delete merged branch `{branch}` locally and on `{}`",
                config.remote
            ));
            cleaned += 1;
            if let Some(progress) = &progress {
                progress.set_position((index + 1) as u64);
            }
            continue;
        }
        match delete_bookmark(runner, branch, diagnostics) {
            Ok(()) => {
                cleaned += 1;
                to_push.push(branch);
            }
            Err(error) => diagnostics.warn(format!(
                "could not delete local bookmark `{branch}`: {error:#}"
            )),
        }
        if let Some(progress) = &progress {
            progress.set_position((index + 1) as u64);
        }
    }
    if let Some(progress) = progress {
        ui_finish_progress_bar(progress);
    }
    if !to_push.is_empty() {
        if let Err(error) = push_bookmark_deletions(runner, config, &to_push, diagnostics) {
            diagnostics.warn(format!(
                "could not push branch deletion(s) to `{}`: {error:#}",
                config.remote
            ));
        }
        // Reconcile jj's view of each branch. GitHub's "automatically delete head
        // branch on merge" often removes the remote ref before (or instead of)
        // our deletion push lands, which leaves jj holding a dangling
        // `branch@remote` tracking ref — a phantom "(deleted)" bookmark that
        // lingers until the next fetch. Forgetting with --include-remotes drops
        // the local bookmark and its tracking refs without touching the remote,
        // so the post-merge state is clean immediately. Purely local and
        // best-effort: if the push already removed the bookmark this is a no-op.
        for branch in &to_push {
            forget_bookmark_tracking(runner, branch, diagnostics);
        }
    }
    cleaned
}

/// Forget a bookmark and its remote-tracking refs locally (no remote mutation).
/// Best-effort: used after a deletion push to clear any tracking ref left
/// dangling when the remote branch was already gone (e.g. GitHub auto-deleted the
/// merged head). Any failure here is a non-event, so errors are swallowed.
fn forget_bookmark_tracking(runner: &impl CommandRunner, branch: &str, diagnostics: Diagnostics) {
    if diagnostics.dry_run {
        return;
    }
    let args = ["bookmark", "forget", "--include-remotes", branch];
    diagnostics.command("jj", &args);
    let _ = runner.run("jj", &args);
}

/// Push the deletion of one or more bookmarks to the remote in a single
/// `jj git push` invocation.
fn push_bookmark_deletions(
    runner: &impl CommandRunner,
    config: &AppConfig,
    branches: &[&str],
    diagnostics: Diagnostics,
) -> Result<()> {
    let mut args: Vec<&str> = vec!["git", "push", "--remote", config.remote.as_str()];
    for branch in branches {
        args.push("--bookmark");
        args.push(branch);
    }
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

/// List local stack bookmarks (those under the configured branch prefix that
/// have no remote-only counterpart), regardless of which revision they point at.
fn local_stack_bookmarks(runner: &impl CommandRunner, config: &AppConfig) -> Result<Vec<String>> {
    let args = ["bookmark", "list", "-T", LOCAL_BOOKMARK_TEMPLATE];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let prefix = format!("{}/", config.branch_prefix.trim_end_matches('/'));
    let mut bookmarks = output
        .stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let remote = fields.next().unwrap_or_default().trim();
            if !remote.is_empty() || !name.starts_with(&prefix) {
                return None;
            }
            Some(name.to_owned())
        })
        .collect::<Vec<_>>();
    bookmarks.sort();
    bookmarks.dedup();
    Ok(bookmarks)
}

/// Clean up stack branches whose commits have already landed in trunk (e.g. from
/// a prior merge). A branch is "landed" when its commit is an ancestor of the
/// remote trunk tip, which for the fast-forward merge model means its PR merged.
/// Returns the number of branches removed.
fn cleanup_landed_branches(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<usize> {
    let bookmarks = local_stack_bookmarks(runner, config)?;
    if bookmarks.is_empty() {
        return Ok(0);
    }
    let trunk_tip = git_rev_parse(runner, &remote_git_ref(config))?;
    let mut landed = Vec::new();
    for branch in bookmarks {
        let commit = jj_ref_commit_id(runner, &branch)?;
        if git_commit_is_ancestor(runner, &commit, &trunk_tip)? {
            landed.push(branch);
        }
    }
    if landed.is_empty() {
        return Ok(0);
    }

    let github = GitHubContext::resolve(runner)
        .context("resolve GitHub repository for merged-branch cleanup")?;
    Ok(cleanup_merged_branches(
        runner,
        config,
        &github,
        &landed,
        diagnostics,
    ))
}

/// Republish the PRs left above a targeted merge.
///
/// `forklift merge <target>` narrows the merge to `::target`, so PRs above the
/// target are never re-submitted by the merge loop even though their changes
/// were rebased (when the merged changes were abandoned) and their stack
/// comments still list the now-merged PRs. Re-resolving the full stack and
/// submitting drops the merged PRs from those comments and pushes the rebased
/// branches. Resolves to the post-merge stack, so merged/abandoned changes are
/// already absent; if nothing remains above the target this is a no-op.
#[tracing::instrument(skip_all)]
fn refresh_stack_above_merge(
    runner: &impl CommandRunner,
    config: &AppConfig,
    revset: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let remaining = resolve_stack(runner, revset)?;
    if remaining.is_empty() {
        return Ok(());
    }

    diagnostics.phase("merge-refresh-above");
    let context = resolve_stack_context(runner, revset)?;
    submit_stack(
        runner,
        config,
        &context,
        true,
        "forklift submit --yes",
        diagnostics,
    )?;
    Ok(())
}

#[tracing::instrument(skip_all)]
fn validate_merge_frozen_dependencies(
    runner: &impl CommandRunner,
    config: &AppConfig,
    context: &AppContext,
    bottom_owned_pr: &GhMergePr,
) -> Result<()> {
    if context.frozen_dependencies.is_empty() {
        return Ok(());
    }

    for dependency in &context.frozen_dependencies {
        let pr = fetch_pr_for_merge(
            runner,
            &context.github,
            &dependency.change.change_id,
            dependency.bookmark.pr_number,
        )?;
        if pr.state.eq_ignore_ascii_case("OPEN") {
            bail!(
                "frozen dependency `{}` is still open as PR #{}; merge dependencies first, then run `forklift sync` before merging owned PRs",
                dependency.bookmark.name,
                pr.number
            );
        }
        if !pr.state.eq_ignore_ascii_case("MERGED") {
            bail!(
                "frozen dependency `{}` PR #{} is `{}`; run `forklift sync` before merging owned PRs",
                dependency.bookmark.name,
                pr.number,
                pr.state
            );
        }
    }

    if !bottom_owned_pr
        .base_ref_name
        .eq_ignore_ascii_case(&config.trunk)
    {
        bail!(
            "frozen dependencies are merged, but bottom owned PR #{} still targets `{}` instead of trunk `{}`; run `forklift sync` before merging",
            bottom_owned_pr.number,
            bottom_owned_pr.base_ref_name,
            config.trunk
        );
    }

    Ok(())
}

fn resolve_merge_pr(
    runner: &impl CommandRunner,
    config: &AppConfig,
    context: &AppContext,
    change: &ResolvedChange,
    diagnostics: Diagnostics,
) -> Result<(PrCacheEntry, GhMergePr)> {
    let store = CacheStore::load_current_best_effort(runner, diagnostics, "merge")?;
    if let Some(entry) = store.get_pr(&context.github.repo, &change.change_id) {
        match resolve_merge_cached_pr(runner, config, &context.github, change, entry) {
            Ok(resolved) => return Ok(resolved),
            Err(error) => diagnostics.warn(format!(
                "phase=merge-pr-lookup object=cache:{} error=ignored stale cache hint: {error:#}",
                change.change_id
            )),
        }
    }

    resolve_merge_pr_from_live_bookmarks(runner, config, &context.github, change)
}

#[tracing::instrument(skip_all, fields(change = %change.change_id, pr = entry.pr_number))]
fn resolve_merge_cached_pr(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    change: &ResolvedChange,
    entry: &PrCacheEntry,
) -> Result<(PrCacheEntry, GhMergePr)> {
    let pr = fetch_pr_for_merge(runner, github, &change.change_id, entry.pr_number)?;
    if pr.head_ref_name != entry.head_branch {
        bail!(
            "cache points to PR #{} on `{}`, but GitHub reports `{}`",
            entry.pr_number,
            entry.head_branch,
            pr.head_ref_name
        );
    }

    let live_entry = pr.clone().into_cache_entry(entry.stack_comment_id.clone());
    validate_submit_bookmark_state(runner, config, change, &live_entry)?;
    Ok((live_entry, pr))
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
fn resolve_merge_pr_from_live_bookmarks(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    change: &ResolvedChange,
) -> Result<(PrCacheEntry, GhMergePr)> {
    let mut matches = Vec::new();
    for head_branch in local_stack_bookmarks_for_change(runner, config, change)? {
        if let Some(entry) =
            lookup_open_pr_by_head_branch(runner, github, &change.change_id, &head_branch)?
        {
            validate_submit_bookmark_state(runner, config, change, &entry)?;
            let pr = fetch_pr_for_merge(runner, github, &change.change_id, entry.pr_number)?;
            if pr.head_ref_name != head_branch {
                bail!(
                    "PR #{} head branch is `{}`, but live bookmark discovery found `{}`",
                    entry.pr_number,
                    pr.head_ref_name,
                    head_branch
                );
            }
            matches.push((entry.stack_comment_id.clone(), pr));
        }
    }

    match matches.as_slice() {
        [(comment_id, pr)] => Ok((pr.clone().into_cache_entry(comment_id.clone()), pr.clone())),
        [] => bail!(
            "no live tracked PR found for {}/{}; run `forklift submit` before merging so forklift can verify the owned PR",
            github.repo,
            change.change_id
        ),
        _ => bail!(
            "multiple live tracked PRs found for {}/{}; refusing to choose before merge",
            github.repo,
            change.change_id
        ),
    }
}

#[tracing::instrument(skip_all, fields(change = %change_id, pr = pr_number))]
fn fetch_pr_for_merge(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    change_id: &str,
    pr_number: u64,
) -> Result<GhMergePr> {
    let pr_number_arg = pr_number.to_string();
    let args = [
        "pr",
        "view",
        pr_number_arg.as_str(),
        "--repo",
        github.repo.as_str(),
        "--json",
        MERGE_PR_JSON_FIELDS,
    ];
    let output = gh_run(runner, &args)?;
    if !output.success {
        bail!(
            "failed-api=`{}` error={} change:{}",
            display_command("gh", &args),
            output.stderr.trim(),
            change_id
        );
    }

    serde_json::from_str(&output.stdout)
        .with_context(|| format!("parse merge metadata for PR #{} ({change_id})", pr_number))
}

/// True when GitHub has not yet computed a PR's mergeability (`UNKNOWN`/missing).
fn mergeability_unknown(pr: &GhMergePr) -> bool {
    pr.mergeable
        .as_deref()
        .map(|state| state.eq_ignore_ascii_case("UNKNOWN"))
        .unwrap_or(true)
}

/// A stack PR being prepared for merge, plus whether it still needs a fresh fetch
/// to settle its mergeability (true after a base re-point, or when GitHub reports
/// a transient `UNKNOWN`).
struct MergeCandidate<'a> {
    change: &'a ResolvedChange,
    entry: PrCacheEntry,
    pr: GhMergePr,
    needs_settle: bool,
}

/// Settle mergeability for all candidates that need it, polling GitHub with a
/// single shared backoff sleep per round instead of waiting on each PR in series.
///
/// Re-pointing a PR's base branch invalidates GitHub's cached mergeability, so
/// the first read often returns `UNKNOWN` while it recomputes in the background.
/// Firing all the re-point PATCHes first (in the caller) and then polling the
/// whole set together turns sum-of-waits into max-of-waits. Each candidate's `pr`
/// is refreshed in place; PRs that never settle keep their last value so
/// validation can surface a clear error.
fn settle_candidates_mergeability(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    candidates: &mut [MergeCandidate<'_>],
    diagnostics: Diagnostics,
) -> Result<()> {
    // Every candidate marked `needs_settle` must be fetched at least once (a
    // re-pointed PR's in-memory copy still has the stale base/mergeability).
    let mut pending: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.needs_settle)
        .map(|(index, _)| index)
        .collect();
    let total = pending.len();
    let progress = diagnostics.progress_bar("Settling", "mergeability", total);
    let mut delay_ms = POLL_INITIAL_DELAY_MS;
    for attempt in 0..POLL_MAX_ATTEMPTS {
        if pending.is_empty() {
            break;
        }
        let mut still_pending = Vec::new();
        for &index in &pending {
            let candidate = &mut candidates[index];
            candidate.pr = match fetch_pr_for_merge(
                runner,
                github,
                &candidate.change.change_id,
                candidate.entry.pr_number,
            ) {
                Ok(pr) => pr,
                Err(error) => {
                    if let Some(progress) = progress {
                        ui_finish_progress_bar(progress);
                    }
                    return Err(error);
                }
            };
            if mergeability_unknown(&candidate.pr) {
                still_pending.push(index);
            }
        }
        pending = still_pending;
        if let Some(progress) = &progress {
            progress.set_position((total - pending.len()) as u64);
        }
        if pending.is_empty() {
            break;
        }
        if diagnostics.verbose {
            let message = format!(
                "waiting for GitHub to compute mergeability for {} PR(s) (attempt {}/{POLL_MAX_ATTEMPTS})",
                pending.len(),
                attempt + 1
            );
            if let Some(progress) = &progress {
                progress.suspend(|| eprintln!("{message}"));
            } else {
                eprintln!("{message}");
            }
        }
        delay_ms = poll_backoff_sleep(delay_ms);
    }
    if let Some(progress) = progress {
        ui_finish_progress_bar(progress);
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(change = %change.change_id, pr = entry.pr_number))]
fn validate_pr_ready_for_merge(
    config: &AppConfig,
    change: &ResolvedChange,
    entry: &PrCacheEntry,
    pr: &GhMergePr,
    admin: bool,
) -> Result<()> {
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            "PR #{} for {} is `{}`; only open PRs can be merged",
            entry.pr_number,
            change.change_id,
            pr.state
        );
    }
    if pr.is_draft {
        bail!(
            "PR #{} is draft; mark it ready for review before merge",
            entry.pr_number
        );
    }
    // The PR's GitHub head, the local jj commit, and our cache must all agree
    // before merging — otherwise we'd land code that isn't what's checked out.
    // Disambiguate the three failure shapes so the fix is unambiguous.
    if pr.head_ref_oid != change.commit_id || pr.head_ref_oid != entry.head_sha {
        if pr.head_ref_oid == entry.head_sha {
            // PR and cache agree; only the local commit moved. This is the
            // common case after `sync` rebased the stack without re-pushing.
            bail!(
                "local change {} is now {}, but PR #{} (and the cache) are still at {}; your stack was rewritten (e.g. by `forklift sync`) but not pushed — run `forklift submit` before merging",
                change.change_id,
                change.commit_id,
                entry.pr_number,
                pr.head_ref_oid
            );
        }
        if change.commit_id == entry.head_sha {
            // Local commit and cache agree; the PR head moved on GitHub. The
            // branch advanced out-of-band, so refresh local state then re-push.
            bail!(
                "PR #{} head is {} on GitHub, but your local change {} and the cache are both at {}; the PR moved out-of-band — run `forklift sync` then `forklift submit` before merging",
                entry.pr_number,
                pr.head_ref_oid,
                change.change_id,
                change.commit_id
            );
        }
        // All three disagree — local, cache, and GitHub have fully drifted.
        bail!(
            "PR #{} is out of sync: GitHub head {}, local change {} is {}, cache expects {}; run `forklift sync` then `forklift submit` before merging",
            entry.pr_number,
            pr.head_ref_oid,
            change.change_id,
            change.commit_id,
            entry.head_sha
        );
    }
    if !pr.base_ref_name.eq_ignore_ascii_case(&config.trunk) {
        bail!(
            "PR #{} base is `{}`, but the bottom of the stack must target trunk `{}`; run `forklift submit` to repoint the base before merging",
            entry.pr_number,
            pr.base_ref_name,
            config.trunk
        );
    }
    if config.require_approval && pr.review_decision.as_deref() != Some("APPROVED") {
        bail!(
            "PR #{} requires approval; reviewDecision is `{}`",
            entry.pr_number,
            pr.review_decision.as_deref().unwrap_or("NONE")
        );
    }
    if pr.auto_merge_request.is_some() {
        bail!(
            "PR #{} has auto-merge enabled; disable auto-merge before using direct squash merge",
            entry.pr_number
        );
    }
    if pr.mergeable.as_deref() != Some("MERGEABLE") {
        bail!(
            "PR #{} is not mergeable; mergeable is `{}`",
            entry.pr_number,
            pr.mergeable.as_deref().unwrap_or("UNKNOWN")
        );
    }

    match pr.merge_state_status.as_deref().unwrap_or("UNKNOWN") {
        "CLEAN" | "UNSTABLE" => {}
        // With --admin the operator force-pushes trunk past branch protection,
        // so a BLOCKED state is expected and allowed.
        "BLOCKED" if admin => {}
        "QUEUED" => bail!(
            "PR #{} is in a merge queue; this workflow only supports direct squash merge",
            entry.pr_number
        ),
        "HAS_HOOKS" => bail!(
            "PR #{} is waiting on pending deployments or repository hooks; direct squash merge is not safe",
            entry.pr_number
        ),
        "BLOCKED" => bail!(
            "PR #{} is blocked by branch protection or an admin-only merge path; direct squash merge is not supported",
            entry.pr_number
        ),
        status => bail!(
            "PR #{} cannot be directly squash merged; mergeStateStatus is `{}`",
            entry.pr_number,
            status
        ),
    }

    // --admin bypasses required status checks via branch protection, so skip the
    // client-side status-check gate; otherwise enforce it.
    if !admin {
        validate_status_checks(entry.pr_number, &pr.status_check_rollup)?;
    }

    Ok(())
}

/// Require every reported status check to pass before a direct squash merge.
///
/// Note: `statusCheckRollup` reports *all* checks on the PR, not only the ones
/// branch protection marks as required. This intentionally fails closed — any
/// failing or pending check blocks the merge — so the messages say "checks",
/// not "required checks", to avoid implying we consulted branch protection.
#[tracing::instrument(skip_all, fields(pr = pr_number))]
fn validate_status_checks(pr_number: u64, checks: &[serde_json::Value]) -> Result<()> {
    for check in checks {
        let name = check_name(check);
        if let Some(state) = check.get("state").and_then(serde_json::Value::as_str) {
            if state != "SUCCESS" {
                bail!(
                    "PR #{} checks are not passing: `{}` is `{}`",
                    pr_number,
                    name,
                    state
                );
            }
            continue;
        }

        let status = check
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("UNKNOWN");
        let conclusion = check
            .get("conclusion")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("UNKNOWN");
        if status != "COMPLETED" {
            bail!(
                "PR #{} checks are pending: `{}` is `{}`",
                pr_number,
                name,
                status
            );
        }
        if !matches!(conclusion, "SUCCESS" | "SKIPPED" | "NEUTRAL") {
            bail!(
                "PR #{} checks are not passing: `{}` concluded `{}`",
                pr_number,
                name,
                conclusion
            );
        }
    }

    Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
fn check_name(check: &serde_json::Value) -> String {
    check
        .get("name")
        .or_else(|| check.get("context"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown>")
        .to_owned()
}

fn sync_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    revset: &str,
    submit: bool,
    yes: bool,
    diagnostics: Diagnostics,
) -> Result<SyncSummary> {
    diagnostics.phase("sync-fetch");
    fetch_remote(runner, config, diagnostics)
        .map_err(|error| phase_error("sync-fetch", &config.remote, error))?;

    // Remove stack branches whose commits already landed in trunk (e.g. merged
    // by a prior `forklift merge` or directly on GitHub). Done before resolving
    // the stack so it still runs when no owned stack remains after a merge.
    let cleaned_branches = cleanup_landed_branches(runner, config, diagnostics)
        .map_err(|error| phase_error("cleanup-merged", "branches", error))?;

    diagnostics.phase("resolve-stack");
    resolve_single_rev(runner, "trunk()")
        .map_err(|error| phase_error("resolve-stack", "trunk()", error))?;
    let frozen_bookmarks = frozen_bookmarks(runner)
        .map_err(|error| phase_error("resolve-stack", "frozen-bookmarks", error))?;
    let stack = resolve_stack(runner, revset)
        .map_err(|error| phase_error("resolve-stack", revset, error))?;
    // Nothing left to sync (e.g. the whole stack just merged). Move trunk to the
    // fetched remote tip and finish, reporting any branches we cleaned up rather
    // than failing on the empty stack.
    if stack.is_empty() && frozen_bookmarks.is_empty() {
        diagnostics.phase("move-trunk");
        move_trunk_to_remote(runner, config, diagnostics)
            .map_err(|error| phase_error("move-trunk", &config.trunk, error))?;
        return Ok(SyncSummary {
            rebased_roots: 0,
            submit_ran: false,
            cleaned_branches,
        });
    }
    let stack_resolution = if stack.is_empty() {
        resolve_purely_frozen_stack(runner, frozen_bookmarks)
    } else {
        (|| {
            validate_stack_shape(&stack, revset)?;
            resolve_stack_resolution(runner, stack, frozen_bookmarks)
        })()
    }
    .map_err(|error| phase_error("resolve-stack", revset, error))?;
    if diagnostics.verbose {
        print_stack(&stack_resolution.owned);
    }

    diagnostics.phase("sync-frozen");
    let frozen_refresh =
        sync_refresh_frozen_dependencies(runner, config, &stack_resolution, diagnostics)
            .map_err(|error| phase_error("sync-frozen", "frozen dependencies", error))?;

    diagnostics.phase("move-trunk");
    move_trunk_to_remote(runner, config, diagnostics)
        .map_err(|error| phase_error("move-trunk", &config.trunk, error))?;

    if stack_resolution.owned.is_empty() {
        return Ok(SyncSummary {
            rebased_roots: 0,
            submit_ran: false,
            cleaned_branches,
        });
    }

    diagnostics.phase("rebase-stack");
    let destination = if frozen_refresh.active_dependencies {
        sync_rebase_destination(config, &stack_resolution)
    } else {
        RebaseDestination::Trunk(config.trunk.clone())
    };
    let rebased_roots =
        rebase_stack_roots(runner, &stack_resolution.owned, destination, diagnostics)
            .map_err(|error| phase_error("rebase-stack", revset, error))?;

    if !submit {
        return Ok(SyncSummary {
            rebased_roots,
            submit_ran: false,
            cleaned_branches,
        });
    }

    if diagnostics.dry_run {
        diagnostics.plan_line("- run submit after sync");
        return Ok(SyncSummary {
            rebased_roots,
            submit_ran: true,
            cleaned_branches,
        });
    }

    diagnostics.phase("sync-submit");
    let mut context = resolve_stack_context(runner, revset)
        .map_err(|error| phase_error("sync-submit", revset, error))?;
    if let Some(github) = frozen_refresh.github {
        context.github = github;
    }
    if diagnostics.verbose {
        print_github_context(&context.github);
        print_stack(&context.stack);
    }
    submit_stack(
        runner,
        config,
        &context,
        yes,
        "forklift sync --submit --yes",
        diagnostics,
    )
    .map_err(|error| phase_error("sync-submit", "submit", error))?;

    Ok(SyncSummary {
        rebased_roots,
        submit_ran: true,
        cleaned_branches,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncFrozenRefresh {
    github: Option<GitHubContext>,
    active_dependencies: bool,
}

#[tracing::instrument(skip_all)]
fn sync_refresh_frozen_dependencies(
    runner: &impl CommandRunner,
    config: &AppConfig,
    stack_resolution: &StackResolution,
    diagnostics: Diagnostics,
) -> Result<SyncFrozenRefresh> {
    if stack_resolution.frozen_dependencies.is_empty() {
        return Ok(SyncFrozenRefresh {
            github: None,
            active_dependencies: false,
        });
    }

    let github = GitHubContext::resolve(runner)
        .context("resolve GitHub repository for frozen dependencies")?;
    let mut prs = Vec::new();
    for dependency in &stack_resolution.frozen_dependencies {
        let pr = fetch_pr_by_number(
            runner,
            &github,
            "sync-frozen",
            dependency.bookmark.pr_number,
        )?;
        prs.push(pr);
    }
    let active_dependencies = validate_sync_frozen_pr_stack(
        config,
        &github,
        &stack_resolution.frozen_dependencies,
        &prs,
    )?;
    if !active_dependencies {
        return Ok(SyncFrozenRefresh {
            github: Some(github),
            active_dependencies: false,
        });
    }

    fetch_get_branches(runner, config, &prs, diagnostics)
        .map_err(|error| anyhow!("fetch frozen dependency branches: {error}"))?;
    update_get_frozen_bookmarks(runner, &prs, diagnostics)?;
    Ok(SyncFrozenRefresh {
        github: Some(github),
        active_dependencies: true,
    })
}

#[tracing::instrument(skip_all)]
fn validate_sync_frozen_pr_stack(
    config: &AppConfig,
    github: &GitHubContext,
    dependencies: &[FrozenDependency],
    prs: &[GhPr],
) -> Result<bool> {
    if dependencies.len() != prs.len() {
        bail!(
            "internal sync error: {} frozen dependencies but {} GitHub PRs",
            dependencies.len(),
            prs.len()
        );
    }

    let merged_count = prs
        .iter()
        .filter(|pr| pr.state.eq_ignore_ascii_case("MERGED"))
        .count();
    if merged_count == prs.len() {
        return Ok(false);
    }
    if merged_count > 0 {
        bail!(
            "partially merged frozen dependency stack is not recoverable automatically yet: {merged_count}/{} dependencies are merged",
            prs.len()
        );
    }

    for (index, (dependency, pr)) in dependencies.iter().zip(prs).enumerate() {
        validate_sync_frozen_pr_metadata(config, github, dependency, pr)?;
        if index == 0 {
            if pr.base_ref_name != config.trunk {
                bail!(
                    "unexpected retarget for frozen dependency `{}` PR #{}: base branch is `{}`, expected trunk `{}`",
                    dependency.bookmark.name,
                    pr.number,
                    pr.base_ref_name,
                    config.trunk
                );
            }
            continue;
        }

        let previous = &prs[index - 1];
        if pr.base_ref_name != previous.head_ref_name {
            bail!(
                "unexpected retarget for frozen dependency `{}` PR #{}: base branch is `{}`, expected previous frozen PR #{} head branch `{}`",
                dependency.bookmark.name,
                pr.number,
                pr.base_ref_name,
                previous.number,
                previous.head_ref_name
            );
        }
        if pr.base_ref_oid != previous.head_ref_oid {
            bail!(
                "unexpected retarget for frozen dependency `{}` PR #{}: base SHA is {}, expected previous frozen PR #{} head SHA {}",
                dependency.bookmark.name,
                pr.number,
                pr.base_ref_oid,
                previous.number,
                previous.head_ref_oid
            );
        }
    }

    Ok(true)
}

#[tracing::instrument(skip_all, fields(pr = pr.number))]
fn validate_sync_frozen_pr_metadata(
    _config: &AppConfig,
    github: &GitHubContext,
    dependency: &FrozenDependency,
    pr: &GhPr,
) -> Result<()> {
    validate_get_pr_metadata(github, pr)?;
    if pr.state.eq_ignore_ascii_case("CLOSED") {
        bail!(
            "closed-unmerged frozen dependency `{}` points to PR #{}; sync cannot recover a closed PR that was not merged",
            dependency.bookmark.name,
            pr.number
        );
    }
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            "frozen dependency `{}` points to PR #{} but the PR state is `{}`; expected OPEN",
            dependency.bookmark.name,
            pr.number,
            pr.state
        );
    }

    let head_repo = get_pr_repo(pr, "head")?;
    let base_repo = get_pr_repo(pr, "base")?;
    if head_repo.name_with_owner != github.repo {
        bail!(
            "frozen dependency `{}` PR #{} is fork-backed from `{}`; sync only supports same-repo frozen dependencies",
            dependency.bookmark.name,
            pr.number,
            head_repo.name_with_owner
        );
    }
    if base_repo.name_with_owner != github.repo {
        bail!(
            "frozen dependency `{}` PR #{} has base repo `{}`; expected `{}`",
            dependency.bookmark.name,
            pr.number,
            base_repo.name_with_owner,
            github.repo
        );
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn fetch_remote(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = ["git", "fetch", "--remote", config.remote.as_str()];
    if diagnostics.dry_run {
        diagnostics.plan_line(&format!("- {}", display_command("jj", &args)));
        return Ok(());
    }

    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    // The fetch is the gate every later trunk-movement and merge step trusts.
    // Verify it actually produced a resolvable remote trunk bookmark, so a wrong
    // remote name or a fetch that exits 0 without updating refs fails loudly here
    // instead of later as a confusing trunk-movement error.
    let remote_jj_ref = remote_jj_ref(config);
    jj_trunk_remote_commit(runner, config).map_err(|error| {
        anyhow!(
            "`{}` reported success but `{remote_jj_ref}` is not resolvable; check {CONFIG_PREFIX}.remote and {CONFIG_PREFIX}.trunk: {error}",
            display_command("jj", &args)
        )
    })?;

    Ok(())
}

/// Resolve the commit id of the remote trunk bookmark (`<trunk>@<remote>`) as jj
/// sees it. This is the authority jj uses when it moves the local trunk bookmark
/// and rebases, so trunk movement must be based on it rather than the colocated
/// git ref, which can lag in a non-colocated repo.
#[tracing::instrument(skip_all)]
fn jj_trunk_remote_commit(runner: &impl CommandRunner, config: &AppConfig) -> Result<String> {
    let remote_jj_ref = remote_jj_ref(config);
    run_required(
        runner,
        "jj",
        &[
            "log",
            "--no-graph",
            "-r",
            remote_jj_ref.as_str(),
            "-T",
            "commit_id",
        ],
    )
    .with_context(|| format!("resolve jj remote trunk bookmark `{remote_jj_ref}`"))
}

#[tracing::instrument(skip_all)]
fn move_trunk_to_remote(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let local = git_rev_parse(runner, &config.trunk)?;
    let remote_git_ref = remote_git_ref(config);
    let remote = git_rev_parse(runner, &remote_git_ref)?;

    // jj moves the local trunk bookmark and rebases against its own view of the
    // remote bookmark (`<trunk>@<remote>`). In a non-colocated repo
    // `git rev-parse <remote>/<trunk>` can lag that view, which would make the
    // local==remote check below a false positive and silently skip trunk
    // movement. Require the two views to agree so we never base trunk movement on
    // a stale git ref.
    let remote_jj = jj_trunk_remote_commit(runner, config)?;
    if remote_jj != remote {
        bail!(
            "remote trunk views disagree: git `{}` is {} but jj `{}` is {}; the colocated git ref is stale. Run `jj git export` (or verify {CONFIG_PREFIX}.remote) before moving trunk.",
            remote_git_ref,
            remote,
            remote_jj_ref(config),
            remote_jj
        );
    }

    if local == remote {
        diagnostics.plan_line(&format!("- leave trunk `{}` at {}", config.trunk, local));
        return Ok(());
    }

    ensure_trunk_can_fast_forward(runner, config, &local, &remote)?;
    let remote_jj_ref = remote_jj_ref(config);
    let args = [
        "bookmark",
        "set",
        config.trunk.as_str(),
        "-r",
        remote_jj_ref.as_str(),
    ];

    if diagnostics.dry_run {
        diagnostics.plan_line(&format!(
            "- fast-forward trunk `{}` from {} to {}",
            config.trunk, local, remote
        ));
        diagnostics.plan_line(&format!("- {}", display_command("jj", &args)));
        return Ok(());
    }

    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    Ok(())
}

#[tracing::instrument(skip_all, fields(local = %local, remote = %remote))]
fn ensure_trunk_can_fast_forward(
    runner: &impl CommandRunner,
    config: &AppConfig,
    local: &str,
    remote: &str,
) -> Result<()> {
    let remote_ref = remote_git_ref(config);
    let args = ["merge-base", "--is-ancestor", local, remote];
    let output = git_run(runner, &args)?;
    if !output.success {
        bail!(
            "trunk `{}` cannot fast-forward to `{}`: local commit {}, remote commit {}",
            config.trunk,
            remote_ref,
            local,
            remote
        );
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RebaseDestination {
    Trunk(String),
    FrozenBookmark(String),
}

impl RebaseDestination {
    #[tracing::instrument(level = "trace", skip_all)]
    fn rev(&self) -> &str {
        match self {
            Self::Trunk(rev) | Self::FrozenBookmark(rev) => rev,
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn label(&self) -> &str {
        match self {
            Self::Trunk(_) => "trunk",
            Self::FrozenBookmark(_) => "frozen dependency",
        }
    }
}

#[tracing::instrument(skip_all)]
fn sync_rebase_destination(
    config: &AppConfig,
    stack_resolution: &StackResolution,
) -> RebaseDestination {
    stack_resolution
        .frozen_dependencies
        .last()
        .map(|dependency| RebaseDestination::FrozenBookmark(dependency.bookmark.name.clone()))
        .unwrap_or_else(|| RebaseDestination::Trunk(config.trunk.clone()))
}

#[tracing::instrument(skip_all)]
fn rebase_stack_roots(
    runner: &impl CommandRunner,
    stack: &[ResolvedChange],
    destination: RebaseDestination,
    diagnostics: Diagnostics,
) -> Result<usize> {
    let root = stack_root(stack)?;
    let destination_rev = destination.rev().to_owned();
    let args = [
        "rebase",
        "-s",
        root.commit_id.as_str(),
        "-d",
        destination_rev.as_str(),
    ];

    if diagnostics.dry_run {
        diagnostics.plan_line(&format!(
            "- rebase stack root {} ({}) onto {} `{}`",
            root.change_id,
            root.commit_id,
            destination.label(),
            destination.rev()
        ));
        diagnostics.plan_line(&format!("- {}", display_command("jj", &args)));
        return Ok(1);
    }

    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    Ok(1)
}

#[tracing::instrument(level = "trace", skip_all)]
fn stack_root(stack: &[ResolvedChange]) -> Result<&ResolvedChange> {
    let selected_commits = stack
        .iter()
        .map(|change| change.commit_id.as_str())
        .collect::<HashSet<_>>();
    stack
        .iter()
        .find(|change| selected_parent(change, &selected_commits).is_none())
        .context("stack has no root")
}

#[tracing::instrument(level = "trace", skip_all, fields(rev = %rev))]
fn git_rev_parse(runner: &impl CommandRunner, rev: &str) -> Result<String> {
    git_run_required(runner, &["rev-parse", rev])
        .with_context(|| format!("resolve commit id for `{rev}`"))
}

#[tracing::instrument(level = "trace", skip_all)]
fn remote_git_ref(config: &AppConfig) -> String {
    format!("{}/{}", config.remote, config.trunk)
}

#[tracing::instrument(level = "trace", skip_all)]
fn remote_jj_ref(config: &AppConfig) -> String {
    format!("{}@{}", config.trunk, config.remote)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteBookmarkStatus {
    tracked: bool,
    conflicted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FrozenBookmark {
    name: String,
    pr_number: u64,
    commit_id: String,
}

#[tracing::instrument(skip_all)]
fn frozen_bookmarks(runner: &impl CommandRunner) -> Result<Vec<FrozenBookmark>> {
    let args = [
        "bookmark",
        "list",
        "forklift/frozen/*",
        "-T",
        FROZEN_BOOKMARK_TEMPLATE,
    ];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let mut bookmarks = Vec::new();
    for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 3 {
            bail!("parse frozen bookmark row `{line}`: expected 3 tab-separated fields");
        }
        let name = fields[0];
        if !name.starts_with(FROZEN_BOOKMARK_PREFIX) {
            continue;
        }
        let pr_number = name[FROZEN_BOOKMARK_PREFIX.len()..]
            .parse::<u64>()
            .with_context(|| format!("parse frozen bookmark `{name}` PR number"))?;
        if fields[1] == "conflicted" {
            bail!(
                "frozen bookmark `{name}` is conflicted; resolve the bookmark conflict before continuing"
            );
        }
        let commit_id = fields[2].trim();
        if commit_id.is_empty() {
            bail!("frozen bookmark `{name}` has no target commit");
        }
        bookmarks.push(FrozenBookmark {
            name: name.to_owned(),
            pr_number,
            commit_id: commit_id.to_owned(),
        });
    }
    bookmarks.sort_by(|left, right| left.pr_number.cmp(&right.pr_number));
    Ok(bookmarks)
}

#[tracing::instrument(skip_all)]
fn resolve_stack_resolution(
    runner: &impl CommandRunner,
    owned: Vec<ResolvedChange>,
    frozen_bookmarks: Vec<FrozenBookmark>,
) -> Result<StackResolution> {
    if frozen_bookmarks.is_empty() {
        return Ok(StackResolution {
            owned,
            frozen_dependencies: Vec::new(),
        });
    }

    let frozen_changes = resolve_frozen_changes(runner, frozen_bookmarks)?;
    let frozen_dependencies = frozen_dependencies_below_owned(&owned, frozen_changes)?;
    Ok(StackResolution {
        owned,
        frozen_dependencies,
    })
}

#[tracing::instrument(skip_all)]
fn resolve_purely_frozen_stack(
    runner: &impl CommandRunner,
    frozen_bookmarks: Vec<FrozenBookmark>,
) -> Result<StackResolution> {
    if frozen_bookmarks.is_empty() {
        bail!(
            "unsupported stack shape: empty owned stack and no frozen bookmarks in scope. Run `forklift get <pr>` first or move to a mutable stack."
        );
    }
    let at_commit =
        resolve_single_rev(runner, "@").context("resolve current revision for frozen sync")?;
    let frozen_changes = resolve_frozen_changes(runner, frozen_bookmarks)?;
    let Some(frozen_dependencies) = frozen_dependency_chain_ending_at(&frozen_changes, &at_commit)?
    else {
        bail!(
            "unsupported stack shape: empty owned stack and current revision {} is not a `forklift/frozen/pr-*` bookmark target. Run `forklift get <pr>` first or move to a mutable stack.",
            at_commit
        );
    };

    Ok(StackResolution {
        owned: Vec::new(),
        frozen_dependencies,
    })
}

#[tracing::instrument(skip_all)]
fn resolve_frozen_changes(
    runner: &impl CommandRunner,
    bookmarks: Vec<FrozenBookmark>,
) -> Result<Vec<FrozenDependency>> {
    let mut dependencies = Vec::new();
    for bookmark in bookmarks {
        let changes = resolve_stack(runner, &bookmark.commit_id)
            .with_context(|| format!("resolve frozen bookmark `{}`", bookmark.name))?;
        let [change] = changes.as_slice() else {
            bail!(
                "frozen bookmark `{}` resolved to {} changes; expected exactly one",
                bookmark.name,
                changes.len()
            );
        };
        if change.commit_id != bookmark.commit_id {
            bail!(
                "frozen bookmark `{}` points at {}, but jj resolved {}",
                bookmark.name,
                bookmark.commit_id,
                change.commit_id
            );
        }
        if change.conflict {
            bail!(
                "frozen bookmark `{}` points at conflicted change {} ({})",
                bookmark.name,
                change.change_id,
                change.commit_id
            );
        }
        dependencies.push(FrozenDependency {
            bookmark,
            change: change.clone(),
        });
    }
    Ok(dependencies)
}

#[tracing::instrument(skip_all, fields(top_commit = %top_commit))]
fn frozen_dependency_chain_ending_at(
    frozen: &[FrozenDependency],
    top_commit: &str,
) -> Result<Option<Vec<FrozenDependency>>> {
    let mut by_commit: HashMap<&str, &FrozenDependency> = HashMap::new();
    for dependency in frozen {
        if let Some(existing) = by_commit.insert(&dependency.change.commit_id, dependency) {
            bail!(
                "multiple frozen bookmarks point at commit {}: `{}` and `{}`",
                dependency.change.commit_id,
                existing.bookmark.name,
                dependency.bookmark.name
            );
        }
    }

    let Some(mut current) = by_commit.get(top_commit).copied() else {
        return Ok(None);
    };
    let mut seen = HashSet::new();
    let mut top_down = Vec::new();
    loop {
        if !seen.insert(current.change.commit_id.as_str()) {
            bail!(
                "unsupported frozen dependency graph: cycle at bookmark `{}`",
                current.bookmark.name
            );
        }
        top_down.push(current);
        let frozen_parents = current
            .change
            .parent_ids
            .iter()
            .filter_map(|parent_id| by_commit.get(parent_id.as_str()).copied())
            .collect::<Vec<_>>();
        match frozen_parents.as_slice() {
            [] => break,
            [parent] => current = *parent,
            parents => bail!(
                "unsupported frozen dependency graph: bookmark `{}` has {} frozen parents; expected a linear frozen chain",
                current.bookmark.name,
                parents.len()
            ),
        }
    }

    Ok(Some(top_down.into_iter().rev().cloned().collect()))
}

#[tracing::instrument(skip_all)]
fn frozen_dependencies_below_owned(
    owned: &[ResolvedChange],
    frozen: Vec<FrozenDependency>,
) -> Result<Vec<FrozenDependency>> {
    if owned.is_empty() || frozen.is_empty() {
        return Ok(Vec::new());
    }

    let mut by_commit: HashMap<&str, &FrozenDependency> = HashMap::new();
    for dependency in &frozen {
        if let Some(existing) = by_commit.insert(&dependency.change.commit_id, dependency) {
            bail!(
                "multiple frozen bookmarks point at commit {}: `{}` and `{}`",
                dependency.change.commit_id,
                existing.bookmark.name,
                dependency.bookmark.name
            );
        }
    }

    let selected_commits = owned
        .iter()
        .map(|change| change.commit_id.as_str())
        .collect::<HashSet<_>>();
    let roots = owned
        .iter()
        .filter(|change| selected_parent(change, &selected_commits).is_none())
        .collect::<Vec<_>>();
    let [root] = roots.as_slice() else {
        let root_labels = roots
            .iter()
            .map(|change| change_label(change))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "unsupported stack shape: multiple roots selected ({} roots): {root_labels}. Move to a single linear stack before running forklift.",
            roots.len(),
        );
    };

    let nearest = root
        .parent_ids
        .iter()
        .filter_map(|parent_id| by_commit.get(parent_id.as_str()).copied())
        .collect::<Vec<_>>();
    let [nearest] = nearest.as_slice() else {
        if nearest.is_empty() {
            return Ok(Vec::new());
        }
        let boundary_labels = nearest
            .iter()
            .map(|dependency| {
                format!(
                    "`{}` at {}",
                    dependency.bookmark.name,
                    change_label(&dependency.change)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "unsupported stack shape: multiple frozen boundaries below owned root {} ({} boundaries): {boundary_labels}. Run `forklift sync` from a single linear stack.",
            root.change_id,
            nearest.len()
        );
    };

    let mut seen = HashSet::new();
    let mut top_down = Vec::new();
    let mut current = *nearest;
    loop {
        if !seen.insert(current.change.commit_id.as_str()) {
            bail!(
                "unsupported frozen dependency graph: cycle at bookmark `{}`",
                current.bookmark.name
            );
        }
        top_down.push(current);

        let frozen_parents = current
            .change
            .parent_ids
            .iter()
            .filter_map(|parent_id| by_commit.get(parent_id.as_str()).copied())
            .collect::<Vec<_>>();
        match frozen_parents.as_slice() {
            [] => break,
            [parent] => current = *parent,
            parents => {
                let parent_labels = parents
                    .iter()
                    .map(|dependency| {
                        format!(
                            "`{}` at {}",
                            dependency.bookmark.name,
                            change_label(&dependency.change)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "unsupported frozen dependency graph: bookmark `{}` has {} frozen parents: {parent_labels}; expected a linear frozen chain",
                    current.bookmark.name,
                    parents.len()
                )
            }
        }
    }

    Ok(top_down.into_iter().rev().cloned().collect())
}

#[tracing::instrument(skip_all)]
fn validate_submit_bookmark_state(
    runner: &impl CommandRunner,
    config: &AppConfig,
    change: &ResolvedChange,
    entry: &PrCacheEntry,
) -> Result<()> {
    let local_target = jj_ref_commit_id(runner, &entry.head_branch).with_context(|| {
        format!(
            "local head bookmark `{}` is missing or conflicted",
            entry.head_branch
        )
    })?;
    if local_target != change.commit_id {
        bail!(
            "local head bookmark `{}` points at {}, but selected change {} is {}; refusing to submit the wrong revision",
            entry.head_branch,
            local_target,
            change.change_id,
            change.commit_id
        );
    }

    let remote = remote_bookmark_status(runner, config, &entry.head_branch)?;
    if !remote.tracked {
        bail!(
            "remote bookmark `{}@{}` is untracked; refusing to submit until jj tracks the PR branch",
            entry.head_branch,
            config.remote
        );
    }
    if remote.conflicted {
        bail!(
            "bookmark `{}` is conflicted with remote `{}`; resolve the jj bookmark conflict before submitting",
            entry.head_branch,
            config.remote
        );
    }

    Ok(())
}

#[tracing::instrument(level = "trace", skip_all, fields(rev = %rev))]
fn jj_ref_commit_id(runner: &impl CommandRunner, rev: &str) -> Result<String> {
    run_required(
        runner,
        "jj",
        &["log", "--no-graph", "-r", rev, "-T", "commit_id"],
    )
    .with_context(|| format!("resolve jj revision `{rev}`"))
}

#[tracing::instrument(skip_all, fields(branch = %branch))]
fn remote_bookmark_status(
    runner: &impl CommandRunner,
    config: &AppConfig,
    branch: &str,
) -> Result<RemoteBookmarkStatus> {
    let args = [
        "bookmark",
        "list",
        "--all-remotes",
        branch,
        "-T",
        BOOKMARK_STATUS_TEMPLATE,
    ];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let mut status = None;
    for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 3 || fields[0] != config.remote {
            continue;
        }
        status = Some(RemoteBookmarkStatus {
            tracked: fields[1] == "tracked",
            conflicted: fields[2] == "conflicted",
        });
        break;
    }

    status.with_context(|| {
        format!(
            "remote bookmark `{}@{}` is missing from jj; run `jj git fetch --remote {}` before submitting",
            branch, config.remote, config.remote
        )
    })
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
fn resolve_submit_head_branch(
    runner: &impl CommandRunner,
    config: &AppConfig,
    used_head_branches: &mut HashSet<String>,
    store: &CacheStore,
    context: &AppContext,
    change: &ResolvedChange,
    diagnostics: Diagnostics,
) -> Result<(String, Option<PrCacheEntry>, Option<String>)> {
    if let Some(discovered) = discover_existing_pr_from_local_bookmarks(
        runner,
        config,
        used_head_branches,
        &context.github,
        change,
    )? {
        return Ok(discovered);
    }

    if let Some(entry) = store.get_pr(&context.github.repo, &change.change_id) {
        let head_branch = entry.head_branch.clone();
        match resolve_submit_cached_head_branch(
            runner,
            config,
            used_head_branches,
            &context.github,
            change,
            entry,
        ) {
            Ok(resolved) => return Ok(resolved),
            Err(error) => {
                diagnostics.warn(format!(
                    "phase=plan-submit object=cache:{} error=ignored stale cache hint for `{}`: {error:#}",
                    change.change_id, head_branch
                ));
            }
        }
    }

    let head_branch = deterministic_head_branch(config, change, used_head_branches);
    let existing_pr =
        lookup_open_pr_by_head_branch(runner, &context.github, &change.change_id, &head_branch)?;
    let expected_remote_head = remote_head_oid(runner, &config.remote, &head_branch)?;

    if let Some(existing_pr) = existing_pr {
        validate_submit_bookmark_state(runner, config, change, &existing_pr)?;
        used_head_branches.insert(head_branch.clone());
        return Ok((head_branch, Some(existing_pr), expected_remote_head));
    }

    if let Some(remote_head) = &expected_remote_head {
        bail!(
            "remote branch `{}` already exists at {} but local forklift cache does not identify a safe matching PR; refusing to push over it. Run `forklift get` for the PR that owns it, delete the branch, or choose a different change title.",
            head_branch,
            remote_head
        );
    }

    used_head_branches.insert(head_branch.clone());
    Ok((head_branch, None, None))
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
fn resolve_submit_cached_head_branch(
    runner: &impl CommandRunner,
    config: &AppConfig,
    used_head_branches: &mut HashSet<String>,
    github: &GitHubContext,
    change: &ResolvedChange,
    entry: &PrCacheEntry,
) -> Result<(String, Option<PrCacheEntry>, Option<String>)> {
    let head_branch = entry.head_branch.clone();
    if used_head_branches.contains(&head_branch) {
        bail!("cache records duplicate head branch `{head_branch}` in stack");
    }
    validate_submit_bookmark_state(runner, config, change, entry)?;
    let existing_pr = lookup_cached_pr(runner, github, &change.change_id, &head_branch, entry)?;
    let expected_remote_head = remote_head_oid(runner, &config.remote, &head_branch)?;
    used_head_branches.insert(head_branch.clone());
    Ok((head_branch, Some(existing_pr), expected_remote_head))
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
fn discover_existing_pr_from_local_bookmarks(
    runner: &impl CommandRunner,
    config: &AppConfig,
    used_head_branches: &mut HashSet<String>,
    github: &GitHubContext,
    change: &ResolvedChange,
) -> Result<Option<(String, Option<PrCacheEntry>, Option<String>)>> {
    let mut matches = Vec::new();
    for head_branch in local_stack_bookmarks_for_change(runner, config, change)? {
        if used_head_branches.contains(&head_branch) {
            continue;
        }
        if let Some(existing_pr) =
            lookup_open_pr_by_head_branch(runner, github, &change.change_id, &head_branch)?
        {
            matches.push((head_branch, existing_pr));
        }
    }

    match matches.as_slice() {
        [] => Ok(None),
        [(head_branch, existing_pr)] => {
            validate_submit_bookmark_state(runner, config, change, existing_pr)?;
            let expected_remote_head = remote_head_oid(runner, &config.remote, head_branch)?;
            used_head_branches.insert(head_branch.clone());
            Ok(Some((
                head_branch.clone(),
                Some(existing_pr.clone()),
                expected_remote_head,
            )))
        }
        _ => bail!(
            "multiple local `{}` bookmarks at {} have open GitHub PRs for {}; refusing to choose: {}",
            config.branch_prefix,
            change.commit_id,
            change.change_id,
            matches
                .iter()
                .map(|(branch, pr)| format!("{} -> PR #{}", branch, pr.pr_number))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
fn local_stack_bookmarks_for_change(
    runner: &impl CommandRunner,
    config: &AppConfig,
    change: &ResolvedChange,
) -> Result<Vec<String>> {
    let args = [
        "bookmark",
        "list",
        "--revision",
        change.commit_id.as_str(),
        "-T",
        LOCAL_BOOKMARK_TEMPLATE,
    ];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let prefix = format!("{}/", config.branch_prefix.trim_end_matches('/'));
    let mut bookmarks = output
        .stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let remote = fields.next().unwrap_or_default().trim();
            if !remote.is_empty() || !name.starts_with(&prefix) {
                return None;
            }
            Some(name.to_owned())
        })
        .collect::<Vec<_>>();
    bookmarks.sort();
    bookmarks.dedup();
    Ok(bookmarks)
}

#[tracing::instrument(skip_all)]
fn submit_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    context: &AppContext,
    yes: bool,
    yes_command: &str,
    diagnostics: Diagnostics,
) -> Result<SubmitSummary> {
    diagnostics.phase("validate-submit-bases");
    validate_submit_bases(runner, config, &context.stack, &context.frozen_dependencies)
        .map_err(|error| phase_error("validate-submit-bases", "stack", error))?;
    validate_submit_descriptions(&context.stack)
        .map_err(|error| phase_error("validate-submit-bases", "stack", error))?;

    diagnostics.phase("plan-submit");
    let mut store = CacheStore::load_current_best_effort(runner, diagnostics, "plan-submit")
        .map_err(|error| phase_error("plan-submit", "cache", error))?;
    diagnostics.repo_details(&store);
    let frozen_entries = resolve_submit_frozen_dependency_entries(
        runner,
        &context.github,
        &context.frozen_dependencies,
        diagnostics,
    )
    .map_err(|error| phase_error("plan-submit", "frozen dependencies", error))?;
    let mut plans = Vec::new();
    let mut used_head_branches = HashSet::new();
    let mut previous_head_branch = frozen_entries
        .last()
        .map(|(_, entry)| entry.head_branch.clone());

    for change in &context.stack {
        let base_branch = previous_head_branch
            .clone()
            .unwrap_or_else(|| config.trunk.clone());

        let (head_branch, existing_pr, expected_remote_head) = resolve_submit_head_branch(
            runner,
            config,
            &mut used_head_branches,
            &store,
            context,
            change,
            diagnostics,
        )
        .map_err(|error| {
            phase_error("plan-submit", format!("change:{}", change.change_id), error)
        })?;
        // For an existing PR the live ref must be either our target commit or the
        // commit we last recorded; anything else means the branch advanced
        // out-of-band and force-pushing would clobber that work.
        if let Some(entry) = &existing_pr {
            let live = expected_remote_head.as_deref();
            if live != Some(change.commit_id.as_str()) && live != Some(entry.head_sha.as_str()) {
                bail!(
                    "phase=plan-submit object=head:{head_branch} error=remote branch is at {} but cache recorded {}; it advanced out-of-band, refusing to force-push. safe-next-command=`forklift submit --dry-run`",
                    live.unwrap_or("<absent>"),
                    entry.head_sha
                );
            }
        }
        let push_needed = expected_remote_head.as_deref() != Some(change.commit_id.as_str());
        let pr_update_needed = existing_pr
            .as_ref()
            .is_some_and(|entry| push_needed || pr_metadata_changed(entry, &base_branch, change));

        previous_head_branch = Some(head_branch.clone());
        plans.push(SubmitPlan {
            change: change.clone(),
            head_branch,
            base_branch,
            existing_pr,
            expected_remote_head,
            push_needed,
            pr_update_needed,
        });
    }

    let mut summary = SubmitSummary {
        pushed_refs: plans.iter().filter(|plan| plan.push_needed).count(),
        created_prs: plans
            .iter()
            .filter(|plan| plan.existing_pr.is_none())
            .count(),
        updated_prs: plans
            .iter()
            .filter(|plan| plan.existing_pr.is_some() && plan.pr_update_needed)
            .count(),
        unchanged_prs: plans
            .iter()
            .filter(|plan| plan.existing_pr.is_some() && !plan.pr_update_needed)
            .count(),
        ..SubmitSummary::default()
    };

    diagnostics.print_submit_plan(config, context, &plans);
    if diagnostics.dry_run {
        diagnostics.plan_line(
            "- live jj/GitHub discovery ran during planning; SQLite cache writes are skipped",
        );
        return Ok(summary);
    }
    print_submit_action_plan(config, &plans);
    confirm_submit_stack(yes, yes_command)?;

    diagnostics.phase("push-refs");
    push_changed_heads(runner, config, &plans, diagnostics)?;

    let mut entries = Vec::new();

    let pr_progress = diagnostics.progress_bar("Submitting", "pull requests", plans.len());
    for (index, plan) in plans.into_iter().enumerate() {
        let previous_comment_id = plan
            .existing_pr
            .as_ref()
            .and_then(|entry| entry.stack_comment_id.clone());
        let (action, entry) = match &plan.existing_pr {
            None => (
                SubmitPrAction::Submit,
                create_pr(runner, &context.github, &plan, diagnostics)?
                    .into_cache_entry(previous_comment_id),
            ),
            Some(existing) if plan.pr_update_needed => (
                SubmitPrAction::Update,
                update_pr(
                    runner,
                    &context.github,
                    existing.pr_number,
                    &plan,
                    diagnostics,
                )?
                .into_cache_entry(previous_comment_id),
            ),
            Some(existing) => (
                SubmitPrAction::Nothing,
                refreshed_cache_entry(existing, &plan, previous_comment_id),
            ),
        };
        diagnostics.submit_pr_action(&context.github.repo, &plan.change, action, &entry);
        save_submit_cache_entry(
            &mut store,
            &context.github.repo,
            &plan.change.change_id,
            entry.clone(),
            diagnostics,
        )?;
        entries.push((plan.change.clone(), entry));
        if let Some(progress) = &pr_progress {
            progress.set_position((index + 1) as u64);
        }
    }
    if let Some(progress) = pr_progress {
        ui_finish_progress_bar(progress);
    }

    diagnostics.phase("stack-comments");
    let comment_entries = entries
        .iter()
        .map(|(change, entry)| (change.change_id.clone(), entry.clone()))
        .collect::<Vec<_>>();
    let comment_progress = diagnostics.progress_bar("Updating", "stack comments", entries.len());
    for (index, (change, mut entry)) in entries.into_iter().enumerate() {
        let body = stack_comment_body_with_frozen(
            context,
            &frozen_entries,
            &comment_entries,
            &change.change_id,
            &config.trunk,
        );
        match upsert_stack_comment(
            runner,
            &context.github,
            entry.pr_number,
            &change.change_id,
            &body,
            diagnostics,
        )? {
            StackCommentAction::Created(comment_id) => {
                summary.created_comments += 1;
                entry.stack_comment_id = Some(comment_id);
            }
            StackCommentAction::Updated(comment_id, duplicate_count) => {
                summary.updated_comments += 1;
                summary.duplicate_comment_warnings += duplicate_count;
                entry.stack_comment_id = Some(comment_id);
            }
            StackCommentAction::Unchanged(comment_id) => {
                summary.unchanged_comments += 1;
                entry.stack_comment_id = Some(comment_id);
            }
        }

        save_submit_cache_entry(
            &mut store,
            &context.github.repo,
            &change.change_id,
            entry,
            diagnostics,
        )?;
        if let Some(progress) = &comment_progress {
            progress.set_position((index + 1) as u64);
        }
    }
    if let Some(progress) = comment_progress {
        ui_finish_progress_bar(progress);
    }

    Ok(summary)
}

#[tracing::instrument(skip_all)]
fn resolve_submit_frozen_dependency_entries(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    dependencies: &[FrozenDependency],
    diagnostics: Diagnostics,
) -> Result<Vec<(String, PrCacheEntry)>> {
    let mut entries = Vec::new();
    for dependency in dependencies {
        let pr = fetch_pr_by_number(
            runner,
            github,
            &dependency.change.change_id,
            dependency.bookmark.pr_number,
        )?;
        validate_submit_frozen_dependency_pr(github, dependency, &pr)?;
        diagnostics.warn(format!(
            "resolved frozen dependency `{}` to PR #{} head `{}`",
            dependency.bookmark.name, pr.number, pr.head_ref_name
        ));
        entries.push((
            dependency.change.change_id.clone(),
            pr.into_cache_entry(None),
        ));
    }
    Ok(entries)
}

#[tracing::instrument(skip_all, fields(pr = pr.number))]
fn validate_submit_frozen_dependency_pr(
    github: &GitHubContext,
    dependency: &FrozenDependency,
    pr: &GhPr,
) -> Result<()> {
    validate_get_pr_metadata(github, pr)?;
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            "frozen dependency `{}` points to PR #{}, but GitHub reports state {}; run `forklift sync` before submitting",
            dependency.bookmark.name,
            pr.number,
            pr.state
        );
    }
    let head_repo = get_pr_repo(pr, "head")?;
    let base_repo = get_pr_repo(pr, "base")?;
    if head_repo.name_with_owner != github.repo || base_repo.name_with_owner != github.repo {
        bail!(
            "frozen dependency `{}` PR #{} must be same-repo before submit; run `forklift sync` or unfreeze manually",
            dependency.bookmark.name,
            pr.number
        );
    }
    if pr.head_ref_oid != dependency.change.commit_id {
        bail!(
            "frozen dependency `{}` points at {}, but GitHub PR #{} head is {}; run `forklift sync` before submitting",
            dependency.bookmark.name,
            dependency.change.commit_id,
            pr.number,
            pr.head_ref_oid
        );
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(repo = %repo, change = %change_id))]
fn save_submit_cache_entry(
    store: &mut CacheStore,
    repo: &str,
    change_id: &str,
    entry: PrCacheEntry,
    diagnostics: Diagnostics,
) -> Result<()> {
    if store.get_pr(repo, change_id) == Some(&entry) {
        return Ok(());
    }

    store.upsert_pr(repo, change_id, entry);
    diagnostics.phase("save-cache");
    store.save_best_effort(diagnostics, "save-cache");
    Ok(())
}

#[tracing::instrument(level = "trace", skip_all)]
fn pr_metadata_changed(entry: &PrCacheEntry, base_branch: &str, change: &ResolvedChange) -> bool {
    entry.base_branch != base_branch
        || entry.base_ref != base_branch
        || entry.title != change.title
        || entry.body != change.body
}

#[tracing::instrument(skip_all, fields(change = %current_change_id))]
fn stack_comment_body_with_frozen(
    context: &AppContext,
    frozen_entries: &[(String, PrCacheEntry)],
    entries: &[(String, PrCacheEntry)],
    current_change_id: &str,
    trunk: &str,
) -> String {
    let mut body = format!(
        "{STACK_COMMENT_MARKER}\nStack for {}\n\n",
        context.github.repo
    );

    // The stack is rendered top-to-bottom: the head change first, the
    // trunk-adjacent change last, and `trunk` itself as the final entry to mark
    // the bottom. Frozen dependencies sit below the current stack (closer to
    // trunk), so they follow it. `context.stack`/`frozen_dependencies` are
    // stored bottom-to-top, hence the `.rev()`.
    let has_dependencies = !context.frozen_dependencies.is_empty();
    if has_dependencies {
        body.push_str("Current stack:\n");
    }

    for change in context.stack.iter().rev() {
        let Some((_, entry)) = entries
            .iter()
            .find(|(change_id, _)| change_id == &change.change_id)
        else {
            continue;
        };
        push_stack_comment_line(
            &mut body,
            &context.github.repo,
            &change.title,
            &change.change_id,
            entry,
            change.change_id == current_change_id,
        );
    }

    if has_dependencies {
        body.push_str("\nDependencies:\n");
        for dependency in context.frozen_dependencies.iter().rev() {
            let Some((_, entry)) = frozen_entries
                .iter()
                .find(|(change_id, _)| change_id == &dependency.change.change_id)
            else {
                continue;
            };
            push_stack_comment_line(
                &mut body,
                &context.github.repo,
                &dependency.change.title,
                &dependency.change.change_id,
                entry,
                false,
            );
        }
    }

    body.push_str(&format!("- {trunk}\n"));
    body.push_str("\n");
    if let Some((_, entry)) = entries
        .iter()
        .find(|(change_id, _)| change_id == current_change_id)
    {
        body.push_str(&format!(
            "Check out this stack: `forklift get {}`\n",
            entry.pr_number
        ));
    }
    body.push_str("Pull/update this stack: `forklift sync`\n");
    body.push_str("Publish local edits: `forklift submit`\n");
    body.push_str("Merge when ready: `forklift merge`\n");

    body
}

#[tracing::instrument(skip_all, fields(current_pr = current_pr_number))]
fn repaired_stack_comment_body(
    github: &GitHubContext,
    prs: &[GhPr],
    current_pr_number: u64,
    trunk: &str,
) -> String {
    let mut body = format!("{STACK_COMMENT_MARKER}\nStack for {}\n\n", github.repo);

    for pr in prs.iter().rev() {
        push_repaired_stack_comment_line(
            &mut body,
            &github.repo,
            pr,
            pr.number == current_pr_number,
        );
    }

    body.push_str(&format!("- {trunk}\n"));
    body.push('\n');
    if prs.iter().any(|pr| pr.number == current_pr_number) {
        body.push_str(&format!(
            "Check out this stack: `forklift get {}`\n",
            current_pr_number
        ));
    }
    body.push_str("Pull/update this stack: `forklift sync`\n");
    body.push_str("Publish local edits: `forklift submit`\n");
    body.push_str("Merge when ready: `forklift merge`\n");

    body
}

#[tracing::instrument(level = "trace", skip_all, fields(pr = pr.number))]
fn push_repaired_stack_comment_line(body: &mut String, repo: &str, pr: &GhPr, is_current: bool) {
    let label = format!(
        "[{} #{}]({})",
        markdown_link_label(&pr.title),
        pr.number,
        github_pr_url(repo, pr.number),
    );
    let label = if is_current {
        format!("**{label}**")
    } else {
        label
    };
    let current_marker = if is_current { " 👈" } else { "" };
    let created_date = created_date_fragment(&pr.created_at);
    body.push_str(&format!(
        "- {} _{}_{}{}\n",
        label,
        stack_comment_change_hint(&pr.head_ref_name),
        created_date,
        current_marker
    ));
}

#[tracing::instrument(level = "trace", skip_all)]
fn stack_comment_change_hint(head_branch: &str) -> String {
    let last = head_branch.rsplit('/').next().unwrap_or(head_branch);
    let mut parts = last.rsplit('-');
    let suffix = parts.next().unwrap_or(last);
    if suffix.chars().all(|ch| ch.is_ascii_digit()) {
        parts.next().unwrap_or(suffix).to_owned()
    } else {
        suffix.to_owned()
    }
}

#[tracing::instrument(level = "trace", skip_all, fields(change = %change_id))]
fn push_stack_comment_line(
    body: &mut String,
    repo: &str,
    title: &str,
    change_id: &str,
    entry: &PrCacheEntry,
    is_current: bool,
) {
    let label = format!(
        "[{} #{}]({})",
        markdown_link_label(title),
        entry.pr_number,
        github_pr_url(repo, entry.pr_number),
    );
    let label = if is_current {
        format!("**{label}**")
    } else {
        label
    };
    let current_marker = if is_current { " 👈" } else { "" };
    let created_date = created_date_fragment(&entry.created_at);
    body.push_str(&format!(
        "- {} _{}_{}{}\n",
        label,
        change_id_branch_prefix(change_id),
        created_date,
        current_marker
    ));
}

#[tracing::instrument(level = "trace", skip_all)]
fn created_date_fragment(created_at: &str) -> String {
    // Render `YYYY-MM-DDTHH:MM:SSZ` as `YYYY-MM-DD HH:MM:SS`, falling back to
    // whatever prefix is available.
    let stamp = created_at.get(..19).unwrap_or(created_at).trim();
    let stamp = stamp.replacen('T', " ", 1);
    if stamp.is_empty() {
        String::new()
    } else {
        format!(" · {stamp}")
    }
}

#[tracing::instrument(level = "trace", skip_all, fields(repo = %repo, pr = pr_number))]
fn github_pr_url(repo: &str, pr_number: u64) -> String {
    format!("https://github.com/{repo}/pull/{pr_number}")
}

#[tracing::instrument(level = "trace", skip_all)]
fn markdown_link_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\\' | '[' | ']' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' | '\r' | '\t' => out.push(' '),
            other if other.is_control() => {}
            other => out.push(other),
        }
    }
    out
}

#[tracing::instrument(level = "trace", skip_all)]
fn refreshed_cache_entry(
    existing: &PrCacheEntry,
    plan: &SubmitPlan,
    stack_comment_id: Option<String>,
) -> PrCacheEntry {
    PrCacheEntry {
        pr_number: existing.pr_number,
        pr_node_id: existing.pr_node_id.clone(),
        head_branch: plan.head_branch.clone(),
        base_branch: plan.base_branch.clone(),
        base_ref: plan.base_branch.clone(),
        head_repo_id: existing.head_repo_id.clone(),
        head_repo_node_id: existing.head_repo_node_id.clone(),
        head_repo_name: existing.head_repo_name.clone(),
        base_repo_id: existing.base_repo_id.clone(),
        base_repo_node_id: existing.base_repo_node_id.clone(),
        base_repo_name: existing.base_repo_name.clone(),
        head_sha: plan.change.commit_id.clone(),
        base_sha: existing.base_sha.clone(),
        author_login: existing.author_login.clone(),
        title: plan.change.title.clone(),
        body: plan.change.body.clone(),
        created_at: existing.created_at.clone(),
        stack_comment_id,
    }
}

#[tracing::instrument(skip_all)]
fn validate_submit_bases(
    runner: &impl CommandRunner,
    config: &AppConfig,
    stack: &[ResolvedChange],
    frozen_dependencies: &[FrozenDependency],
) -> Result<()> {
    let Some(root) = stack.first() else {
        bail!("cannot submit an empty stack");
    };
    let root_parent = root.parent_ids.first().with_context(|| {
        format!(
            "change {} ({}) has no parent to compare against trunk `{}`",
            root.change_id, root.commit_id, config.trunk
        )
    })?;
    let (base_label, merge_base_target, expected_parent) = frozen_dependencies
        .last()
        .map(|dependency| {
            (
                dependency.bookmark.name.as_str(),
                dependency.change.commit_id.as_str(),
                dependency.change.commit_id.as_str(),
            )
        })
        .unwrap_or((
            config.trunk.as_str(),
            config.trunk.as_str(),
            root_parent.as_str(),
        ));
    if root_parent != expected_parent {
        bail!(
            "submit base validation failed for {} ({}): jj parent is {}, expected base {} at {}",
            root.change_id,
            root.commit_id,
            root_parent,
            base_label,
            expected_parent
        );
    }
    let root_merge_base = merge_base(runner, &root.commit_id, merge_base_target)?;
    if root_merge_base != expected_parent {
        bail!(
            "submit base validation failed for {} ({}): merge-base with base `{}` ({}) is {}, but expected {}",
            root.change_id,
            root.commit_id,
            base_label,
            merge_base_target,
            root_merge_base,
            expected_parent
        );
    }

    for pair in stack.windows(2) {
        let parent = &pair[0];
        let child = &pair[1];
        if selected_parent(child, &HashSet::from([parent.commit_id.as_str()]))
            != Some(parent.commit_id.as_str())
        {
            bail!(
                "submit parent validation failed for {} ({}): expected parent commit {} from previous change {}",
                child.change_id,
                child.commit_id,
                parent.commit_id,
                parent.change_id
            );
        }

        let merge_base = merge_base(runner, &child.commit_id, &parent.commit_id)?;
        if merge_base != parent.commit_id {
            bail!(
                "submit base validation failed for {} ({}): merge-base with parent change {} ({}) is {}",
                child.change_id,
                child.commit_id,
                parent.change_id,
                parent.commit_id,
                merge_base
            );
        }
    }

    Ok(())
}

/// Rejects a stack up front if any change has no description. jj refuses to push
/// an undescribed commit, but it only errors at push time — partway through the
/// push loop, after earlier branches are already on the remote. Catching it here
/// (before `push-refs`) fails cleanly with zero side effects.
#[tracing::instrument(skip_all)]
fn validate_submit_descriptions(stack: &[ResolvedChange]) -> Result<()> {
    let undescribed = stack
        .iter()
        .filter(|change| change.title.trim().is_empty())
        .collect::<Vec<_>>();
    if undescribed.is_empty() {
        return Ok(());
    }

    let list = undescribed
        .iter()
        .map(|change| format!("{} ({})", change.change_id, change.commit_id))
        .collect::<Vec<_>>()
        .join(", ");
    let revs = undescribed
        .iter()
        .map(|change| change.change_id.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    bail!(
        "cannot submit: {} change(s) have no description and jj refuses to push undescribed commits: {list}. Describe them first, e.g. `jj describe -r {revs}`.",
        undescribed.len()
    );
}

#[tracing::instrument(level = "trace", skip_all, fields(left = %left, right = %right))]
fn merge_base(runner: &impl CommandRunner, left: &str, right: &str) -> Result<String> {
    git_run_required(runner, &["merge-base", left, right])
        .with_context(|| format!("validate merge base between {left} and {right}"))
}

#[tracing::instrument(skip_all, fields(remote = %remote, branch = %branch))]
fn remote_head_oid(
    runner: &impl CommandRunner,
    remote: &str,
    branch: &str,
) -> Result<Option<String>> {
    let args = ["ls-remote", "--heads", remote, branch];
    let output = git_run(runner, &args)?;
    if !output.success {
        bail!(
            "`{}` failed while resolving remote head `{}`: {}",
            display_command("git", &args),
            branch,
            output.stderr.trim()
        );
    }

    let lines = output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    match lines.as_slice() {
        [] => Ok(None),
        [line] => {
            let oid = line
                .split_whitespace()
                .next()
                .filter(|oid| !oid.is_empty())
                .with_context(|| format!("parse remote head `{branch}` from ls-remote output"))?;
            Ok(Some(oid.to_owned()))
        }
        _ => bail!(
            "remote head lookup for `{branch}` returned {} refs; refusing to push ambiguously",
            lines.len()
        ),
    }
}

#[tracing::instrument(skip_all)]
fn push_changed_heads(
    runner: &impl CommandRunner,
    config: &AppConfig,
    plans: &[SubmitPlan],
    diagnostics: Diagnostics,
) -> Result<()> {
    let changed = plans
        .iter()
        .filter(|plan| plan.push_needed)
        .collect::<Vec<_>>();
    if changed.is_empty() {
        return Ok(());
    }

    let progress = diagnostics.progress_bar("Pushing", "bookmarks", changed.len());
    for (index, plan) in changed.iter().enumerate() {
        set_submit_bookmark(runner, plan, diagnostics)?;
        push_submit_bookmark(runner, config, plan, diagnostics)?;
        if let Some(progress) = &progress {
            progress.set_position((index + 1) as u64);
        }
    }
    if let Some(progress) = progress {
        ui_finish_progress_bar(progress);
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn set_submit_bookmark(
    runner: &impl CommandRunner,
    plan: &SubmitPlan,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = [
        "bookmark",
        "set",
        "--allow-backwards",
        plan.head_branch.as_str(),
        "-r",
        plan.change.commit_id.as_str(),
    ];
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "phase=push-refs object=bookmark:{} failed-command=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            plan.head_branch,
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn push_submit_bookmark(
    runner: &impl CommandRunner,
    config: &AppConfig,
    plan: &SubmitPlan,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = [
        "git",
        "push",
        "--remote",
        config.remote.as_str(),
        "--bookmark",
        plan.head_branch.as_str(),
    ];
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "phase=push-refs object=bookmark:{} failed-command=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            plan.head_branch,
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn create_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    plan: &SubmitPlan,
    diagnostics: Diagnostics,
) -> Result<GhPr> {
    let title = format!("title={}", plan.change.title);
    let head = format!("head={}", plan.head_branch);
    let base = format!("base={}", plan.base_branch);
    let body = format!("body={}", plan.change.body);
    let endpoint = format!("repos/{}/pulls", github.repo);
    let args = [
        "api",
        "-X",
        "POST",
        endpoint.as_str(),
        "-f",
        title.as_str(),
        "-f",
        head.as_str(),
        "-f",
        base.as_str(),
        "-f",
        body.as_str(),
        "--jq",
        PR_API_JQ,
    ];

    run_pr_api(runner, &args, "create", &plan.change.change_id, diagnostics)
}

#[tracing::instrument(skip_all, fields(pr = pr_number))]
fn update_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    plan: &SubmitPlan,
    diagnostics: Diagnostics,
) -> Result<GhPr> {
    let title = format!("title={}", plan.change.title);
    let base = format!("base={}", plan.base_branch);
    let body = format!("body={}", plan.change.body);
    let endpoint = format!("repos/{}/pulls/{}", github.repo, pr_number);
    let args = [
        "api",
        "-X",
        "PATCH",
        endpoint.as_str(),
        "-f",
        title.as_str(),
        "-f",
        base.as_str(),
        "-f",
        body.as_str(),
        "--jq",
        PR_API_JQ,
    ];

    run_pr_api(runner, &args, "update", &plan.change.change_id, diagnostics)
}

#[tracing::instrument(skip_all, fields(action = %action, change = %change_id))]
fn run_pr_api(
    runner: &impl CommandRunner,
    args: &[&str],
    action: &str,
    change_id: &str,
    diagnostics: Diagnostics,
) -> Result<GhPr> {
    diagnostics.command("gh", args);
    let output = gh_run(runner, args)?;
    if !output.success {
        bail!(
            "phase=github-pr-{action} object=change:{change_id} failed-api=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            display_command("gh", args),
            output.stderr.trim()
        );
    }

    serde_json::from_str(&output.stdout)
        .with_context(|| format!("parse GitHub PR response while trying to {action} {change_id}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StackCommentAction {
    Created(String),
    Updated(String, usize),
    Unchanged(String),
}

#[derive(Debug, Clone, Deserialize)]
struct GhStackComment {
    id: u64,
    body: String,
    #[serde(rename = "userLogin")]
    user_login: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GhCommentId {
    id: u64,
}

#[tracing::instrument(skip_all, fields(pr = pr_number, change = %change_id))]
fn upsert_stack_comment(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    change_id: &str,
    body: &str,
    diagnostics: Diagnostics,
) -> Result<StackCommentAction> {
    let mut matches = list_stack_comments(runner, github, pr_number, change_id)?
        .into_iter()
        .filter(|comment| {
            comment.user_login == github.username && comment.body.contains(STACK_COMMENT_MARKER)
        })
        .collect::<Vec<_>>();

    if matches.is_empty() {
        return create_stack_comment(runner, github, pr_number, change_id, body, diagnostics)
            .map(StackCommentAction::Created);
    }

    matches.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let newest = matches.last().cloned().with_context(|| {
        format!("phase=stack-comments object=PR #{pr_number} select newest stack comment")
    })?;
    let duplicate_count = matches.len().saturating_sub(1);

    if duplicate_count > 0 {
        ui_warn!(
            "phase=stack-comments object=PR #{} found {} duplicate stack comments; updating newest comment {}",
            pr_number,
            duplicate_count,
            newest.id
        );
    }

    if duplicate_count == 0 && newest.body == body {
        return Ok(StackCommentAction::Unchanged(newest.id.to_string()));
    }

    update_stack_comment(
        runner,
        github,
        newest.id,
        pr_number,
        change_id,
        body,
        diagnostics,
    )
    .map(|comment_id| StackCommentAction::Updated(comment_id, duplicate_count))
}

#[tracing::instrument(skip_all, fields(pr = pr_number, change = %change_id))]
fn list_stack_comments(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    change_id: &str,
) -> Result<Vec<GhStackComment>> {
    let endpoint = format!("repos/{}/issues/{}/comments", github.repo, pr_number);
    let args = [
        "api",
        "--paginate",
        endpoint.as_str(),
        "--jq",
        STACK_COMMENT_JQ,
    ];
    let output = gh_run(runner, &args)?;
    if !output.success {
        bail!(
            "phase=stack-comments object=PR #{} change:{} failed-api=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            pr_number,
            change_id,
            display_command("gh", &args),
            output.stderr.trim()
        );
    }

    output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| parse_stack_comment_line(line, pr_number, change_id))
        .collect()
}

/// Best-effort lookup of an existing stack comment id, used when rebuilding cache
/// from GitHub during recovery. Returns `None` (rather than failing recovery) if
/// the scan errors or finds no matching comment; a later submit re-scans and will
/// create or update the comment as needed. Backfilling the id here keeps the
/// rebuilt cache complete instead of relying solely on the scan to avoid a
/// duplicate comment.
#[tracing::instrument(skip_all, fields(pr = pr_number, change = %change_id))]
fn find_stack_comment_id(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    change_id: &str,
) -> Option<String> {
    let mut matches = list_stack_comments(runner, github, pr_number, change_id)
        .ok()?
        .into_iter()
        .filter(|comment| {
            comment.user_login == github.username && comment.body.contains(STACK_COMMENT_MARKER)
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    matches.last().map(|comment| comment.id.to_string())
}

#[tracing::instrument(level = "trace", skip_all, fields(pr = pr_number, change = %change_id))]
fn parse_stack_comment_line(line: &str, pr_number: u64, change_id: &str) -> Result<GhStackComment> {
    serde_json::from_str::<GhStackComment>(line)
        .or_else(|_| {
            let encoded = serde_json::from_str::<String>(line)?;
            serde_json::from_str::<GhStackComment>(&encoded)
        })
        .with_context(|| {
            format!(
                "phase=stack-comments object=PR #{} change:{} parse stack comment lookup",
                pr_number, change_id
            )
        })
}

#[tracing::instrument(skip_all, fields(pr = pr_number, change = %change_id))]
fn create_stack_comment(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    change_id: &str,
    body: &str,
    diagnostics: Diagnostics,
) -> Result<String> {
    let endpoint = format!("repos/{}/issues/{}/comments", github.repo, pr_number);
    let body_field = format!("body={body}");
    let args = [
        "api",
        "-X",
        "POST",
        endpoint.as_str(),
        "-f",
        body_field.as_str(),
        "--jq",
        "{id}",
    ];

    run_comment_mutation(runner, &args, "create", pr_number, change_id, diagnostics)
}

#[tracing::instrument(skip_all, fields(comment = comment_id, pr = pr_number, change = %change_id))]
fn update_stack_comment(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    comment_id: u64,
    pr_number: u64,
    change_id: &str,
    body: &str,
    diagnostics: Diagnostics,
) -> Result<String> {
    let endpoint = format!("repos/{}/issues/comments/{}", github.repo, comment_id);
    let body_field = format!("body={body}");
    let args = [
        "api",
        "-X",
        "PATCH",
        endpoint.as_str(),
        "-f",
        body_field.as_str(),
        "--jq",
        "{id}",
    ];

    run_comment_mutation(runner, &args, "update", pr_number, change_id, diagnostics)
}

#[tracing::instrument(skip_all, fields(action = %action, pr = pr_number, change = %change_id))]
fn run_comment_mutation(
    runner: &impl CommandRunner,
    args: &[&str],
    action: &str,
    pr_number: u64,
    change_id: &str,
    diagnostics: Diagnostics,
) -> Result<String> {
    diagnostics.command("gh", args);
    let output = gh_run(runner, args)?;
    if !output.success {
        bail!(
            "phase=stack-comments object=PR #{} change:{} failed-api=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            pr_number,
            change_id,
            display_command("gh", args),
            output.stderr.trim()
        );
    }

    let id = serde_json::from_str::<GhCommentId>(&output.stdout)
        .with_context(|| {
            format!(
                "phase=stack-comments object=PR #{} change:{} parse {action} stack comment response",
                pr_number, change_id
            )
        })?
        .id;
    Ok(id.to_string())
}

#[tracing::instrument(skip_all, fields(change = %change_id, head_branch = %head_branch))]
fn lookup_cached_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    change_id: &str,
    head_branch: &str,
    entry: &PrCacheEntry,
) -> Result<PrCacheEntry> {
    if entry.head_branch != head_branch {
        bail!(
            "conflicting cached PR for {}/{}: cache records head branch `{}` but deterministic head branch is `{}`; refusing to create a duplicate PR",
            github.repo,
            change_id,
            entry.head_branch,
            head_branch
        );
    }

    let pr = fetch_pr_by_number(runner, github, change_id, entry.pr_number)?;
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            "closed cached PR for {}/{}: cache points to {} PR #{} on `{}`; refusing to create a duplicate PR",
            github.repo,
            change_id,
            pr.state,
            entry.pr_number,
            head_branch
        );
    }

    if pr.head_ref_name != entry.head_branch {
        bail!(
            "conflicting cached PR for {}/{}: cache points to PR #{} but GitHub reports head branch `{}` instead of `{}`",
            github.repo,
            change_id,
            entry.pr_number,
            pr.head_ref_name,
            entry.head_branch
        );
    }
    validate_cached_pr_metadata(github, change_id, entry, &pr)?;

    Ok(pr.into_cache_entry(entry.stack_comment_id.clone()))
}

#[tracing::instrument(skip_all, fields(change = %change_id, pr = pr.number))]
fn validate_cached_pr_metadata(
    github: &GitHubContext,
    change_id: &str,
    entry: &PrCacheEntry,
    pr: &GhPr,
) -> Result<()> {
    let live = pr.clone().into_cache_entry(entry.stack_comment_id.clone());
    let required = [
        ("pr_node_id", entry.pr_node_id.as_str()),
        ("head_repo_id", entry.head_repo_id.as_str()),
        ("head_repo_node_id", entry.head_repo_node_id.as_str()),
        ("head_repo_name", entry.head_repo_name.as_str()),
        ("base_repo_id", entry.base_repo_id.as_str()),
        ("base_repo_node_id", entry.base_repo_node_id.as_str()),
        ("base_repo_name", entry.base_repo_name.as_str()),
        ("author_login", entry.author_login.as_str()),
    ];
    if let Some((field, _)) = required.iter().find(|(_, value)| value.trim().is_empty()) {
        bail!(
            "unsupported PR state for {}/{}: missing required metadata field `{}`; run `forklift get {}` or recreate the PR branch with forklift",
            github.repo,
            change_id,
            field,
            github_pr_url(&github.repo, entry.pr_number)
        );
    }

    for (field, stored, live_value) in [
        (
            "pr_node_id",
            entry.pr_node_id.as_str(),
            live.pr_node_id.as_str(),
        ),
        (
            "head_repo_id",
            entry.head_repo_id.as_str(),
            live.head_repo_id.as_str(),
        ),
        (
            "head_repo_node_id",
            entry.head_repo_node_id.as_str(),
            live.head_repo_node_id.as_str(),
        ),
        (
            "head_repo_name",
            entry.head_repo_name.as_str(),
            live.head_repo_name.as_str(),
        ),
        (
            "base_repo_id",
            entry.base_repo_id.as_str(),
            live.base_repo_id.as_str(),
        ),
        (
            "base_repo_node_id",
            entry.base_repo_node_id.as_str(),
            live.base_repo_node_id.as_str(),
        ),
        (
            "base_repo_name",
            entry.base_repo_name.as_str(),
            live.base_repo_name.as_str(),
        ),
        (
            "author_login",
            entry.author_login.as_str(),
            live.author_login.as_str(),
        ),
        ("head_sha", entry.head_sha.as_str(), live.head_sha.as_str()),
        ("base_sha", entry.base_sha.as_str(), live.base_sha.as_str()),
        (
            "base_branch",
            entry.base_branch.as_str(),
            live.base_branch.as_str(),
        ),
    ] {
        if stored != live_value {
            bail!(
                "GitHub PR metadata mismatch for {}/{} PR #{} field `{}`: cache has `{}`, GitHub has `{}`; refusing to update remote branch",
                github.repo,
                change_id,
                entry.pr_number,
                field,
                stored,
                live_value
            );
        }
    }

    Ok(())
}

#[tracing::instrument(skip_all, fields(change = %change_id, pr = pr_number))]
fn fetch_pr_by_number(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    change_id: &str,
    pr_number: u64,
) -> Result<GhPr> {
    let pr_number_string = pr_number.to_string();
    let endpoint = format!("repos/{}/pulls/{}", github.repo, pr_number);
    let args = ["api", endpoint.as_str(), "--jq", PR_API_JQ];
    let output = gh_run(runner, &args)?;
    if !output.success {
        bail!(
            "missing cached PR for {}/{}: cache points to PR #{} but gh could not load it: {}; refusing to create a duplicate PR",
            github.repo,
            change_id,
            pr_number_string,
            output.stderr.trim()
        );
    }

    serde_json::from_str(&output.stdout)
        .with_context(|| format!("parse GitHub PR #{} metadata", pr_number_string))
}

#[tracing::instrument(skip_all, fields(change = %change_id, head_branch = %head_branch))]
fn lookup_open_pr_by_head_branch(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    change_id: &str,
    head_branch: &str,
) -> Result<Option<PrCacheEntry>> {
    let args = [
        "pr",
        "list",
        "--repo",
        github.repo.as_str(),
        "--head",
        head_branch,
        "--state",
        "open",
        "--json",
        PR_JSON_FIELDS,
    ];
    let output = gh_run(runner, &args)?;
    if !output.success {
        bail!(
            "`{}` failed while looking up open PR for {}/{}: {}",
            display_command("gh", &args),
            github.repo,
            change_id,
            output.stderr.trim()
        );
    }

    let prs = serde_json::from_str::<Vec<GhPr>>(&output.stdout)
        .with_context(|| format!("parse open PR lookup for branch `{head_branch}`"))?;
    match prs.as_slice() {
        [] => Ok(None),
        [pr] => {
            if !pr.state.eq_ignore_ascii_case("OPEN") {
                bail!(
                    "conflicting PR lookup for {}/{}: branch `{}` returned {} PR #{} despite open-state lookup",
                    github.repo,
                    change_id,
                    head_branch,
                    pr.state,
                    pr.number
                );
            }
            let comment_id = find_stack_comment_id(runner, github, pr.number, change_id);
            Ok(Some(pr.clone().into_cache_entry(comment_id)))
        }
        _ => bail!(
            "conflicting PR lookup for {}/{}: branch `{}` matched {} open PRs; refusing to choose one",
            github.repo,
            change_id,
            head_branch,
            prs.len()
        ),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GhPr {
    number: u64,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    id: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: String,
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "null_to_default_string")]
    body: String,
    #[serde(default, rename = "createdAt")]
    created_at: String,
    #[serde(default, rename = "headRepository")]
    head_repository: Option<GhRepository>,
    #[serde(default, rename = "baseRepository")]
    base_repository: Option<GhRepository>,
    #[serde(default)]
    author: Option<GhAuthor>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GhRepository {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "node_id")]
    node_id: String,
    #[serde(default, rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GhAuthor {
    #[serde(default)]
    login: String,
}

impl GhPr {
    #[tracing::instrument(level = "trace", skip_all)]
    fn into_cache_entry(self, stack_comment_id: Option<String>) -> PrCacheEntry {
        let head_repository = self.head_repository.unwrap_or_default();
        let base_repository = self.base_repository.unwrap_or_default();
        let author = self.author.unwrap_or_default();
        PrCacheEntry {
            pr_number: self.number,
            pr_node_id: self.id,
            head_branch: self.head_ref_name,
            base_branch: self.base_ref_name.clone(),
            base_ref: self.base_ref_name,
            head_repo_id: head_repository.id,
            head_repo_node_id: head_repository.node_id,
            head_repo_name: head_repository.name_with_owner,
            base_repo_id: base_repository.id,
            base_repo_node_id: base_repository.node_id,
            base_repo_name: base_repository.name_with_owner,
            head_sha: self.head_ref_oid,
            base_sha: self.base_ref_oid,
            author_login: author.login,
            title: self.title,
            body: self.body,
            created_at: self.created_at,
            stack_comment_id,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GhMergePr {
    number: u64,
    state: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: String,
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "null_to_default_string")]
    body: String,
    #[serde(default, rename = "createdAt")]
    created_at: String,
    #[serde(default, rename = "isDraft")]
    is_draft: bool,
    #[serde(
        default,
        rename = "reviewDecision",
        deserialize_with = "empty_string_to_none"
    )]
    review_decision: Option<String>,
    #[serde(default)]
    mergeable: Option<String>,
    #[serde(default, rename = "mergeStateStatus")]
    merge_state_status: Option<String>,
    #[serde(default, rename = "statusCheckRollup")]
    status_check_rollup: Vec<serde_json::Value>,
    #[serde(default, rename = "autoMergeRequest")]
    auto_merge_request: Option<serde_json::Value>,
}

#[tracing::instrument(skip_all)]
fn null_to_default_string<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

impl GhMergePr {
    #[tracing::instrument(level = "trace", skip_all)]
    fn into_cache_entry(self, stack_comment_id: Option<String>) -> PrCacheEntry {
        PrCacheEntry {
            pr_number: self.number,
            pr_node_id: String::new(),
            head_branch: self.head_ref_name,
            base_branch: self.base_ref_name.clone(),
            base_ref: self.base_ref_name,
            head_repo_id: String::new(),
            head_repo_node_id: String::new(),
            head_repo_name: String::new(),
            base_repo_id: String::new(),
            base_repo_node_id: String::new(),
            base_repo_name: String::new(),
            head_sha: self.head_ref_oid,
            base_sha: self.base_ref_oid,
            author_login: String::new(),
            title: self.title,
            body: self.body,
            created_at: self.created_at,
            stack_comment_id,
        }
    }
}

#[tracing::instrument(skip_all)]
fn parse_stack_log(runner: &impl CommandRunner, stdout: &str) -> Result<Vec<ResolvedChange>> {
    stdout
        .split(STACK_RECORD_SEPARATOR)
        .filter(|record| !record.is_empty())
        .map(|record| parse_stack_record(runner, record))
        .collect()
}

#[tracing::instrument(level = "trace", skip_all)]
fn parse_stack_record(runner: &impl CommandRunner, record: &str) -> Result<ResolvedChange> {
    let fields = record.split(STACK_FIELD_SEPARATOR).collect::<Vec<_>>();
    if fields.len() != 7 {
        bail!("expected 7 stack fields, got {}", fields.len());
    }

    let change_id = parse_json_string(fields[0], "change id")?;
    let commit_id = parse_json_string(fields[1], "commit id")?;
    let parent_ids = serde_json::from_str::<Vec<String>>(fields[2]).context("parse parent ids")?;
    let title = parse_json_string(fields[3], "title")?;
    let description = parse_json_string(fields[4], "description")?;
    let empty = serde_json::from_str::<bool>(fields[5]).context("parse empty status")?;
    let conflict = serde_json::from_str::<bool>(fields[6]).context("parse conflict status")?;
    let tree_id = resolve_tree_id(runner, &commit_id)?;

    Ok(ResolvedChange {
        change_id,
        commit_id,
        parent_ids,
        body: description_body(&description, &title),
        title,
        tree_id,
        empty,
        conflict,
    })
}

#[tracing::instrument(level = "trace", skip_all, fields(field = %field))]
fn parse_json_string(value: &str, field: &str) -> Result<String> {
    serde_json::from_str(value).with_context(|| format!("parse {field}"))
}

#[tracing::instrument(level = "trace", skip_all)]
fn description_body(description: &str, title: &str) -> String {
    let mut body = description.strip_prefix(title).unwrap_or(description);
    for _ in 0..2 {
        body = body.strip_prefix('\n').unwrap_or(body);
    }
    body.trim_end_matches('\n').to_owned()
}

#[tracing::instrument(level = "trace", skip_all, fields(commit = %commit_id))]
fn resolve_tree_id(runner: &impl CommandRunner, commit_id: &str) -> Result<String> {
    git_run_required(runner, &["show", "-s", "--format=%T", commit_id])
        .with_context(|| format!("resolve tree id for commit {commit_id}"))
}

#[tracing::instrument(level = "trace", skip_all)]
fn change_label(change: &ResolvedChange) -> String {
    format!("{} ({})", change.change_id, change.commit_id)
}

#[tracing::instrument(skip_all, fields(revset = %revset))]
fn validate_stack_shape(stack: &[ResolvedChange], revset: &str) -> Result<()> {
    if stack.is_empty() {
        bail!(
            "unsupported stack shape: empty stack selected by `{revset}`. Move to a non-empty owned stack before running forklift."
        );
    }

    if let Some(change) = stack.iter().find(|change| change.empty) {
        bail!(
            "unsupported stack shape: empty change {} ({}) selected. Amend or abandon the empty change before running forklift.",
            change.change_id,
            change.commit_id
        );
    }

    if let Some(change) = stack.iter().find(|change| change.conflict) {
        bail!(
            "unsupported stack shape: conflicted change {} ({}) selected. Resolve conflicts before running forklift.",
            change.change_id,
            change.commit_id
        );
    }

    if let Some(change) = stack.iter().find(|change| change.parent_ids.len() > 1) {
        bail!(
            "unsupported stack shape: merge commit {} ({}) has {} parents. Forklift requires a linear owned stack.",
            change.change_id,
            change.commit_id,
            change.parent_ids.len()
        );
    }

    let selected_commits = stack
        .iter()
        .map(|change| change.commit_id.as_str())
        .collect::<HashSet<_>>();
    let roots = stack
        .iter()
        .filter(|change| selected_parent(change, &selected_commits).is_none())
        .collect::<Vec<_>>();

    if roots.len() != 1 {
        let root_labels = roots
            .iter()
            .map(|change| change_label(change))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "unsupported stack shape: multiple roots selected ({} roots): {root_labels}. Move to a single linear stack before running forklift.",
            roots.len()
        );
    }

    let mut children_by_parent = HashMap::<&str, Vec<&ResolvedChange>>::new();
    for change in stack {
        if let Some(parent_id) = selected_parent(change, &selected_commits) {
            children_by_parent
                .entry(parent_id)
                .or_default()
                .push(change);
        }
    }

    if let Some((parent_id, children)) = children_by_parent
        .iter()
        .find(|(_, children)| children.len() > 1)
    {
        let child_labels = children
            .iter()
            .map(|change| change_label(change))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "unsupported stack shape: siblings selected under parent {parent_id} ({} children): {child_labels}. Move to one linear branch before running forklift.",
            children.len()
        );
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
fn selected_parent<'a>(
    change: &'a ResolvedChange,
    selected_commits: &HashSet<&'a str>,
) -> Option<&'a str> {
    change
        .parent_ids
        .iter()
        .map(String::as_str)
        .find(|parent_id| selected_commits.contains(parent_id))
}

#[tracing::instrument(skip_all)]
fn print_github_context(github: &GitHubContext) {
    eprintln!(
        "resolved github: repo={}, username={}",
        github.repo, github.username
    );
}

#[tracing::instrument(skip_all)]
fn print_stack(stack: &[ResolvedChange]) {
    eprintln!("resolved stack table: {} changes", stack.len());
    eprintln!("change\tcommit\ttitle");
    for change in stack {
        eprintln!(
            "{}\t{}\t{}{}",
            change.change_id,
            change.commit_id,
            change.title,
            if change.conflict { " [conflict]" } else { "" }
        );
    }
}

#[tracing::instrument(skip_all)]
fn display_command(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}
