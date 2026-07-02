use super::super::*;
use super::*;

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
/// 4. **merge-push** — `fast_forward_trunk_over_stack`: fetch the remote first
///    (tracking refs go stale while other clones push trunk; jj would refuse
///    the push with "stale info"), hard-check that remote `trunk` is an
///    ancestor of the stack top (else bail: "run sync first"), set the local
///    `trunk` bookmark to the top commit, and push **once**.
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
pub(crate) async fn diagnose_empty_targeted_merge(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: &MergeTarget,
    narrowed_revset: &str,
    frozen_bookmarks: &[FrozenBookmark],
) -> Result<()> {
    let target_label = target.label();
    let trunk = resolve_single_rev(runner, "trunk()").await?;
    if git_commit_is_ancestor(runner, &target.commit_id, &trunk).await? {
        return Err(CliError::new(format!("{target_label} is already on trunk"))
            .reason(format!("{} is in `{}`", target.commit_id, config.trunk))
            .resolution("choose an unmerged owned PR or run `forklift sync`")
            .into());
    }

    let target_range_revset = format!("trunk()..{} & ~empty()", target.commit_id);
    let target_range = resolve_stack(runner, &target_range_revset).await?;
    let immutable_target_revset = format!("{} & ::(immutable_heads() | root())", target.commit_id);
    let immutable_target = resolve_stack(runner, &immutable_target_revset).await?;
    if !immutable_target.is_empty() {
        let covering_bookmarks =
            frozen_bookmarks_covering_target(runner, &target.commit_id, frozen_bookmarks).await?;
        if !covering_bookmarks.is_empty() {
            let unfreeze_targets = covering_bookmarks
                .iter()
                .map(|bookmark| bookmark.pr_number.to_string())
                .collect::<Vec<_>>();
            let bookmark_names = covering_bookmarks
                .iter()
                .map(|bookmark| format!("`{}`", bookmark.name))
                .collect::<Vec<_>>()
                .join(", ");
            let unfreeze_commands = merge_unfreeze_commands(&unfreeze_targets);
            return Err(MergeUnfreezeRequired::new(
                target.input.clone(),
                unfreeze_targets,
                format!("{} is covered by {bookmark_names}", target.commit_id),
                format!(
                    "run {unfreeze_commands}, then `forklift sync {} --submit --yes`, then rerun `forklift merge {}`",
                    target.input, target.input
                ),
            )
            .into());
        }

        return Err(CliError::new(format!("{target_label} is immutable"))
            .reason(format!(
                "{} is excluded by `immutable_heads()`",
                target.commit_id
            ))
            .resolution("choose an owned mutable stack change before merging it")
            .into());
    }

    if !target_range.is_empty() {
        return Err(
            CliError::new(format!("{target_label} is outside the active stack"))
                .reason(format!(
                    "{} is not in the stack selected from `@`",
                    target.commit_id
                ))
                .resolution(format!(
                    "move to the stack containing {target_label}, then run `forklift merge {}`",
                    target.input
                ))
                .into(),
        );
    }

    Err(
        CliError::new(format!("{target_label} has no owned changes"))
            .reason(format!(
                "`{narrowed_revset}` selected no owned non-empty changes"
            ))
            .resolution("choose an owned mutable non-empty PR before merging it")
            .into(),
    )
}

pub(crate) async fn frozen_bookmarks_covering_target<'a>(
    runner: &impl CommandRunner,
    target_commit: &str,
    frozen_bookmarks: &'a [FrozenBookmark],
) -> Result<Vec<&'a FrozenBookmark>> {
    let mut covering = Vec::new();
    if let Some(bookmark) = frozen_bookmarks
        .iter()
        .find(|bookmark| bookmark.commit_id == target_commit)
    {
        covering.push(bookmark);
    }

    for bookmark in frozen_bookmarks {
        if bookmark.commit_id != target_commit
            && git_commit_is_ancestor(runner, target_commit, &bookmark.commit_id).await?
        {
            covering.push(bookmark);
        }
    }
    sort_frozen_bookmarks_top_down(runner, &mut covering).await?;
    Ok(covering)
}

pub(crate) async fn sort_frozen_bookmarks_top_down(
    runner: &impl CommandRunner,
    bookmarks: &mut Vec<&FrozenBookmark>,
) -> Result<()> {
    let mut ordered = Vec::new();
    while !bookmarks.is_empty() {
        let mut top_index = None;
        for (index, candidate) in bookmarks.iter().enumerate() {
            let mut child_flags = Vec::new();
            for (other_index, other) in bookmarks.iter().enumerate() {
                if other_index == index {
                    continue;
                }
                child_flags.push(
                    git_commit_is_ancestor(runner, &candidate.commit_id, &other.commit_id).await?,
                );
            }
            let has_frozen_child = child_flags.into_iter().any(|is_ancestor| is_ancestor);
            if !has_frozen_child {
                top_index = Some(index);
                break;
            }
        }
        let Some(index) = top_index else {
            bail!("frozen bookmarks covering merge target contain an ancestry cycle");
        };
        ordered.push(bookmarks.remove(index));
    }
    *bookmarks = ordered;
    Ok(())
}

pub(crate) fn merge_unfreeze_commands(targets: &[String]) -> String {
    targets
        .iter()
        .map(|target| format!("`forklift unfreeze {target}`"))
        .collect::<Vec<_>>()
        .join(", then ")
}

#[tracing::instrument(skip_all, fields(revset = %revset))]
pub(crate) async fn merge_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    revset: &str,
    target: Option<&MergeTarget>,
    sync_command: &str,
    admin: bool,
    diagnostics: Diagnostics,
) -> Result<MergeSummary> {
    let mut summary = MergeSummary::default();

    diagnostics.phase("resolve-stack");
    resolve_single_rev(runner, "trunk()")
        .await
        .map_err(|error| phase_error("resolve-stack", "trunk()", error))?;
    let frozen_bookmarks = frozen_bookmarks(runner)
        .await
        .map_err(|error| phase_error("resolve-stack", "frozen-bookmarks", error))?;
    let stack = resolve_stack(runner, revset)
        .await
        .map_err(|error| phase_error("resolve-stack", revset, error))?;
    if stack.is_empty() {
        if let Some(target) = target {
            diagnose_empty_targeted_merge(runner, config, target, revset, &frozen_bookmarks).await?;
        }
        return Ok(summary);
    }
    validate_stack_shape(&stack, revset)
        .map_err(|error| phase_error("resolve-stack", revset, error))?;
    let stack_resolution = resolve_stack_resolution(runner, stack, frozen_bookmarks)
        .await
        .map_err(|error| phase_error("resolve-stack", "frozen-dependencies", error))?;

    let github = GitHubContext::resolve(runner)
        .await
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
    let (_, bottom_pr) = match resolve_merge_pr(runner, config, &context, bottom, diagnostics).await {
        Ok(resolved) => resolved,
        Err(error) if error.downcast_ref::<MergeSubmitRequired>().is_some() => return Err(error),
        Err(error) => return Err(phase_error("merge-pr-lookup", &bottom.change_id, error)),
    };
    validate_merge_frozen_dependencies(runner, config, &context, &bottom_pr).await.map_err(|error| {
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
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                if let Some(progress) = checking_progress {
                    ui_finish_progress_bar(progress);
                }
                if error.downcast_ref::<MergeSubmitRequired>().is_some() {
                    return Err(error);
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
            )
            .await
            {
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
        .await
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
            if let Some(submit_required) = error.downcast_ref::<MergeSubmitRequired>() {
                return anyhow::Error::new(submit_required.clone().with_phase(
                    "merge-pr-check",
                    format!("PR #{}", candidate.entry.pr_number),
                ));
            }
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
        validate_trunk_fast_forward_over_stack(runner, config, &top.commit_id, sync_command)
            .await
            .map_err(|error| merge_push_error(&config.trunk, error))?;
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
    fast_forward_trunk_over_stack(runner, config, &top.commit_id, sync_command, diagnostics)
        .await
        .map_err(|error| merge_push_error(&config.trunk, error))?;
    summary.submit_runs += 1;

    // GitHub marks each PR merged asynchronously once its head lands in trunk.
    verify_prs_merged(runner, &context.github, &pr_numbers, diagnostics)
        .await
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
    )
    .await;

    // Leave the working copy on a fresh empty change on top of the new trunk.
    diagnostics.phase("reset-working-copy");
    reset_working_copy_to_trunk(runner, config, diagnostics)
        .await
        .map_err(|error| phase_error("reset-working-copy", &config.trunk, error))?;

    Ok(summary)
}

/// Re-point a PR's base branch to `trunk` so that a fast-forward push of trunk
/// is recognized by GitHub as merging the PR (GitHub keys auto-merge off the
/// PR's base branch, not just any branch the head lands in).
pub(crate) async fn repoint_pr_base(
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
    let output = gh_run(runner, &args).await?;
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
pub(crate) async fn fast_forward_trunk_over_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    top_commit: &str,
    sync_command: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    // Both the FF validation below and jj's own push-position check trust the
    // local tracking ref for trunk, and merge has not fetched anything up to
    // this point — while other clones push trunk concurrently. Refresh the
    // tracking refs first so a trunk that moved on the remote surfaces as
    // MergeSyncRequired (which merge recovers from by offering sync+submit)
    // instead of jj refusing the push with "stale info", a state rerunning
    // merge could never clear because it never fetched.
    fetch_remote_preserving_local_commits(runner, config, diagnostics).await?;

    validate_trunk_fast_forward_over_stack(runner, config, top_commit, sync_command).await?;

    // jj's default `git.auto-local-bookmark = false` leaves a fetched remote
    // trunk bookmark untracked, and `jj bookmark set <trunk>` below then creates
    // a *separate* non-tracking local bookmark. The subsequent push would fail
    // with "Non-tracking remote bookmark <trunk>@<remote> exists". Repair it
    // here (with a warning) instead of bailing.
    ensure_trunk_tracked(runner, config, diagnostics).await?;

    let previous_trunk = git_rev_parse(runner, &config.trunk).await?;

    let set_args = ["bookmark", "set", config.trunk.as_str(), "-r", top_commit];
    diagnostics.command("jj", &set_args);
    let set = runner.run("jj", &set_args).await?;
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
    let push = runner.run("jj", &push_args).await?;
    if !push.success {
        // The bookmark already moved to the stack top; leaving it there after
        // a failed push strands local trunk on unmerged commits, a diverged
        // state every later sync trunk-move refuses to touch. Restore it
        // (best-effort — the push failure is the error worth reporting).
        let restore_args = [
            "bookmark",
            "set",
            "--allow-backwards",
            config.trunk.as_str(),
            "-r",
            previous_trunk.as_str(),
        ];
        diagnostics.command("jj", &restore_args);
        match runner.run("jj", &restore_args).await {
            Ok(restore) if restore.success => {}
            _ => ui_warn!(
                "failed to restore trunk `{}` to {} after the failed push",
                config.trunk,
                short_commit_id(&previous_trunk)
            ),
        }
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &push_args),
            push.stderr.trim()
        );
    }
    Ok(())
}

pub(crate) fn merge_push_error(object: impl Display, error: anyhow::Error) -> anyhow::Error {
    if let Some(sync_required) = error.downcast_ref::<MergeSyncRequired>() {
        return anyhow::Error::new(
            sync_required
                .clone()
                .with_phase("merge-push", object.to_string()),
        );
    }
    phase_error("merge-push", object, error)
}

#[tracing::instrument(skip_all, fields(top = %top_commit))]
pub(crate) async fn validate_trunk_fast_forward_over_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    top_commit: &str,
    sync_command: &str,
) -> Result<()> {
    let remote_git_ref = remote_git_ref(config);
    let remote = git_rev_parse(runner, &remote_git_ref).await?;
    let is_ancestor = git_run(
        runner,
        &["merge-base", "--is-ancestor", remote.as_str(), top_commit],
    )
    .await?;
    if !is_ancestor.success {
        return Err(MergeSyncRequired::new(
            format!(
            "trunk `{}` cannot fast-forward to {}: remote {} is not an ancestor; run `{}` first",
            config.trunk,
            top_commit,
            remote,
            sync_command
            ),
            format!("run `{sync_command}` to sync and submit the stack before merging"),
        )
        .into());
    }
    Ok(())
}

/// Ensure the local trunk bookmark is tracking `<trunk>@<remote>` before we push
/// it. With jj's default `git.auto-local-bookmark = false`, a fetched remote
/// trunk bookmark is left untracked and forklift's own `jj bookmark set <trunk>`
/// creates a non-tracking local bookmark, so the fast-forward push would abort
/// with "Non-tracking remote bookmark <trunk>@<remote> exists". Rather than fail
/// the merge over a recoverable local-state quirk, auto-track it and warn.
pub(crate) async fn ensure_trunk_tracked(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let status = remote_bookmark_status(runner, config, &config.trunk).await?;
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
    track_remote_bookmark(runner, config, &config.trunk, diagnostics).await
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
pub(crate) fn poll_backoff_sleep(delay_ms: u64) -> u64 {
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    delay_ms.saturating_mul(2).min(POLL_MAX_DELAY_MS)
}

pub(crate) async fn verify_prs_merged(
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
        // Poll every still-pending PR concurrently; a single wave replaces one
        // serial `gh` round-trip per PR on each attempt.
        let merged = stream::iter(
            pending
                .iter()
                .map(|&pr_number| pr_is_merged(runner, github, pr_number)),
        )
        .buffered(NETWORK_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        let mut still_pending = Vec::new();
        for (&pr_number, is_merged) in pending.iter().zip(merged) {
            if !is_merged.unwrap_or(false) {
                still_pending.push(pr_number);
            }
        }
        pending = still_pending;
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
        CliError::new(format!(
            "PRs not marked merged after push: {}",
            pending
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .reason("their head commits are in trunk but GitHub has not closed them")
    );
}

pub(crate) async fn pr_is_merged(
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
    let output = gh_run(runner, &args).await?;
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
pub(crate) async fn reset_working_copy_to_trunk(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = ["new", config.trunk.as_str()];
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args).await?;
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
pub(crate) async fn open_pr_bases_on_branch(
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
    let output = gh_run(runner, &args).await?;
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
pub(crate) async fn cleanup_merged_branches(
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
        match open_pr_bases_on_branch(runner, github, branch).await {
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
        match delete_bookmark(runner, branch, diagnostics).await {
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
        if let Err(error) = push_bookmark_deletions(runner, config, &to_push, diagnostics).await {
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
            forget_bookmark_tracking(runner, branch, diagnostics).await;
        }
    }
    cleaned
}

/// Clean up stack branches whose commits have already landed in trunk (e.g. from
/// a prior merge). A branch is "landed" when its commit is an ancestor of the
/// remote trunk tip, which for the fast-forward merge model means its PR merged.
/// Returns the number of branches removed.
pub(crate) async fn cleanup_landed_branches(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<usize> {
    let bookmarks = local_stack_bookmarks(runner, config).await?;
    if bookmarks.is_empty() {
        return Ok(0);
    }
    let trunk_tip = git_rev_parse(runner, &remote_git_ref(config)).await?;
    let mut landed = Vec::new();
    for branch in bookmarks {
        let commit = jj_ref_commit_id(runner, &branch).await?;
        if git_commit_is_ancestor(runner, &commit, &trunk_tip).await? {
            landed.push(branch);
        }
    }
    if landed.is_empty() {
        return Ok(0);
    }

    let github = GitHubContext::resolve(runner)
        .await
        .context("resolve GitHub repository for merged-branch cleanup")?;
    let cleaned = cleanup_merged_branches(runner, config, &github, &landed, diagnostics).await;
    Ok(cleaned + cleanup_landed_conflicted_bookmarks(runner, config, diagnostics).await?)
}

/// Prune conflicted `<prefix>/*` bookmarks whose named change has already landed
/// in trunk. A squash/abandon can collapse several stack bookmarks onto one
/// surviving commit, leaving each conflicted and mis-pointed; once the change in
/// the bookmark's name is an ancestor of trunk the bookmark is pure residue.
/// Purely local and best-effort — the merged remote branch is already gone, and
/// each deletion is undoable via `jj op undo`.
#[tracing::instrument(skip_all)]
pub(crate) async fn cleanup_landed_conflicted_bookmarks(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<usize> {
    let conflicted = conflicted_local_stack_bookmarks(runner, config).await?;
    let mut cleaned = 0;
    for branch in conflicted {
        let Some(change_prefix) = stack_bookmark_change_id_prefix(&branch) else {
            continue;
        };
        if !change_landed_in_trunk(runner, change_prefix).await? {
            continue;
        }
        if diagnostics.dry_run {
            diagnostics
                .plan_line(&format!("- delete landed conflicted bookmark `{branch}` locally"));
            cleaned += 1;
            continue;
        }
        match delete_bookmark(runner, &branch, diagnostics).await {
            Ok(()) => {
                cleaned += 1;
                // Drop any dangling tracking ref too, without touching the remote.
                forget_bookmark_tracking(runner, &branch, diagnostics).await;
            }
            Err(error) => diagnostics.warn(format!(
                "could not delete conflicted bookmark `{branch}`: {error:#}"
            )),
        }
    }
    Ok(cleaned)
}

/// True when any visible copy of `change_id_prefix` is an ancestor of trunk
/// (i.e. the change merged). A prefix that no longer resolves is treated as
/// not-landed so cleanup leaves the bookmark untouched.
#[tracing::instrument(level = "trace", skip_all)]
async fn change_landed_in_trunk(runner: &impl CommandRunner, change_id_prefix: &str) -> Result<bool> {
    let revset = format!("change_id({change_id_prefix}) & ::trunk()");
    let args = ["log", "--no-graph", "-r", &revset, "-T", "\"x\\n\""];
    let output = runner.run("jj", &args).await?;
    if !output.success {
        return Ok(false);
    }
    Ok(output.stdout.lines().any(|line| !line.trim().is_empty()))
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
pub(crate) async fn refresh_stack_above_merge(
    runner: &impl CommandRunner,
    config: &AppConfig,
    revset: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let remaining = resolve_stack(runner, revset).await?;
    if remaining.is_empty() {
        return Ok(());
    }

    diagnostics.phase("merge-refresh-above");
    let context = resolve_stack_context(runner, revset).await?;
    submit_stack(
        runner,
        config,
        &context,
        true,
        "forklift submit --yes",
        diagnostics,
    )
    .await?;
    Ok(())
}

pub(crate) async fn validate_merge_frozen_dependencies(
    runner: &impl CommandRunner,
    config: &AppConfig,
    context: &AppContext,
    bottom_owned_pr: &GhMergePr,
) -> Result<()> {
    if context.frozen_dependencies.is_empty() {
        return Ok(());
    }

    // Fetch every frozen dependency's PR concurrently; the validation below runs
    // in order and short-circuits on the first offending dependency.
    let fetched = stream::iter(context.frozen_dependencies.iter().map(|dependency| {
        fetch_pr_for_merge(
            runner,
            &context.github,
            &dependency.change.change_id,
            dependency.bookmark.pr_number,
        )
    }))
    .buffered(NETWORK_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    for (dependency, pr) in context.frozen_dependencies.iter().zip(fetched) {
        let pr = pr?;
        if pr.state.eq_ignore_ascii_case("OPEN") {
            bail!(
                CliError::new(format!(
                    "frozen dependency `{}` is still open as PR #{}",
                    dependency.bookmark.name, pr.number
                ))
                .resolution("merge dependencies, then run `forklift sync`")
            );
        }
        if !pr.state.eq_ignore_ascii_case("MERGED") {
            bail!(
                CliError::new(format!(
                    "frozen dependency `{}` PR #{} is `{}`",
                    dependency.bookmark.name, pr.number, pr.state
                ))
                .resolution("run `forklift sync` before merging owned PRs")
            );
        }
    }

    if !bottom_owned_pr
        .base_ref_name
        .eq_ignore_ascii_case(&config.trunk)
    {
        bail!(
            CliError::new(format!(
                "bottom owned PR #{} targets `{}`, expected trunk `{}`",
                bottom_owned_pr.number, bottom_owned_pr.base_ref_name, config.trunk
            ))
            .resolution("run `forklift sync` before merging")
        );
    }

    Ok(())
}

pub(crate) async fn resolve_merge_pr(
    runner: &impl CommandRunner,
    config: &AppConfig,
    context: &AppContext,
    change: &ResolvedChange,
    diagnostics: Diagnostics,
) -> Result<(PrCacheEntry, GhMergePr)> {
    let store = CacheStore::load_current_best_effort(runner, diagnostics, "merge").await?;
    if let Some(entry) = store.get_pr(&context.github.repo, &change.change_id) {
        match resolve_merge_cached_pr(runner, config, &context.github, change, entry).await {
            Ok(resolved) => return Ok(resolved),
            Err(error) => diagnostics.warn(format!(
                "phase=merge-pr-lookup object=cache:{} error=ignored stale cache hint: {error:#}",
                change.change_id
            )),
        }
    }

    resolve_merge_pr_from_live_bookmarks(runner, config, &context.github, change).await
}

#[tracing::instrument(skip_all, fields(change = %change.change_id, pr = entry.pr_number))]
pub(crate) async fn resolve_merge_cached_pr(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    change: &ResolvedChange,
    entry: &PrCacheEntry,
) -> Result<(PrCacheEntry, GhMergePr)> {
    let pr = fetch_pr_for_merge(runner, github, &change.change_id, entry.pr_number).await?;
    if pr.head_ref_name != entry.head_branch {
        bail!(CliError::new(format!(
            "cache points to PR #{} on `{}`, but GitHub reports `{}`",
            entry.pr_number, entry.head_branch, pr.head_ref_name
        )));
    }

    let live_entry = pr.clone().into_cache_entry(entry.stack_comment_id.clone());
    validate_submit_bookmark_state(runner, config, change, &live_entry).await?;
    Ok((live_entry, pr))
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
pub(crate) async fn resolve_merge_pr_from_live_bookmarks(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    change: &ResolvedChange,
) -> Result<(PrCacheEntry, GhMergePr)> {
    let mut matches = Vec::new();
    for head_branch in local_stack_bookmarks_for_change(runner, config, change).await? {
        if let Some(entry) =
            lookup_open_pr_by_head_branch(runner, github, &change.change_id, &head_branch).await?
        {
            validate_submit_bookmark_state(runner, config, change, &entry).await?;
            let pr = fetch_pr_for_merge(runner, github, &change.change_id, entry.pr_number).await?;
            if pr.head_ref_name != head_branch {
                bail!(CliError::new(format!(
                    "PR #{} head branch is `{}`, but live bookmark discovery found `{}`",
                    entry.pr_number, pr.head_ref_name, head_branch
                )));
            }
            matches.push((entry.stack_comment_id.clone(), pr));
        }
    }

    match matches.as_slice() {
        [(comment_id, pr)] => Ok((pr.clone().into_cache_entry(comment_id.clone()), pr.clone())),
        [] => Err(MergeSubmitRequired::new(
            format!(
                "change {} is still local-only: no tracked stack bookmark or GitHub PR was found for `{}`. PRs may exist for changes below it, but merge can only verify submitted changes.",
                short_change_id(&change.change_id),
                change.title
            ),
            "run `forklift submit`, confirm the plan, then rerun `forklift merge`",
        )
        .with_phase("merge-pr-lookup", change.change_id.clone())
        .into()),
        _ => bail!(CliError::new(format!(
            "multiple live tracked PRs found for {}/{}",
            github.repo,
            short_change_id(&change.change_id)
        ))),
    }
}

#[tracing::instrument(skip_all, fields(change = %change_id, pr = pr_number))]
pub(crate) async fn fetch_pr_for_merge(
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
    let output = gh_run(runner, &args).await?;
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
pub(crate) fn mergeability_unknown(pr: &GhMergePr) -> bool {
    pr.mergeable
        .as_deref()
        .map(|state| state.eq_ignore_ascii_case("UNKNOWN"))
        .unwrap_or(true)
}

/// A stack PR being prepared for merge, plus whether it still needs a fresh fetch
/// to settle its mergeability (true after a base re-point, or when GitHub reports
/// a transient `UNKNOWN`).
pub(crate) struct MergeCandidate<'a> {
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
pub(crate) async fn settle_candidates_mergeability(
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
        // Re-fetch every pending candidate's PR concurrently, then apply the
        // results sequentially (the mutation needs `&mut candidates`).
        let fetched = stream::iter(pending.iter().map(|&index| {
            let candidate = &candidates[index];
            fetch_pr_for_merge(
                runner,
                github,
                &candidate.change.change_id,
                candidate.entry.pr_number,
            )
        }))
        .buffered(NETWORK_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        let mut still_pending = Vec::new();
        for (&index, pr) in pending.iter().zip(fetched) {
            candidates[index].pr = match pr {
                Ok(pr) => pr,
                Err(error) => {
                    if let Some(progress) = progress {
                        ui_finish_progress_bar(progress);
                    }
                    return Err(error);
                }
            };
            if mergeability_unknown(&candidates[index].pr) {
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
pub(crate) fn validate_pr_ready_for_merge(
    config: &AppConfig,
    change: &ResolvedChange,
    entry: &PrCacheEntry,
    pr: &GhMergePr,
    admin: bool,
) -> Result<()> {
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            CliError::new(format!(
                "PR #{} for {} is `{}`",
                entry.pr_number,
                short_change_id(&change.change_id),
                pr.state
            ))
            .resolution("only open PRs can be merged")
        );
    }
    if pr.is_draft {
        bail!(
            CliError::new(format!("PR #{} is a draft", entry.pr_number))
                .resolution("mark it ready for review")
        );
    }
    // The PR's GitHub head, the local jj commit, and our cache must all agree
    // before merging — otherwise we'd land code that isn't what's checked out.
    // Disambiguate the three failure shapes so the fix is unambiguous.
    if pr.head_ref_oid != change.commit_id || pr.head_ref_oid != entry.head_sha {
        if pr.head_ref_oid == entry.head_sha {
            // PR and cache agree; only the local commit moved. This is the
            // common case after `sync` rebased the stack without re-pushing.
            return Err(MergeSubmitRequired::new(
                format!(
                "local change {} is now {}, but PR #{} (and the cache) are still at {}; your stack was rewritten (e.g. by `forklift sync`) but not pushed — run `forklift submit` before merging",
                change.change_id,
                change.commit_id,
                entry.pr_number,
                pr.head_ref_oid
                ),
                "run `forklift submit`; it will show the submit plan and ask whether to apply it. Then rerun `forklift merge`.",
            )
            .into());
        }
        if change.commit_id == entry.head_sha {
            // Local commit and cache agree; the PR head moved on GitHub. The
            // branch advanced out-of-band, so refresh local state then re-push.
            return Err(MergeSubmitRequired::new(
                format!(
                "PR #{} head is {} on GitHub, but your local change {} and the cache are both at {}; the PR moved out-of-band — run `forklift sync` then `forklift submit` before merging",
                entry.pr_number,
                pr.head_ref_oid,
                change.change_id,
                change.commit_id
                ),
                "run `forklift sync`, then run `forklift submit`; submit will show the plan and ask whether to apply it. Then rerun `forklift merge`.",
            )
            .into());
        }
        // All three disagree — local, cache, and GitHub have fully drifted.
        return Err(MergeSubmitRequired::new(
            format!(
            "PR #{} is out of sync: GitHub head {}, local change {} is {}, cache expects {}; run `forklift sync` then `forklift submit` before merging",
            entry.pr_number,
            pr.head_ref_oid,
            change.change_id,
            change.commit_id,
            entry.head_sha
            ),
            "run `forklift sync`, then run `forklift submit`; submit will show the plan and ask whether to apply it. Then rerun `forklift merge`.",
        )
        .into());
    }
    if !pr.base_ref_name.eq_ignore_ascii_case(&config.trunk) {
        bail!(
            CliError::new(format!(
                "PR #{} base is `{}`, expected trunk `{}`",
                entry.pr_number, pr.base_ref_name, config.trunk
            ))
            .resolution("run `forklift submit` to repoint the base")
        );
    }
    if config.require_approval && pr.review_decision.as_deref() != Some("APPROVED") {
        return Err(CliError::new(format!(
            "PR #{} requires approval; reviewDecision is `{}`",
            entry.pr_number,
            pr.review_decision.as_deref().unwrap_or("NONE")
        ))
        .resolution(
            "get the PR approved, or rerun `forklift merge --no-require-approval` to skip approval checks. Use `forklift merge --admin` only when you also intend to bypass protected-branch/status gates.",
        )
        .into());
    }
    if pr.auto_merge_request.is_some() {
        bail!(
            CliError::new(format!("PR #{} has auto-merge enabled", entry.pr_number))
                .resolution("disable auto-merge before using direct squash merge")
        );
    }
    if pr.mergeable.as_deref() != Some("MERGEABLE") {
        bail!(CliError::new(format!(
            "PR #{} is not mergeable (mergeable is `{}`)",
            entry.pr_number,
            pr.mergeable.as_deref().unwrap_or("UNKNOWN")
        )));
    }

    match pr.merge_state_status.as_deref().unwrap_or("UNKNOWN") {
        "CLEAN" | "UNSTABLE" => {}
        // With --admin the operator force-pushes trunk past branch protection,
        // so a BLOCKED state is expected and allowed.
        "BLOCKED" if admin => {}
        "QUEUED" => bail!(
            CliError::new(format!("PR #{} is in a merge queue", entry.pr_number))
                .resolution("this workflow only supports direct squash merge")
        ),
        "HAS_HOOKS" => bail!(
            CliError::new(format!(
                "PR #{} is waiting on pending deployments or repository hooks",
                entry.pr_number
            ))
            .resolution("direct squash merge is not safe")
        ),
        "BLOCKED" => bail!(
            CliError::new(format!(
                "PR #{} is blocked by branch protection or an admin-only merge path",
                entry.pr_number
            ))
            .resolution("direct squash merge is not supported")
        ),
        status => bail!(CliError::new(format!(
            "PR #{} cannot be directly squash merged (mergeStateStatus is `{}`)",
            entry.pr_number, status
        ))),
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
pub(crate) fn validate_status_checks(pr_number: u64, checks: &[serde_json::Value]) -> Result<()> {
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
pub(crate) fn check_name(check: &serde_json::Value) -> String {
    check
        .get("name")
        .or_else(|| check.get("context"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown>")
        .to_owned()
}
