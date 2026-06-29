use super::super::*;

/// Result of adopting an existing branch + PR into forklift's tracked set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackOutcome {
    pub(crate) pr_number: u64,
    pub(crate) head_branch: String,
    pub(crate) change_id: String,
    pub(crate) commit_id: String,
    /// True when the tracked commit is not currently in `trunk()..@`, so submit
    /// and sync won't act on it until the working copy moves onto the stack.
    pub(crate) outside_current_stack: bool,
}

/// Adopt the branch behind an existing open PR into forklift: point the local
/// `<head_branch>` bookmark at the PR's head commit, track it against the
/// remote, and write the `pr_cache` row binding the change to that PR. After
/// this, `forklift submit`/`sync` update the existing PR instead of opening a
/// new one. This is forklift's analogue of `gt track`.
#[tracing::instrument(skip_all, fields(target = target))]
pub(crate) fn track_target(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: &str,
    diagnostics: Diagnostics,
) -> Result<TrackOutcome> {
    diagnostics.phase("resolve-github");
    let mut github = GitHubContext::resolve(runner)
        .map_err(|error| phase_error("resolve-github", "github", error))?;
    let parsed = parse_get_target(target, &github.repo)?;
    github.repo = parsed.repo().to_owned();

    diagnostics.phase("resolve-pr");
    let pr = resolve_target_pr(runner, &github, parsed, "track")
        .map_err(|error| phase_error("resolve-pr", target, error))?;

    if pr.merged || !pr.state.eq_ignore_ascii_case("open") {
        bail!(
            CliError::new(format!(
                "PR #{} is {}, not open",
                pr.number,
                if pr.merged { "merged" } else { &pr.state }
            ))
            .resolution("track only applies to open PRs you intend to keep updating")
        );
    }

    let head_branch = pr.head_ref_name.clone();
    // The head branch is passed to jj as a bookmark name / positional; reject a
    // value that could be parsed as a flag even though GitHub ref names cannot
    // normally begin with `-`.
    if head_branch.is_empty() || head_branch.starts_with('-') {
        bail!(CliError::new(format!(
            "PR #{} has an unusable head branch `{head_branch}`",
            pr.number
        )));
    }

    diagnostics.phase("resolve-commit");
    let commit_id = resolve_single_rev(runner, &pr.head_ref_oid).map_err(|error| {
        phase_error(
            "resolve-commit",
            &head_branch,
            anyhow!(
                "PR head {} is not present locally ({error}); run `jj git fetch --remote {}` first",
                short_commit_id(&pr.head_ref_oid),
                config.remote
            ),
        )
    })?;
    let change_id = jj_ref_change_id(runner, &commit_id)
        .map_err(|error| phase_error("resolve-commit", &head_branch, error))?;

    diagnostics.phase("set-bookmark");
    set_local_bookmark(runner, &head_branch, &commit_id, diagnostics)
        .map_err(|error| phase_error("set-bookmark", &head_branch, error))?;

    // Ensure the remote bookmark is tracked so submit's bookmark-state check
    // passes. The remote ref already exists (it is the PR head), but jj only
    // tracks it once asked.
    diagnostics.phase("track-remote");
    let remote_status = remote_bookmark_status(runner, config, &head_branch)
        .map_err(|error| phase_error("track-remote", &head_branch, error))?;
    if !remote_status.tracked {
        track_remote_bookmark(runner, config, &head_branch, diagnostics)
            .map_err(|error| phase_error("track-remote", &head_branch, error))?;
    }

    // Preserve a previously recorded stack-comment id if we are re-tracking.
    diagnostics.phase("save-cache");
    let mut store = CacheStore::load_current_best_effort(runner, diagnostics, "track")?;
    let stack_comment_id = store
        .get_pr(&github.repo, &change_id)
        .and_then(|entry| entry.stack_comment_id.clone());
    let pr_number = pr.number;
    let entry = pr.into_cache_entry(stack_comment_id);
    if !diagnostics.dry_run {
        save_submit_cache_entry(&mut store, &github.repo, &change_id, entry, diagnostics)
            .map_err(|error| phase_error("save-cache", &head_branch, error))?;
    }

    let outside_current_stack = !commit_is_in_current_stack(runner, &commit_id)?;

    Ok(TrackOutcome {
        pr_number,
        head_branch,
        change_id,
        commit_id,
        outside_current_stack,
    })
}

/// Point a local bookmark at `commit_id`, creating it or moving it if it already
/// exists elsewhere (`--allow-backwards` covers a move toward an ancestor).
#[tracing::instrument(skip_all, fields(bookmark = %bookmark))]
fn set_local_bookmark(
    runner: &impl CommandRunner,
    bookmark: &str,
    commit_id: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = [
        "bookmark",
        "set",
        bookmark,
        "-r",
        commit_id,
        "--allow-backwards",
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

/// True when `commit_id` is part of the current owned stack (`trunk()..@`).
fn commit_is_in_current_stack(runner: &impl CommandRunner, commit_id: &str) -> Result<bool> {
    let revset = format!("({commit_id}) & (trunk()..@)");
    let args = ["log", "--no-graph", "-r", &revset, "-T", "commit_id ++ \"\\n\""];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(output.stdout.lines().any(|line| !line.trim().is_empty()))
}
