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
    let context = resolve_optional_status_context(runner, revset)?;
    let store = CacheStore::load_current_best_effort(runner, diagnostics, "status")?;
    let stack_entries = status_stack_entries(
        runner,
        &store,
        &context.github,
        &config.branch_prefix,
        &context.stack,
    )?;
    let stack_log_revset = status_stack_log_revset(&config.trunk, &stack_entries);
    let mut used_head_branches = HashSet::new();
    let mut orphaned_prs: Vec<OrphanedPr> = Vec::new();
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
            &mut orphaned_prs,
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

    for orphan in &orphaned_prs {
        let note = format!(
            "orphaned PR #{} (`{}`) collapsed onto {}; close it or `forklift submit` to clean it up",
            orphan.pr_number,
            orphan.bookmark,
            short_change_id(&orphan.host_change_id)
        );
        bookmark_problems.push(note.clone());
        problems.push(note);
    }

    if let Some((change, owned)) = context.stack.first().zip(owned_prs.first()) {
        match owned.pr_number {
            Some(pr_number) => {
                match fetch_pr_for_merge(runner, &context.github, &change.change_id, pr_number) {
                    Ok(pr) => {
                        if let Err(error) =
                            validate_merge_frozen_dependencies(runner, config, &context, None, &pr)
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

    let suggested_next_command = suggested_status_next_command(
        &stack_entries,
        &owned_prs,
        &frozen_dependencies,
        &merge_blockers,
        &problems,
    );

    Ok(StatusReport {
        repo: context.github.repo,
        username: context.github.username,
        remote: config.remote.clone(),
        trunk: config.trunk.clone(),
        branch_prefix: config.branch_prefix.clone(),
        require_approval: config.require_approval,
        startup_aliases,
        stack_log_revset,
        stack_entries,
        owned_prs,
        frozen_dependencies,
        first_owned_base_branch,
        merge_blockers,
        bookmark_problems,
        problems,
        suggested_next_command,
    })
}

pub(crate) fn resolve_optional_status_context(
    runner: &impl CommandRunner,
    revset: &str,
) -> Result<AppContext> {
    resolve_single_rev(runner, "trunk()")?;
    let frozen_bookmarks = frozen_bookmarks(runner)?;
    let stack = resolve_stack(runner, revset)?;
    let stack_resolution = if stack.is_empty() {
        StackResolution {
            owned: Vec::new(),
            frozen_dependencies: Vec::new(),
        }
    } else {
        validate_stack_shape(runner, &stack, revset)?;
        resolve_stack_resolution(runner, stack, frozen_bookmarks)?
    };
    let github = GitHubContext::resolve(runner)?;

    Ok(AppContext::new(github, stack_resolution))
}

pub(crate) fn status_stack_entries(
    runner: &impl CommandRunner,
    store: &CacheStore,
    github: &GitHubContext,
    branch_prefix: &str,
    current_stack: &[ResolvedChange],
) -> Result<Vec<StatusStackEntry>> {
    let current_change_ids = current_stack
        .iter()
        .map(|change| change.change_id.as_str())
        .collect::<HashSet<_>>();

    let stack_branch_prefix = format!("{}/", branch_prefix.trim_end_matches('/'));
    let open_prs = list_open_prs(runner, github, "status")?
        .into_iter()
        .filter(|pr| {
            pr.head_ref_name.starts_with(&stack_branch_prefix)
                && pr
                    .author
                    .as_ref()
                    .map(|author| author.login.eq_ignore_ascii_case(&github.username))
                    .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    let open_pr_numbers = open_prs.iter().map(|pr| pr.number).collect::<HashSet<_>>();
    let open_pr_by_number = open_prs
        .iter()
        .map(|pr| (pr.number, pr))
        .collect::<HashMap<_, _>>();
    let open_pr_by_head = open_prs
        .iter()
        .map(|pr| (pr.head_ref_oid.as_str(), pr))
        .collect::<HashMap<_, _>>();

    let revset = status_candidate_revset(&open_prs, current_stack);
    if revset.is_empty() {
        return Ok(Vec::new());
    }

    resolve_stack(runner, &revset)
        .with_context(|| format!("resolve unmerged stack entries `{revset}`"))?
        .into_iter()
        .filter_map(|change| {
            let cached = store.get_pr(&github.repo, &change.change_id);
            let open_pr = open_pr_by_head
                .get(change.commit_id.as_str())
                .copied()
                .or_else(|| {
                    cached.and_then(|entry| open_pr_by_number.get(&entry.pr_number).copied())
                });
            let current_stack = current_change_ids.contains(change.change_id.as_str());
            if !current_stack
                && open_pr.is_none()
                && !cached
                    .map(|entry| open_pr_numbers.contains(&entry.pr_number))
                    .unwrap_or(false)
            {
                return None;
            }
            let pr_number = open_pr.map(|pr| pr.number).or_else(|| {
                cached
                    .filter(|entry| open_pr_numbers.contains(&entry.pr_number))
                    .map(|entry| entry.pr_number)
            });
            let head_branch = open_pr
                .map(|pr| pr.head_ref_name.clone())
                .or_else(|| cached.map(|entry| entry.head_branch.clone()));
            Some(Ok(StatusStackEntry {
                current_stack,
                pr_number,
                head_branch,
                change_id: change.change_id,
                commit_id: change.commit_id,
                title: change.title,
            }))
        })
        .collect()
}

pub(crate) fn status_candidate_revset(
    open_prs: &[GhPr],
    current_stack: &[ResolvedChange],
) -> String {
    open_prs
        .iter()
        .map(|pr| format!("present({})", pr.head_ref_oid))
        .chain(current_stack.iter().map(|change| change.commit_id.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(" | ")
}

pub(crate) fn status_stack_log_revset(trunk: &str, entries: &[StatusStackEntry]) -> String {
    std::iter::once(trunk)
        .chain(entries.iter().map(|entry| entry.commit_id.as_str()))
        .collect::<Vec<_>>()
        .join(" | ")
}

pub(crate) fn print_status_stack_log(runner: &impl CommandRunner, revset: &str) -> Result<()> {
    if revset.is_empty() {
        ui_info!("- <none>");
        return Ok(());
    }
    let args = ["log", "--color=always", "-r", revset];
    runner
        .run_interactive("jj", &args)
        .with_context(|| format!("render status stack log `{revset}`"))
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
    stack_entries: &[StatusStackEntry],
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
    if let Some(entry) = stack_entries.last() {
        return format!("jj edit {}", short_change_id(&entry.change_id));
    }
    "forklift merge".to_owned()
}
