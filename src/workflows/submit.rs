use super::super::*;
use super::*;

pub(crate) fn validate_submit_bookmark_state(
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
        // The bookmark may be stranded on a divergent sibling — the same change
        // resolved to a second visible commit (e.g. a duplicate that didn't
        // carry the bookmark). That is safe: the push phase re-points the
        // bookmark onto the selected copy with `jj bookmark set`. Only bail when
        // the bookmark sits on an unrelated change.
        let local_change_id = jj_ref_change_id(runner, &entry.head_branch).with_context(|| {
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

    let remote = remote_bookmark_status(runner, config, &entry.head_branch)?;
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

pub(crate) fn resolve_submit_head_branch(
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
pub(crate) fn resolve_submit_cached_head_branch(
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
pub(crate) fn discover_existing_pr_from_local_bookmarks(
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
            CliError::new(format!(
                "multiple local `{}` bookmarks at {} have open GitHub PRs for {}",
                config.branch_prefix,
                short_commit_id(&change.commit_id),
                short_change_id(&change.change_id)
            ))
            .reason(
                matches
                    .iter()
                    .map(|(branch, pr)| format!("{} -> PR #{}", branch, pr.pr_number))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        ),
    }
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
pub(crate) fn local_stack_bookmarks_for_change(
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

pub(crate) fn submit_stack(
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
        .map_err(|error| phase_error("validate-submit-bases", "stack", error))?;
    validate_submit_descriptions(&context.stack)
        .map_err(|error| phase_error("validate-submit-bases", "stack", error))?;

    tracing::debug!(phase = "plan-submit", "recovery phase");
    let plan_progress = diagnostics.progress_bar("Planning", "submit", context.stack.len());
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
        diagnostics.plan_line(
            "- live jj/GitHub discovery ran during planning; SQLite cache writes are skipped",
        );
        return Ok(summary);
    }
    print_submit_action_plan(config, &plans);
    confirm_submit_stack(yes, yes_command)?;

    tracing::debug!(phase = "push-refs", "recovery phase");
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

pub(crate) fn resolve_submit_frozen_dependency_entries(
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

pub(crate) fn validate_submit_bases(
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
        bail!(CliError::new(format!(
            "submit base validation failed for {} ({}): jj parent is {}, expected base {} at {}",
            short_change_id(&root.change_id),
            short_commit_id(&root.commit_id),
            short_commit_id(root_parent),
            base_label,
            short_commit_id(expected_parent)
        )));
    }
    let root_merge_base = merge_base(runner, &root.commit_id, merge_base_target)?;
    if root_merge_base != expected_parent {
        bail!(CliError::new(format!(
            "submit base validation failed for {} ({}): merge-base with base `{}` ({}) is {}, expected {}",
            short_change_id(&root.change_id),
            short_commit_id(&root.commit_id),
            base_label,
            short_commit_id(merge_base_target),
            short_commit_id(&root_merge_base),
            short_commit_id(expected_parent)
        )));
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

        let merge_base = merge_base(runner, &child.commit_id, &parent.commit_id)?;
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

#[tracing::instrument(level = "trace", skip_all, fields(left = %left, right = %right))]
pub(crate) fn merge_base(runner: &impl CommandRunner, left: &str, right: &str) -> Result<String> {
    git_run_required(runner, &["merge-base", left, right])
        .with_context(|| format!("validate merge base between {left} and {right}"))
}

#[tracing::instrument(skip_all, fields(remote = %remote, branch = %branch))]
pub(crate) fn remote_head_oid(
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
        _ => bail!(CliError::new(format!(
            "remote head lookup for `{branch}` returned {} refs, expected one",
            lines.len()
        ))),
    }
}

pub(crate) fn push_changed_heads(
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

pub(crate) fn set_submit_bookmark(
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

pub(crate) fn push_submit_bookmark(
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
