use super::super::*;
use super::*;

#[tracing::instrument(skip_all, fields(revset = %revset))]
pub(crate) fn status_report(
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

pub(crate) fn status_alias_state(runner: &impl CommandRunner) -> Result<StatusAliasState> {
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

pub(crate) fn status_frozen_dependencies(
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

pub(crate) fn suggested_status_next_command(
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

pub(crate) fn print_status_report(report: &StatusReport) {
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
