use super::super::*;

/// Result of adopting an existing branch into forklift's tracked set. When the
/// branch had an open PR, `pr_number` is set and the PR is bound via the cache;
/// otherwise the branch was adopted locally only and `pr_number` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackOutcome {
    pub(crate) pr_number: Option<u64>,
    pub(crate) head_branch: String,
    pub(crate) change_id: String,
    pub(crate) commit_id: String,
    /// True when the tracked commit is not currently in `trunk()..@`, so submit
    /// and sync won't act on it until the working copy moves onto the stack.
    pub(crate) outside_current_stack: bool,
}

/// Adopt an existing branch into forklift's tracked set. If the target resolves
/// to an open PR, bind that PR (set its head bookmark, track the remote, write
/// the `pr_cache` row) so submit/sync update it instead of opening a new one —
/// forklift's analogue of `gt track`. If no PR matches a branch target, fall
/// back to adopting the branch locally so it is still tracked; a later submit
/// opens the PR.
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
    match resolve_target_pr(runner, &github, parsed.clone(), "track") {
        Ok(pr) => adopt_open_pr(runner, config, &github, pr, diagnostics),
        // A PR number that doesn't resolve is a hard error; a branch/change that
        // has no open PR falls back to local-only tracking.
        Err(pr_error) => match parsed {
            GetTarget::BranchOrChange { value, .. } => {
                track_branch_locally(runner, config, &value, diagnostics)
            }
            GetTarget::PullRequest { .. } => Err(phase_error("resolve-pr", target, pr_error)),
        },
    }
}

/// Bind an open PR: set its head bookmark on the PR head commit, track the
/// remote, and record the cache row so submit updates this PR.
fn adopt_open_pr(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    pr: GhPr,
    diagnostics: Diagnostics,
) -> Result<TrackOutcome> {
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
        pr_number: Some(pr_number),
        head_branch,
        change_id,
        commit_id,
        outside_current_stack,
    })
}

/// Adopt a branch that has no open PR: point its local bookmark at the branch
/// commit and track the remote if one exists. No cache row is written (there is
/// no PR yet); a later `forklift submit` opens the PR. The target must name an
/// existing local or remote branch — a bare change id has no branch to track.
fn track_branch_locally(
    runner: &impl CommandRunner,
    config: &AppConfig,
    branch: &str,
    diagnostics: Diagnostics,
) -> Result<TrackOutcome> {
    if branch.is_empty() || branch.starts_with('-') {
        bail!(CliError::new(format!("`{branch}` is not a usable branch name")));
    }

    diagnostics.phase("resolve-branch");
    let commit_id = if bookmark_exists(runner, branch)? {
        jj_ref_commit_id(runner, branch)
            .map_err(|error| phase_error("resolve-branch", branch, error))?
    } else if remote_bookmark_status(runner, config, branch).is_ok() {
        jj_ref_commit_id(runner, &format!("{branch}@{}", config.remote))
            .map_err(|error| phase_error("resolve-branch", branch, error))?
    } else {
        bail!(
            CliError::new(format!(
                "no open PR for `{branch}`, and no local or remote branch by that name"
            ))
            .resolution(
                "pass an open PR (number, URL, or its head branch), or create the bookmark first"
            )
        );
    };
    let change_id = jj_ref_change_id(runner, &commit_id)
        .map_err(|error| phase_error("resolve-branch", branch, error))?;

    diagnostics.phase("set-bookmark");
    set_local_bookmark(runner, branch, &commit_id, diagnostics)
        .map_err(|error| phase_error("set-bookmark", branch, error))?;

    // Track the remote branch if one exists; a purely local branch has nothing
    // to track yet and submit will push it.
    diagnostics.phase("track-remote");
    if let Ok(status) = remote_bookmark_status(runner, config, branch) {
        if !status.tracked && !diagnostics.dry_run {
            track_remote_bookmark(runner, config, branch, diagnostics)
                .map_err(|error| phase_error("track-remote", branch, error))?;
        }
    }

    // No cache row is written: the bookmark we just set on the commit is the
    // durable record that submit reads. The cache stays a best-effort PR-metadata
    // store, never a correctness gate.
    let outside_current_stack = !commit_is_in_current_stack(runner, &commit_id)?;

    Ok(TrackOutcome {
        pr_number: None,
        head_branch: branch.to_owned(),
        change_id,
        commit_id,
        outside_current_stack,
    })
}

/// True when a local bookmark named `name` exists (including a conflicted one).
fn bookmark_exists(runner: &impl CommandRunner, name: &str) -> Result<bool> {
    let revset = format!("bookmarks(exact:'{name}')");
    let args = ["log", "--no-graph", "-r", &revset, "-T", "\"x\\n\""];
    let output = runner.run("jj", &args)?;
    if !output.success {
        return Ok(false);
    }
    Ok(output.stdout.lines().any(|line| !line.trim().is_empty()))
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
