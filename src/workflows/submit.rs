use super::super::*;
use super::*;

/// A `stack/` bookmark that collapsed onto a kept commit during a rebase/squash.
///
/// When one change is folded into another, jj strands the absorbed change's
/// bookmark on the surviving commit, so a single commit ends up carrying two
/// PR branches. The bookmark whose change-id suffix matches the commit's own
/// change id stays canonical; the rest become orphans recorded here so submit
/// can offer to close their now-dangling PRs.
#[derive(Debug, Clone)]
pub(crate) struct OrphanedPr {
    pub(crate) bookmark: String,
    pub(crate) pr_number: u64,
    pub(crate) host_change_id: String,
    pub(crate) host_commit_id: String,
}

pub(crate) async fn validate_submit_bookmark_state(
    runner: &impl CommandRunner,
    config: &AppConfig,
    change: &ResolvedChange,
    entry: &PrCacheEntry,
) -> Result<()> {
    let local_target = jj_ref_commit_id(runner, &entry.head_branch).await.with_context(|| {
        format!(
            "local head bookmark `{}` is missing or conflicted",
            entry.head_branch
        )
    })?;
    if local_target != change.commit_id {
        // The bookmark may be stranded on a divergent sibling — the same change
        // resolved to a second visible commit (e.g. a duplicate that didn't
        // carry the bookmark). That is safe: the push phase re-points the
        // bookmark onto the selected copy with `jj bookmark set`. Only bail when
        // the bookmark sits on an unrelated change.
        let local_change_id = jj_ref_change_id(runner, &entry.head_branch).await.with_context(|| {
            format!(
                "resolve change id for local head bookmark `{}`",
                entry.head_branch
            )
        })?;
        if local_change_id != change.change_id {
            bail!(CliError::new(format!(
                "local head bookmark `{}` points at {} (change {}), but selected change {} is {}",
                entry.head_branch,
                short_commit_id(&local_target),
                short_change_id(&local_change_id),
                short_change_id(&change.change_id),
                short_commit_id(&change.commit_id)
            )));
        }
    }

    let remote = remote_bookmark_status(runner, config, &entry.head_branch).await?;
    if !remote.tracked {
        bail!(
            CliError::new(format!(
                "remote bookmark `{}@{}` is untracked",
                entry.head_branch, config.remote
            ))
            .resolution("track the PR branch in jj before submitting")
        );
    }
    if remote.conflicted {
        bail!(
            CliError::new(format!(
                "bookmark `{}` is conflicted with remote `{}`",
                entry.head_branch, config.remote
            ))
            .resolution("resolve the jj bookmark conflict before submitting")
        );
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_submit_head_branch(
    runner: &impl CommandRunner,
    config: &AppConfig,
    used_head_branches: &mut HashSet<String>,
    store: &CacheStore,
    context: &AppContext,
    change: &ResolvedChange,
    orphans: &mut Vec<OrphanedPr>,
    diagnostics: Diagnostics,
) -> Result<(String, Option<PrCacheEntry>, Option<String>)> {
    if let Some(discovered) = discover_existing_pr_from_local_bookmarks(
        runner,
        config,
        used_head_branches,
        &context.github,
        change,
        orphans,
    )
    .await?
    {
        return Ok(discovered);
    }

    // A branch the user adopted with `forklift track <branch>` lives as a real
    // local bookmark on the change's commit — the durable source of truth, no
    // cache required. Prefer it as the PR head over a generated `stack/*` name.
    if let Some(adopted) = adopted_head_bookmark_for_change(runner, config, change).await? {
        if !used_head_branches.contains(&adopted) {
            return resolve_submit_pending_head_branch(
                runner,
                config,
                used_head_branches,
                &context.github,
                change,
                &adopted,
            )
            .await;
        }
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
        )
        .await
        {
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
        lookup_open_pr_by_head_branch(runner, &context.github, &change.change_id, &head_branch)
            .await?;
    let expected_remote_head = remote_head_oid(runner, &config.remote, &head_branch).await?;

    if let Some(existing_pr) = existing_pr {
        validate_submit_bookmark_state(runner, config, change, &existing_pr).await?;
        used_head_branches.insert(head_branch.clone());
        return Ok((head_branch, Some(existing_pr), expected_remote_head));
    }

    if let Some(remote_head) = &expected_remote_head {
        bail!(CliError::new(format!(
            "remote branch `{}` already exists at {} with no matching PR in the cache",
            head_branch,
            short_commit_id(remote_head)
        ))
        .resolution(
            "run `forklift get` for the PR that owns it, delete the branch, or choose a different change title"
        ));
    }

    used_head_branches.insert(head_branch.clone());
    Ok((head_branch, None, None))
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
pub(crate) async fn resolve_submit_cached_head_branch(
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
    validate_submit_bookmark_state(runner, config, change, entry).await?;
    let existing_pr = lookup_cached_pr(runner, github, &change.change_id, &head_branch, entry).await?;
    let expected_remote_head = remote_head_oid(runner, &config.remote, &head_branch).await?;
    used_head_branches.insert(head_branch.clone());
    Ok((head_branch, Some(existing_pr), expected_remote_head))
}

/// Resolve the head branch for a change whose head is an adopted bookmark
/// (`forklift track <branch>`). Use that branch as the head: adopt an open PR if
/// one now exists, otherwise return it for the create path. Unlike the
/// deterministic path, an existing remote branch is not treated as a collision —
/// it is the user's deliberately tracked branch.
#[tracing::instrument(skip_all, fields(change = %change.change_id))]
pub(crate) async fn resolve_submit_pending_head_branch(
    runner: &impl CommandRunner,
    config: &AppConfig,
    used_head_branches: &mut HashSet<String>,
    github: &GitHubContext,
    change: &ResolvedChange,
    head_branch: &str,
) -> Result<(String, Option<PrCacheEntry>, Option<String>)> {
    if used_head_branches.contains(head_branch) {
        bail!("cache records duplicate head branch `{head_branch}` in stack");
    }
    let existing_pr =
        lookup_open_pr_by_head_branch(runner, github, &change.change_id, head_branch).await?;
    if let Some(existing_pr) = &existing_pr {
        validate_submit_bookmark_state(runner, config, change, existing_pr).await?;
    }
    let expected_remote_head = remote_head_oid(runner, &config.remote, head_branch).await?;
    used_head_branches.insert(head_branch.to_owned());
    Ok((head_branch.to_owned(), existing_pr, expected_remote_head))
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
pub(crate) async fn discover_existing_pr_from_local_bookmarks(
    runner: &impl CommandRunner,
    config: &AppConfig,
    used_head_branches: &mut HashSet<String>,
    github: &GitHubContext,
    change: &ResolvedChange,
    orphans: &mut Vec<OrphanedPr>,
) -> Result<Option<(String, Option<PrCacheEntry>, Option<String>)>> {
    let mut matches = Vec::new();
    for head_branch in local_stack_bookmarks_for_change(runner, config, change).await? {
        if used_head_branches.contains(&head_branch) {
            continue;
        }
        if let Some(existing_pr) =
            lookup_open_pr_by_head_branch(runner, github, &change.change_id, &head_branch).await?
        {
            matches.push((head_branch, existing_pr));
        }
    }

    match matches.as_slice() {
        [] => Ok(None),
        [(head_branch, existing_pr)] => {
            validate_submit_bookmark_state(runner, config, change, existing_pr).await?;
            let expected_remote_head = remote_head_oid(runner, &config.remote, head_branch).await?;
            used_head_branches.insert(head_branch.clone());
            Ok(Some((
                head_branch.clone(),
                Some(existing_pr.clone()),
                expected_remote_head,
            )))
        }
        _ => {
            resolve_collapsed_bookmark_matches(
                runner,
                config,
                change,
                matches,
                used_head_branches,
                orphans,
            )
            .await
        }
    }
}

/// Disambiguate several PR-bearing `stack/` bookmarks that collapsed onto one
/// commit. forklift encodes the owning change id as each bookmark's suffix, so
/// the bookmark whose suffix matches the commit's own change id is the canonical
/// owner; the rest are orphans from changes that got absorbed by a rebase/squash.
/// We proceed with the canonical bookmark and record the orphans for cleanup.
/// Only when exactly one canonical bookmark can't be identified do we bail.
async fn resolve_collapsed_bookmark_matches(
    runner: &impl CommandRunner,
    config: &AppConfig,
    change: &ResolvedChange,
    matches: Vec<(String, PrCacheEntry)>,
    used_head_branches: &mut HashSet<String>,
    orphans: &mut Vec<OrphanedPr>,
) -> Result<Option<(String, Option<PrCacheEntry>, Option<String>)>> {
    let change_prefix = change_id_branch_prefix(&change.change_id);
    let (mut owned, orphaned): (Vec<_>, Vec<_>) = matches
        .into_iter()
        .partition(|(branch, _)| head_branch_matches_change_prefix(branch, change_prefix));

    if owned.len() != 1 {
        // Zero or several bookmarks claim this change id: genuinely ambiguous,
        // so refuse rather than guess which PR owns the commit.
        let reason = owned
            .iter()
            .chain(orphaned.iter())
            .map(|(branch, pr)| format!("{} -> PR #{}", branch, pr.pr_number))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            CliError::new(format!(
                "multiple local `{}` bookmarks at {} have open GitHub PRs for {}",
                config.branch_prefix,
                short_commit_id(&change.commit_id),
                short_change_id(&change.change_id)
            ))
            .reason(reason)
            .resolution(
                "delete the stale bookmarks (or close their PRs) so each stack commit owns exactly one PR branch"
            )
        );
    }

    let (head_branch, existing_pr) = owned.pop().expect("exactly one canonical bookmark");
    validate_submit_bookmark_state(runner, config, change, &existing_pr).await?;
    let expected_remote_head = remote_head_oid(runner, &config.remote, &head_branch).await?;
    used_head_branches.insert(head_branch.clone());

    for (branch, pr) in orphaned {
        ui_warn!(
            "bookmark `{}` (PR #{}) was absorbed into {} ({}); it no longer maps to a distinct commit",
            branch,
            pr.pr_number,
            short_change_id(&change.change_id),
            short_commit_id(&change.commit_id),
        );
        orphans.push(OrphanedPr {
            bookmark: branch,
            pr_number: pr.pr_number,
            host_change_id: change.change_id.clone(),
            host_commit_id: change.commit_id.clone(),
        });
    }

    Ok(Some((head_branch, Some(existing_pr), expected_remote_head)))
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
pub(crate) async fn local_stack_bookmarks_for_change(
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
    let output = runner.run("jj", &args).await?;
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
            let status = fields.next().unwrap_or_default().trim();
            let target = fields.next().unwrap_or_default().trim();
            if !remote.is_empty()
                || !name.starts_with(&prefix)
                || status == "conflicted"
                || !is_resolvable_bookmark_target(target)
            {
                return None;
            }
            Some(name.to_owned())
        })
        .collect::<Vec<_>>();
    bookmarks.sort();
    bookmarks.dedup();
    Ok(bookmarks)
}

/// A head branch the user adopted with `forklift track <branch>`: a local,
/// non-conflicted bookmark on the change's commit that is *not* a generated
/// `stack/*` name, the trunk, or a frozen dependency. This reads jj's bookmarks
/// directly, so submit honors a tracked branch without depending on the cache.
/// Returns `None` unless there is exactly one such candidate (an ambiguous set
/// falls back to the deterministic name rather than guessing).
#[tracing::instrument(skip_all, fields(change = %change.change_id))]
pub(crate) async fn adopted_head_bookmark_for_change(
    runner: &impl CommandRunner,
    config: &AppConfig,
    change: &ResolvedChange,
) -> Result<Option<String>> {
    let args = [
        "bookmark",
        "list",
        "--revision",
        change.commit_id.as_str(),
        "-T",
        LOCAL_BOOKMARK_TEMPLATE,
    ];
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let stack_prefix = format!("{}/", config.branch_prefix.trim_end_matches('/'));
    let mut candidates = output
        .stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let remote = fields.next().unwrap_or_default().trim();
            let status = fields.next().unwrap_or_default().trim();
            let target = fields.next().unwrap_or_default().trim();
            if !remote.is_empty()
                || status == "conflicted"
                || !is_resolvable_bookmark_target(target)
                || name == config.trunk
                || name.starts_with(&stack_prefix)
                || name.starts_with("forklift/frozen/")
            {
                return None;
            }
            Some(name.to_owned())
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();
    match candidates.as_slice() {
        [only] => Ok(Some(only.clone())),
        _ => Ok(None),
    }
}

pub(crate) async fn submit_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    context: &AppContext,
    yes: bool,
    yes_command: &str,
    diagnostics: Diagnostics,
) -> Result<SubmitSummary> {
    // Surface divergence before planning so the heads-up is visible regardless of
    // which copy the PR bookmark was stranded on; the push phase re-points it.
    warn_divergent_changes(&context.stack);

    diagnostics.phase("validate-submit-bases");
    validate_submit_bases(runner, config, &context.stack, &context.frozen_dependencies)
        .await
        .map_err(|error| phase_error("validate-submit-bases", "stack", error))?;
    validate_submit_descriptions(&context.stack)
        .map_err(|error| phase_error("validate-submit-bases", "stack", error))?;

    tracing::debug!(phase = "plan-submit", "recovery phase");
    let plan_progress = diagnostics.progress_bar("Planning", "submit", context.stack.len());
    let mut store = CacheStore::load_current_best_effort(runner, diagnostics, "plan-submit")
        .await
        .map_err(|error| phase_error("plan-submit", "cache", error))?;
    diagnostics.repo_details(&store);
    let frozen_entries = resolve_submit_frozen_dependency_entries(
        runner,
        &context.github,
        &context.frozen_dependencies,
        diagnostics,
    )
    .await
    .map_err(|error| phase_error("plan-submit", "frozen dependencies", error))?;
    let mut plans = Vec::new();
    let mut used_head_branches = HashSet::new();
    let mut orphaned_prs: Vec<OrphanedPr> = Vec::new();
    let mut previous_head_branch = frozen_entries
        .last()
        .map(|(_, entry)| entry.head_branch.clone());

    for (index, change) in context.stack.iter().enumerate() {
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
            &mut orphaned_prs,
            diagnostics,
        )
        .await
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
        tracing::debug!(
            phase = "plan-submit",
            change = %change.change_id,
            head_branch = %head_branch,
            base_branch = %base_branch,
            push_needed,
            pr_update_needed,
            existing_pr = existing_pr.as_ref().map(|entry| entry.pr_number),
            "planned submit change"
        );
        plans.push(SubmitPlan {
            change: change.clone(),
            head_branch,
            base_branch,
            existing_pr,
            expected_remote_head,
            push_needed,
            pr_update_needed,
        });
        if let Some(progress) = &plan_progress {
            progress.set_position((index + 1) as u64);
        }
    }
    if let Some(progress) = plan_progress {
        ui_finish_progress_bar(progress);
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
        for orphan in &orphaned_prs {
            diagnostics.plan_line(&format!(
                "- would offer to close orphaned PR #{} and delete branch {} (absorbed into {})",
                orphan.pr_number,
                orphan.bookmark,
                short_change_id(&orphan.host_change_id)
            ));
        }
        diagnostics.plan_line(
            "- live jj/GitHub discovery ran during planning; SQLite cache writes are skipped",
        );
        return Ok(summary);
    }
    print_submit_action_plan(&context.github.repo, &plans);
    confirm_submit_stack(yes, yes_command)?;

    // Anything that touches a file between planning and here — a build tool
    // rewriting Cargo.lock after a rebase, an editor save while the prompt
    // waits — gets absorbed by the next jj command's working-copy snapshot,
    // which rewrites the affected stack commits and hides the planned ids.
    // `jj bookmark set` on a hidden id resurrects it next to its successor
    // and leaves the change divergent, so re-pin every plan to its change's
    // current visible commit first (this re-resolve performs the snapshot
    // itself; the push phase below then skips snapshotting entirely).
    reconcile_plans_with_current_commits(runner, &mut plans).await?;

    tracing::debug!(phase = "push-refs", "recovery phase");
    push_changed_heads(runner, config, &plans, diagnostics).await?;

    let mut entries = Vec::new();

    let pr_progress = diagnostics.progress_bar("Submitting", "pull requests", plans.len());
    // Create/update every PR concurrently. Each head branch is already on the
    // remote from the push phase above, so GitHub accepts the calls in any
    // order; only the cache writes and progress reporting below stay sequential.
    let pr_results = stream::iter(plans.iter().map(|plan| {
        let previous_comment_id = plan
            .existing_pr
            .as_ref()
            .and_then(|entry| entry.stack_comment_id.clone());
        async move {
            let result: Result<(SubmitPrAction, PrCacheEntry)> = match &plan.existing_pr {
                None => Ok((
                    SubmitPrAction::Submit,
                    create_pr(runner, &context.github, plan, diagnostics)
                        .await?
                        .into_cache_entry(previous_comment_id),
                )),
                Some(existing) if plan.pr_update_needed => Ok((
                    SubmitPrAction::Update,
                    update_pr(runner, &context.github, existing.pr_number, plan, diagnostics)
                        .await?
                        .into_cache_entry(previous_comment_id),
                )),
                Some(existing) => Ok((
                    SubmitPrAction::Nothing,
                    refreshed_cache_entry(existing, plan, previous_comment_id),
                )),
            };
            result
        }
    }))
    .buffered(NETWORK_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    for (index, (plan, result)) in plans.into_iter().zip(pr_results).enumerate() {
        let (action, entry) = result?;
        diagnostics.submit_pr_action(
            &context.github.repo,
            &plan.change,
            action,
            &entry,
            pr_progress.as_ref(),
        );
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

    tracing::debug!(phase = "stack-comments", "recovery phase");
    let comment_entries = entries
        .iter()
        .map(|(change, entry)| (change.change_id.clone(), entry.clone()))
        .collect::<Vec<_>>();
    let comment_progress = diagnostics.progress_bar("Updating", "stack comments", entries.len());
    // Bodies are computed up front from the fully-resolved entry set (pure), so
    // the comment upserts are independent and run concurrently. Summary counters
    // and cache writes stay sequential below.
    let comment_results = stream::iter(entries.iter().map(|(change, entry)| {
        let body = stack_comment_body_with_frozen(
            context,
            &frozen_entries,
            &comment_entries,
            &change.change_id,
            &config.trunk,
        );
        async move {
            upsert_stack_comment(
                runner,
                &context.github,
                entry.pr_number,
                &change.change_id,
                &body,
                diagnostics,
            )
            .await
        }
    }))
    .buffered(NETWORK_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    for (index, ((change, mut entry), action)) in
        entries.into_iter().zip(comment_results).enumerate()
    {
        match action? {
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

    if !orphaned_prs.is_empty() {
        summary.closed_orphans =
            close_orphaned_prs(runner, &context.github, &orphaned_prs, yes, diagnostics).await?;
    }

    Ok(summary)
}

/// Offer to close PRs whose `stack/` bookmarks collapsed onto a kept commit.
/// Returns the number of PRs actually closed. Closing a PR is destructive and
/// outward-facing, so it requires explicit consent: `--yes` (or an interactive
/// `y`) closes them; a bare non-interactive run leaves them open with a hint.
async fn close_orphaned_prs(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    orphans: &[OrphanedPr],
    yes: bool,
    diagnostics: Diagnostics,
) -> Result<usize> {
    if !confirm_close_orphans(orphans, yes)? {
        for orphan in orphans {
            ui_warn!(
                "left orphaned PR #{} (`{}`) open; close it with `gh pr close {} --delete-branch` when ready",
                orphan.pr_number,
                orphan.bookmark,
                orphan.pr_number,
            );
        }
        return Ok(0);
    }

    tracing::debug!(phase = "close-orphans", "recovery phase");
    let progress = diagnostics.progress_bar("Closing", "orphaned PRs", orphans.len());
    for (index, orphan) in orphans.iter().enumerate() {
        close_orphaned_pr(runner, github, orphan, diagnostics).await?;
        // The PR's remote branch is gone (gh deleted it); forget the local
        // bookmark and its remote-tracking ref so jj stops carrying the strand.
        forget_bookmark_tracking(runner, &orphan.bookmark, diagnostics).await;
        if let Some(progress) = &progress {
            progress.set_position((index + 1) as u64);
        }
    }
    if let Some(progress) = progress {
        ui_finish_progress_bar(progress);
    }
    Ok(orphans.len())
}

async fn close_orphaned_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    orphan: &OrphanedPr,
    diagnostics: Diagnostics,
) -> Result<()> {
    let number = orphan.pr_number.to_string();
    let args = [
        "pr",
        "close",
        number.as_str(),
        "--repo",
        github.repo.as_str(),
        "--delete-branch",
    ];
    diagnostics.command("gh", &args);
    let output = gh_run(runner, &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("gh", &args),
            output.stderr.trim()
        );
    }
    ui_progress(
        "Closed",
        &format!(
            "orphaned PR #{} (`{}`) absorbed into {}",
            orphan.pr_number,
            orphan.bookmark,
            short_commit_id(&orphan.host_commit_id)
        ),
    );
    Ok(())
}

fn confirm_close_orphans(orphans: &[OrphanedPr], yes: bool) -> Result<bool> {
    ui_warn!(
        "{} orphaned PR branch(es) collapsed onto a kept commit:",
        orphans.len()
    );
    for orphan in orphans {
        ui_warn!("- PR #{} `{}`", orphan.pr_number, orphan.bookmark);
    }

    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        // Never close GitHub PRs implicitly in a non-interactive run.
        return Ok(false);
    }

    eprint!(
        "Close {} orphaned PR(s) and delete their branches? [y/N] ",
        orphans.len()
    );
    io::stderr()
        .flush()
        .context("flush orphan confirmation prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read orphan confirmation")?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

pub(crate) async fn resolve_submit_frozen_dependency_entries(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    dependencies: &[FrozenDependency],
    diagnostics: Diagnostics,
) -> Result<Vec<(String, PrCacheEntry)>> {
    // Fetch every frozen dependency's PR concurrently; the per-dependency
    // validation and cache-entry assembly below stay ordered and sequential so
    // warnings and errors surface deterministically.
    let prs = stream::iter(dependencies.iter().map(|dependency| {
        fetch_pr_by_number(
            runner,
            github,
            &dependency.change.change_id,
            dependency.bookmark.pr_number,
        )
    }))
    .buffered(NETWORK_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut entries = Vec::new();
    for (dependency, pr) in dependencies.iter().zip(prs) {
        let pr = pr?;
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
pub(crate) fn validate_submit_frozen_dependency_pr(
    github: &GitHubContext,
    dependency: &FrozenDependency,
    pr: &GhPr,
) -> Result<()> {
    validate_get_pr_metadata(github, pr)?;
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            CliError::new(format!(
                "frozen dependency `{}` points to PR #{}, but GitHub reports state {}",
                dependency.bookmark.name, pr.number, pr.state
            ))
            .resolution("run `forklift sync` before submitting")
        );
    }
    let head_repo = get_pr_repo(pr, "head")?;
    let base_repo = get_pr_repo(pr, "base")?;
    if head_repo.name_with_owner != github.repo || base_repo.name_with_owner != github.repo {
        bail!(
            CliError::new(format!(
                "frozen dependency `{}` PR #{} must be same-repo before submit",
                dependency.bookmark.name, pr.number
            ))
            .resolution("run `forklift sync`, or unfreeze manually")
        );
    }
    if pr.head_ref_oid != dependency.change.commit_id {
        bail!(
            CliError::new(format!(
                "frozen dependency `{}` points at {}, but GitHub PR #{} head is {}",
                dependency.bookmark.name,
                short_commit_id(&dependency.change.commit_id),
                pr.number,
                short_commit_id(&pr.head_ref_oid)
            ))
            .resolution("run `forklift sync` before submitting")
        );
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(repo = %repo, change = %change_id))]
pub(crate) fn save_submit_cache_entry(
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
pub(crate) fn pr_metadata_changed(
    entry: &PrCacheEntry,
    base_branch: &str,
    change: &ResolvedChange,
) -> bool {
    entry.base_branch != base_branch
        || entry.base_ref != base_branch
        || entry.title != change.title
        || entry.body != change.body
}

#[tracing::instrument(skip_all, fields(change = %current_change_id))]
pub(crate) fn stack_comment_body_with_frozen(
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
        body.push_str(&format!("Get stack: `forklift get {}`\n", entry.pr_number));
        body.push_str("Push local edits: `forklift submit`\n");
        body.push_str(&format!(
            "Merge when ready: `forklift merge {}`\n",
            entry.pr_number
        ));
    }

    body
}

#[tracing::instrument(skip_all, fields(current_pr = current_pr_number))]
pub(crate) fn repaired_stack_comment_body(
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
        body.push_str(&format!("Get stack: `forklift get {current_pr_number}`\n"));
        body.push_str("Push local edits: `forklift submit`\n");
        body.push_str(&format!(
            "Merge when ready: `forklift merge {current_pr_number}`\n"
        ));
    }

    body
}

#[tracing::instrument(level = "trace", skip_all, fields(pr = pr.number))]
pub(crate) fn push_repaired_stack_comment_line(
    body: &mut String,
    repo: &str,
    pr: &GhPr,
    is_current: bool,
) {
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
pub(crate) fn stack_comment_change_hint(head_branch: &str) -> String {
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
pub(crate) fn push_stack_comment_line(
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
pub(crate) fn created_date_fragment(created_at: &str) -> String {
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
pub(crate) fn github_pr_url(repo: &str, pr_number: u64) -> String {
    format!("https://github.com/{repo}/pull/{pr_number}")
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn markdown_link_label(value: &str) -> String {
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
pub(crate) fn refreshed_cache_entry(
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

/// True when the stack's base is trunk and the local trunk bookmark has moved
/// past the stack root's fork point — typically because the pre-submit fetch
/// fast-forwarded it. Submit base validation rejects that state, so the stack
/// must be rebased onto the new trunk before it can be submitted.
pub(crate) async fn stack_behind_trunk(
    runner: &impl CommandRunner,
    config: &AppConfig,
    context: &AppContext,
) -> Result<bool> {
    if !context.frozen_dependencies.is_empty() {
        return Ok(false);
    }
    let Some(root) = context.stack.first() else {
        return Ok(false);
    };
    let trunk_tip = resolve_single_rev(runner, &config.trunk).await?;
    Ok(merge_base(runner, &root.commit_id, &trunk_tip).await? != trunk_tip)
}

/// Ask before submit performs sync's trunk-move + rebase on a stack that fell
/// behind trunk. Runs before anything is mutated, so declining leaves the repo
/// untouched. `--yes` accepts without prompting.
pub(crate) fn confirm_sync_before_submit(trunk: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }

    if !io::stdin().is_terminal() {
        return Err(CliError::new("submit requires the stack to be synced")
            .reason(format!(
                "the stack is behind trunk `{trunk}` and stdin is not a terminal"
            ))
            .resolution(
                "run `forklift sync`, or rerun with `forklift submit --yes` to sync and submit",
            )
            .into());
    }

    eprint!("Stack is behind trunk `{trunk}` — sync before submit? [y/N] ");
    io::stderr()
        .flush()
        .context("flush sync-before-submit prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read sync-before-submit confirmation")?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        return Ok(());
    }

    Err(CliError::new("submit cancelled")
        .reason(format!(
            "the stack must be rebased onto trunk `{trunk}` before it can be submitted"
        ))
        .resolution("run `forklift sync`, then rerun `forklift submit`")
        .into())
}

pub(crate) async fn validate_submit_bases(
    runner: &impl CommandRunner,
    config: &AppConfig,
    stack: &[ResolvedChange],
    frozen_dependencies: &[FrozenDependency],
) -> Result<()> {
    let Some(root) = stack.first() else {
        bail!("cannot submit an empty stack");
    };
    let (base_label, base_tip) = match frozen_dependencies.last() {
        Some(dependency) => (
            dependency.bookmark.name.clone(),
            dependency.change.commit_id.clone(),
        ),
        None => (
            config.trunk.clone(),
            resolve_single_rev(runner, &config.trunk).await?,
        ),
    };
    // The stack revset excludes empty commits (`~empty()`), so the resolved base
    // can sit one or more empty spacer commits below the stack root (e.g. a
    // leftover `jj new`). Validate by ancestry — the base must be an ancestor of
    // the root — rather than by strict parent adjacency, which would spuriously
    // fail on those skipped empties. The revset guarantees every *non-empty*
    // commit between the base and the root is already part of the stack, so
    // anything sitting between them here is empty and harmless to submit.
    let root_merge_base = merge_base(runner, &root.commit_id, &base_tip).await?;
    if root_merge_base != base_tip {
        bail!(CliError::new(format!(
            "submit base validation failed for {} ({}): base `{}` ({}) is not an ancestor of the stack root (merge-base is {})",
            short_change_id(&root.change_id),
            short_commit_id(&root.commit_id),
            base_label,
            short_commit_id(&base_tip),
            short_commit_id(&root_merge_base),
        ))
        .resolution(format!(
            "run `forklift sync` to rebase the stack onto `{base_label}`"
        )));
    }

    // The stack revset excludes empty commits, so an empty spacer between the
    // base and the stack root rides along in the pushed range without being a
    // stack member. jj refuses to push a commit with no description, so an
    // *undescribed* spacer would abort mid-push (`push-refs`) after earlier
    // bookmarks already landed. Catch it here, before any mutation. Empty
    // *described* spacers push fine and are left alone.
    let undescribed_spacers = list_commit_ids(
        runner,
        &format!(
            "{base}..{root} & ~{root} & description(exact:\"\")",
            base = base_tip,
            root = root.commit_id
        ),
    )
    .await?;
    if !undescribed_spacers.is_empty() {
        let revs = undescribed_spacers
            .iter()
            .map(|id| short_commit_id(id))
            .collect::<Vec<_>>()
            .join(" ");
        let noun = if undescribed_spacers.len() == 1 {
            "an empty commit with no description sits"
        } else {
            "empty commits with no description sit"
        };
        bail!(
            CliError::new(format!(
                "{noun} between base `{base_label}` and the stack root; jj cannot push it"
            ))
            .resolution(format!("run `jj abandon {revs}` to drop the empty spacer"))
        );
    }

    for pair in stack.windows(2) {
        let parent = &pair[0];
        let child = &pair[1];
        if selected_parent(child, &HashSet::from([parent.commit_id.as_str()]))
            != Some(parent.commit_id.as_str())
        {
            bail!(CliError::new(format!(
                "submit parent validation failed for {} ({}): expected parent commit {} from previous change {}",
                short_change_id(&child.change_id),
                short_commit_id(&child.commit_id),
                short_commit_id(&parent.commit_id),
                short_change_id(&parent.change_id)
            )));
        }

        let merge_base = merge_base(runner, &child.commit_id, &parent.commit_id).await?;
        if merge_base != parent.commit_id {
            bail!(CliError::new(format!(
                "submit base validation failed for {} ({}): merge-base with parent change {} ({}) is {}",
                short_change_id(&child.change_id),
                short_commit_id(&child.commit_id),
                short_change_id(&parent.change_id),
                short_commit_id(&parent.commit_id),
                short_commit_id(&merge_base)
            )));
        }
    }

    Ok(())
}

/// Rejects a stack up front if any change has no description. jj refuses to push
/// an undescribed commit, but it only errors at push time — partway through the
/// push loop, after earlier branches are already on the remote. Catching it here
/// (before `push-refs`) fails cleanly with zero side effects.
pub(crate) fn validate_submit_descriptions(stack: &[ResolvedChange]) -> Result<()> {
    let undescribed = stack
        .iter()
        .filter(|change| change.title.trim().is_empty())
        .collect::<Vec<_>>();
    if undescribed.is_empty() {
        return Ok(());
    }

    let revs = undescribed
        .iter()
        .map(|change| short_change_id(&change.change_id))
        .collect::<Vec<_>>()
        .join(" ");
    let count = undescribed.len();
    let noun = if count == 1 {
        "change has"
    } else {
        "changes have"
    };
    bail!(
        CliError::new(format!("{count} {noun} no description"))
            .resolution(format!("run `jj describe -r {revs}`"))
    );
}

/// Lists the commit ids matching `revset`, in `jj log` order. Returns an empty
/// vec when nothing matches (unlike [`resolve_single_rev`], which requires
/// exactly one).
#[tracing::instrument(level = "trace", skip_all, fields(revset = %revset))]
pub(crate) async fn list_commit_ids(runner: &impl CommandRunner, revset: &str) -> Result<Vec<String>> {
    let template = "commit_id ++ \"\\n\"";
    let args = ["log", "--no-graph", "-r", revset, "-T", template];
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

#[tracing::instrument(level = "trace", skip_all, fields(left = %left, right = %right))]
pub(crate) async fn merge_base(runner: &impl CommandRunner, left: &str, right: &str) -> Result<String> {
    git_run_required(runner, &["merge-base", left, right])
        .await
        .with_context(|| format!("validate merge base between {left} and {right}"))
}

#[tracing::instrument(skip_all, fields(remote = %remote, branch = %branch))]
pub(crate) async fn remote_head_oid(
    runner: &impl CommandRunner,
    remote: &str,
    branch: &str,
) -> Result<Option<String>> {
    let args = ["ls-remote", "--heads", remote, branch];
    let output = git_run(runner, &args).await?;
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
        _ => bail!(CliError::new(format!(
            "remote head lookup for `{branch}` returned {} refs, expected one",
            lines.len()
        ))),
    }
}

/// Re-pin every plan to its change's current visible commit. Planning pinned
/// commit ids, but a working-copy snapshot triggered by any jj command since
/// then (absorbing concurrent file edits) may have rewritten stack commits;
/// pushing the old, now-hidden ids would resurrect them and leave the changes
/// divergent. A change that was benignly rewritten is re-pinned (and pushed)
/// with a warning; a change that disappeared or became divergent aborts the
/// submit before anything is pushed.
pub(crate) async fn reconcile_plans_with_current_commits(
    runner: &impl CommandRunner,
    plans: &mut [SubmitPlan],
) -> Result<()> {
    for plan in plans.iter_mut() {
        let current = list_commit_ids(runner, &format!("change_id({})", plan.change.change_id)).await?;
        // A plan whose commit is still visible is fine as-is — including the
        // deliberately selected copy of an already-divergent change.
        if current.iter().any(|commit| *commit == plan.change.commit_id) {
            continue;
        }
        match current.as_slice() {
            [current] => {
                ui_warn!(
                    "change {} was rewritten from {} to {} while submit was running (e.g. a working-copy snapshot absorbed file edits); submitting the current commit",
                    short_change_id(&plan.change.change_id),
                    short_commit_id(&plan.change.commit_id),
                    short_commit_id(current)
                );
                plan.change.commit_id = current.clone();
                plan.push_needed = true;
                if plan.existing_pr.is_some() {
                    plan.pr_update_needed = true;
                }
            }
            [] => {
                bail!(
                    CliError::new(format!(
                        "change {} disappeared while submit was running",
                        short_change_id(&plan.change.change_id)
                    ))
                    .resolution("rerun `forklift submit` to replan against the current stack")
                );
            }
            _ => {
                bail!(
                    CliError::new(format!(
                        "change {} became divergent while submit was running",
                        short_change_id(&plan.change.change_id)
                    ))
                    .resolution(
                        "abandon the unwanted copy (`jj abandon <commit>`), then rerun `forklift submit`",
                    )
                );
            }
        }
    }
    Ok(())
}

pub(crate) async fn push_changed_heads(
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
    // Re-point every changed bookmark first. These are local jj mutations that
    // take the repo's working-copy lock, so they must run sequentially.
    for plan in &changed {
        set_submit_bookmark(runner, plan, diagnostics).await?;
    }
    // Push the whole stack in a single `jj git push`, so it rides one remote
    // negotiation instead of one round-trip per head.
    push_submit_bookmarks(runner, config, &changed, diagnostics).await?;
    // Verify the remote heads concurrently; these are independent `git ls-remote`
    // reads with no shared state.
    let verifications = stream::iter(
        changed
            .iter()
            .map(|plan| verify_submit_bookmark_pushed(runner, config, plan)),
    )
    .buffer_unordered(NETWORK_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    for result in verifications {
        result?;
    }
    if let Some(progress) = progress {
        progress.set_position(changed.len() as u64);
        ui_finish_progress_bar(progress);
    }

    Ok(())
}

pub(crate) async fn verify_submit_bookmark_pushed(
    runner: &impl CommandRunner,
    config: &AppConfig,
    plan: &SubmitPlan,
) -> Result<()> {
    let remote_head = remote_head_oid(runner, &config.remote, &plan.head_branch).await?;
    if remote_head.as_deref() != Some(plan.change.commit_id.as_str()) {
        bail!(
            "phase=push-refs object=bookmark:{} error=push completed but remote branch is {}; expected {}. safe-next-command=`forklift submit --dry-run`",
            plan.head_branch,
            remote_head
                .as_deref()
                .map(short_commit_id)
                .unwrap_or("<absent>"),
            short_commit_id(&plan.change.commit_id)
        );
    }

    Ok(())
}

pub(crate) async fn set_submit_bookmark(
    runner: &impl CommandRunner,
    plan: &SubmitPlan,
    diagnostics: Diagnostics,
) -> Result<()> {
    // --ignore-working-copy: the plan was re-pinned to current commit ids
    // after the last snapshot; snapshotting again here could rewrite the
    // commit mid-push and turn this set into a hidden-id resurrection.
    let args = [
        "bookmark",
        "set",
        "--ignore-working-copy",
        "--allow-backwards",
        plan.head_branch.as_str(),
        "-r",
        plan.change.commit_id.as_str(),
    ];
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args).await?;
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

pub(crate) async fn push_submit_bookmarks(
    runner: &impl CommandRunner,
    config: &AppConfig,
    plans: &[&SubmitPlan],
    diagnostics: Diagnostics,
) -> Result<()> {
    let mut args = vec![
        "git",
        "push",
        "--ignore-working-copy",
        "--remote",
        config.remote.as_str(),
    ];
    for plan in plans {
        args.push("--bookmark");
        args.push(plan.head_branch.as_str());
    }
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "phase=push-refs object=bookmarks failed-command=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    Ok(())
}
