use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
use clap::Parser;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use owo_colors::OwoColorize;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use unicode_normalization::UnicodeNormalization;

use crate::empty_string_to_none;

const CONFIG_PREFIX: &str = "stack";
const DEFAULT_REMOTE: &str = "origin";
const DEFAULT_TRUNK: &str = "main";
const DEFAULT_REQUIRE_APPROVAL: bool = true;
const DEFAULT_BRANCH_PREFIX: &str = "stack";
const DEFAULT_STACK_REVSET: &str = "trunk()..@ & ~::(immutable_heads() | root()) & ~empty()";
const STACK_FIELD_SEPARATOR: char = '\x1f';
const STACK_RECORD_SEPARATOR: char = '\x1e';
const STACK_LOG_TEMPLATE: &str = "json(change_id) ++ \"\\x1f\" ++ json(commit_id) ++ \"\\x1f\" ++ json(parents.map(|c| c.commit_id())) ++ \"\\x1f\" ++ json(description.first_line()) ++ \"\\x1f\" ++ json(description) ++ \"\\x1f\" ++ json(empty) ++ \"\\x1f\" ++ json(conflict) ++ \"\\x1f\" ++ json(divergent) ++ \"\\x1e\"";
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

mod build_info {
    pub const VERSION: &str = env!("CARGO_PKG_VERSION");
    pub const LONG_VERSION: &str = concat!(
        env!("CARGO_PKG_VERSION"),
        "\n",
        "build date: ",
        env!("VERGEN_BUILD_DATE"),
        "\n",
        "build timestamp: ",
        env!("VERGEN_BUILD_TIMESTAMP"),
        "\n",
        "cargo target: ",
        env!("VERGEN_CARGO_TARGET_TRIPLE"),
        "\n",
        "cargo features: ",
        env!("VERGEN_CARGO_FEATURES"),
        "\n",
        "cargo opt level: ",
        env!("VERGEN_CARGO_OPT_LEVEL"),
        "\n",
        "cargo debug: ",
        env!("VERGEN_CARGO_DEBUG"),
        "\n",
        "git branch: ",
        env!("VERGEN_GIT_BRANCH"),
        "\n",
        "git describe: ",
        env!("VERGEN_GIT_DESCRIBE"),
        "\n",
        "git sha: ",
        env!("VERGEN_GIT_SHA"),
        "\n",
        "git dirty: ",
        env!("VERGEN_GIT_DIRTY"),
        "\n",
        "git commit count: ",
        env!("VERGEN_GIT_COMMIT_COUNT"),
        "\n",
        "git commit date: ",
        env!("VERGEN_GIT_COMMIT_DATE"),
        "\n",
        "git commit timestamp: ",
        env!("VERGEN_GIT_COMMIT_TIMESTAMP"),
        "\n",
        "rustc: ",
        env!("VERGEN_RUSTC_SEMVER"),
        "\n",
        "rustc channel: ",
        env!("VERGEN_RUSTC_CHANNEL"),
        "\n",
        "rustc host: ",
        env!("VERGEN_RUSTC_HOST_TRIPLE"),
        "\n",
        "rustc commit: ",
        env!("VERGEN_RUSTC_COMMIT_HASH"),
        "\n",
        "rustc commit date: ",
        env!("VERGEN_RUSTC_COMMIT_DATE"),
        "\n",
        "rustc llvm: ",
        env!("VERGEN_RUSTC_LLVM_VERSION")
    );
}

#[macro_use]
#[path = "ui.rs"]
mod ui;
#[path = "cache.rs"]
mod cache;
#[path = "cli.rs"]
mod cli;
#[path = "commands/mod.rs"]
mod commands;
#[path = "config.rs"]
mod config;
#[path = "diagnostics.rs"]
mod diagnostics;
#[path = "errors.rs"]
mod errors;
#[path = "github/mod.rs"]
mod github;
#[path = "jj/mod.rs"]
mod jj;
#[path = "runner.rs"]
mod runner;
#[path = "tracing_setup.rs"]
mod tracing_setup;
#[path = "workflows/mod.rs"]
mod workflows;

use cache::*;
use cli::{Cli, Commands};
use config::*;
use diagnostics::*;
use errors::*;
use github::*;
use jj::*;
use runner::*;
use tracing_setup::*;
use ui::*;
use workflows::*;

pub fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    // `run` prints a human-readable failure itself; main only sets the exit code
    // so anyhow doesn't also dump the raw structured error to the terminal.
    match run(cli, &SystemRunner) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

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
        version = build_info::VERSION,
        build_date = env!("VERGEN_BUILD_DATE"),
        build_timestamp = env!("VERGEN_BUILD_TIMESTAMP"),
        cargo_target = env!("VERGEN_CARGO_TARGET_TRIPLE"),
        cargo_features = env!("VERGEN_CARGO_FEATURES"),
        cargo_opt_level = env!("VERGEN_CARGO_OPT_LEVEL"),
        cargo_debug = env!("VERGEN_CARGO_DEBUG"),
        git_branch = env!("VERGEN_GIT_BRANCH"),
        git_describe = env!("VERGEN_GIT_DESCRIBE"),
        git_sha = env!("VERGEN_GIT_SHA"),
        git_dirty = env!("VERGEN_GIT_DIRTY"),
        git_commit_count = env!("VERGEN_GIT_COMMIT_COUNT"),
        git_commit_date = env!("VERGEN_GIT_COMMIT_DATE"),
        git_commit_timestamp = env!("VERGEN_GIT_COMMIT_TIMESTAMP"),
        rustc = env!("VERGEN_RUSTC_SEMVER"),
        rustc_channel = env!("VERGEN_RUSTC_CHANNEL"),
        rustc_host = env!("VERGEN_RUSTC_HOST_TRIPLE"),
        rustc_commit = env!("VERGEN_RUSTC_COMMIT_HASH"),
        rustc_commit_date = env!("VERGEN_RUSTC_COMMIT_DATE"),
        rustc_llvm = env!("VERGEN_RUSTC_LLVM_VERSION"),
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
        Commands::Submit(options) => commands::submit::run(
            runner,
            &config,
            options,
            diagnostics,
            cli.verbose,
            cli.dry_run,
        )?,
        Commands::Sync(options) => commands::sync::run(
            runner,
            &config,
            options,
            diagnostics,
            cli.verbose,
            cli.dry_run,
        )?,
        Commands::Merge(options) => commands::merge::run(
            runner,
            &config,
            options,
            diagnostics,
            cli.verbose,
            cli.dry_run,
        )?,
        Commands::Get(options) => commands::get::run(
            runner,
            &config,
            options,
            diagnostics,
            cli.verbose,
            cli.dry_run,
        )?,
        Commands::Repair(options) => commands::repair::run(
            runner,
            &config,
            options,
            diagnostics,
            cli.verbose,
            cli.dry_run,
        )?,
        Commands::Unfreeze(options) => commands::unfreeze::run(
            runner,
            &config,
            options,
            diagnostics,
            cli.verbose,
            cli.dry_run,
        )?,
        Commands::Status(options) => commands::status::run(
            runner,
            &config,
            options,
            diagnostics,
            cli.verbose,
            cli.dry_run,
        )?,
        Commands::Pr(options) => commands::pr::run(
            runner,
            &config,
            options,
            diagnostics,
            cli.verbose,
            cli.dry_run,
        )?,
    }

    Ok(())
}
