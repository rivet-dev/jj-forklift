use super::super::*;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RebaseDestination {
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

pub(crate) fn sync_rebase_destination(
    config: &AppConfig,
    stack_resolution: &StackResolution,
) -> RebaseDestination {
    stack_resolution
        .frozen_dependencies
        .last()
        .map(|dependency| RebaseDestination::FrozenBookmark(dependency.bookmark.name.clone()))
        .unwrap_or_else(|| RebaseDestination::Trunk(config.trunk.clone()))
}

pub(crate) fn rebase_stack_roots(
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

pub(crate) fn rebase_selected_stack(
    runner: &impl CommandRunner,
    revset: &str,
    stack: &[ResolvedChange],
    destination: RebaseDestination,
    diagnostics: Diagnostics,
) -> Result<usize> {
    let root = stack_root(stack)?;
    let destination_rev = destination.rev().to_owned();
    let args = ["rebase", "-r", revset, "-d", destination_rev.as_str()];

    if diagnostics.dry_run {
        diagnostics.plan_line(&format!(
            "- rebase targeted stack root {} ({}) onto {} `{}`",
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
pub(crate) fn stack_root(stack: &[ResolvedChange]) -> Result<&ResolvedChange> {
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
pub(crate) fn git_rev_parse(runner: &impl CommandRunner, rev: &str) -> Result<String> {
    git_run_required(runner, &["rev-parse", rev])
        .with_context(|| format!("resolve commit id for `{rev}`"))
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn remote_git_ref(config: &AppConfig) -> String {
    format!("{}/{}", config.remote, config.trunk)
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn remote_jj_ref(config: &AppConfig) -> String {
    format!("{}@{}", config.trunk, config.remote)
}
