use super::super::*;
use super::*;

/// Sync every tracked stack in one pass: each distinct head among the working
/// copy's stack and all local `<prefix>/*` head bookmarks is synced through the
/// same per-stack pipeline as a targeted sync. This is the `forklift` analogue
/// of `gt sync` operating over all tracked branches rather than just the one
/// containing `@`.
#[tracing::instrument(skip_all)]
pub(crate) async fn sync_all_stacks(
    runner: &impl CommandRunner,
    config: &AppConfig,
    submit: bool,
    yes: bool,
    diagnostics: Diagnostics,
) -> Result<SyncAllSummary> {
    let heads = tracked_stack_heads(runner, config)
        .await
        .map_err(|error| phase_error("resolve-stacks", "tracked stacks", error))?;

    // No tracked stack sits above trunk (e.g. an empty `@` on trunk). Fall back
    // to the default single-stack sync, which still advances trunk and finishes
    // cleanly instead of erroring on an empty stack.
    if heads.is_empty() {
        let summary = sync_stack(
            runner,
            config,
            DEFAULT_STACK_REVSET,
            None,
            submit,
            yes,
            diagnostics,
        )
        .await?;
        let mut aggregate = SyncAllSummary::default();
        aggregate.add(&summary);
        return Ok(aggregate);
    }

    // Best-effort only matters when there is more than one stack: a single bad
    // stack should surface its real error directly (rich refusal/divergence
    // guidance), while with several stacks one failure must not abort the rest.
    let mut aggregate = SyncAllSummary::default();
    let total = heads.len();
    let best_effort = total > 1;
    for (index, head) in heads.iter().enumerate() {
        ui_progress(
            "Syncing",
            &format!("stack {}/{} ({})", index + 1, total, short_change_id(head)),
        );
        // Anchor the per-stack revset at this head's change id, which follows
        // the rewrite when sync rebases the stack (a commit id would go stale
        // mid-sync, breaking the submit phase). A divergent change id fails to
        // resolve here; jj's own error then explains how to abandon the extra
        // copy. We don't try to auto-resolve divergence.
        let revset = merge_revset_for_target(head);
        match sync_stack(runner, config, &revset, None, submit, yes, diagnostics).await {
            Ok(summary) => {
                aggregate.add(&summary);
                aggregate.stacks += 1;
            }
            Err(error) if best_effort => {
                aggregate.failed += 1;
                let diagnostic = diagnostic_from_error(&error);
                ui_warn!(
                    "stack {} failed to sync: {}",
                    short_change_id(head),
                    diagnostic.message
                );
                // Surface the underlying cause (e.g. jj's divergence hints with
                // the change offsets and `jj abandon` command) indented beneath.
                if let Some(reason) = &diagnostic.reason {
                    for line in reason.lines() {
                        ui_detail_line(line);
                    }
                }
                if let Some(resolution) = &diagnostic.resolution {
                    ui_detail_line(&format!("resolution: {resolution}"));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(aggregate)
}

/// Change ids of the head of every tracked stack: the maximal mutable,
/// non-empty owned commits reachable from the working copy or any local
/// `<prefix>/*` bookmark. Frozen dependency bookmarks are intentionally excluded
/// — they are immutable inputs handled within each owning stack's sync. Change
/// ids (not commit ids) anchor each stack so the revset survives the rebase that
/// sync performs on it. Duplicates (e.g. a divergent change surfacing twice) are
/// removed so each stack is attempted once.
#[tracing::instrument(skip_all)]
pub(crate) async fn tracked_stack_heads(
    runner: &impl CommandRunner,
    config: &AppConfig,
) -> Result<Vec<String>> {
    let prefix = config.branch_prefix.trim_end_matches('/');
    let revset = format!(
        "heads((trunk()..(@ | bookmarks(glob:'{prefix}/*'))) & ~empty() & ~::(immutable_heads() | root()))"
    );
    let template = "json(change_id) ++ \"\\n\"";
    let args = ["log", "--no-graph", "-r", &revset, "-T", template];
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    let mut heads = Vec::new();
    let mut seen = HashSet::new();
    for line in output.stdout.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let change_id = serde_json::from_str::<String>(line)
            .with_context(|| format!("parse change id from `{line}`"))?;
        if seen.insert(change_id.clone()) {
            heads.push(change_id);
        }
    }
    Ok(heads)
}

pub(crate) async fn sync_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    revset: &str,
    target: Option<&MergeTarget>,
    submit: bool,
    yes: bool,
    diagnostics: Diagnostics,
) -> Result<SyncSummary> {
    diagnostics.phase("sync-fetch");
    fetch_remote(runner, config, diagnostics)
        .await
        .map_err(|error| phase_error("sync-fetch", &config.remote, error))?;

    // Remove stack branches whose commits already landed in trunk (e.g. merged
    // by a prior `forklift merge` or directly on GitHub). Done before resolving
    // the stack so it still runs when no owned stack remains after a merge.
    let cleaned_branches = cleanup_landed_branches(runner, config, diagnostics)
        .await
        .map_err(|error| phase_error("cleanup-merged", "branches", error))?;

    diagnostics.phase("resolve-stack");
    resolve_single_rev(runner, "trunk()")
        .await
        .map_err(|error| phase_error("resolve-stack", "trunk()", error))?;
    let frozen_bookmarks = frozen_bookmarks(runner)
        .await
        .map_err(|error| phase_error("resolve-stack", "frozen-bookmarks", error))?;
    let mut stack = resolve_stack(runner, revset)
        .await
        .map_err(|error| phase_error("resolve-stack", revset, error))?;
    let pruned_duplicate_commits =
        prune_landed_duplicate_changes(runner, config, &stack, diagnostics)
            .await
            .map_err(|error| phase_error("resolve-stack", revset, error))?;
    let pruned_duplicates = pruned_duplicate_commits.len();
    if pruned_duplicates > 0 {
        if diagnostics.dry_run {
            let pruned = pruned_duplicate_commits
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>();
            stack.retain(|change| !pruned.contains(change.commit_id.as_str()));
        } else {
            stack = resolve_stack(runner, revset)
                .await
                .map_err(|error| phase_error("resolve-stack", revset, error))?;
        }
    }
    // Nothing left to sync (e.g. the whole stack just merged). Move trunk to the
    // fetched remote tip and finish, reporting any branches we cleaned up rather
    // than failing on the empty stack.
    if stack.is_empty() && frozen_bookmarks.is_empty() {
        diagnostics.phase("move-trunk");
        move_trunk_to_remote(runner, config, diagnostics)
            .await
            .map_err(|error| phase_error("move-trunk", &config.trunk, error))?;
        carry_empty_working_copy_to_trunk(runner, config, diagnostics)
            .await
            .map_err(|error| phase_error("carry-working-copy", "@", error))?;
        return Ok(SyncSummary {
            rebased_roots: 0,
            submit_ran: false,
            cleaned_branches,
            pruned_duplicates,
            conflicts: 0,
        });
    }
    let stack_resolution = if stack.is_empty() {
        resolve_purely_frozen_stack(runner, frozen_bookmarks).await
    } else {
        match validate_stack_shape(&stack, revset) {
            Ok(()) => resolve_stack_resolution(runner, stack, frozen_bookmarks).await,
            Err(error) => Err(error),
        }
    }
    .map_err(|error| phase_error("resolve-stack", revset, error))?;
    if diagnostics.verbose {
        print_stack(&stack_resolution.owned);
    }
    let submit_revset = if target.is_some() {
        let target_change = stack_resolution.owned.last().with_context(|| {
            format!("phase=resolve-stack object={revset} targeted sync resolved no owned changes")
        })?;
        merge_revset_for_target(&target_change.change_id)
    } else {
        revset.to_owned()
    };

    diagnostics.phase("sync-frozen");
    let frozen_refresh =
        sync_refresh_frozen_dependencies(runner, config, &stack_resolution, diagnostics)
            .await
            .map_err(|error| phase_error("sync-frozen", "frozen dependencies", error))?;

    diagnostics.phase("move-trunk");
    move_trunk_to_remote(runner, config, diagnostics)
        .await
        .map_err(|error| phase_error("move-trunk", &config.trunk, error))?;
    carry_empty_working_copy_to_trunk(runner, config, diagnostics)
        .await
        .map_err(|error| phase_error("carry-working-copy", "@", error))?;

    if stack_resolution.owned.is_empty() {
        return Ok(SyncSummary {
            rebased_roots: 0,
            submit_ran: false,
            cleaned_branches,
            pruned_duplicates,
            conflicts: 0,
        });
    }

    diagnostics.phase("rebase-stack");
    let destination = if frozen_refresh.active_dependencies {
        sync_rebase_destination(config, &stack_resolution)
    } else {
        RebaseDestination::Trunk(config.trunk.clone())
    };
    let rebased_roots = if target.is_some() {
        rebase_selected_stack(
            runner,
            revset,
            &stack_resolution.owned,
            destination,
            diagnostics,
        )
        .await
    } else {
        rebase_stack_roots(runner, &stack_resolution.owned, destination, diagnostics).await
    }
    .map_err(|error| phase_error("rebase-stack", revset, error))?;

    let conflicts = if diagnostics.dry_run {
        0
    } else {
        report_sync_conflicts(runner, &submit_revset)
            .await
            .map_err(|error| phase_error("resolve-stack", &submit_revset, error))?
    };

    let prompted_submit = if submit {
        false
    } else {
        prompt_submit_after_sync(rebased_roots, conflicts, diagnostics.dry_run)?
    };
    let should_submit = submit || prompted_submit;
    let submit_yes = yes || prompted_submit;
    if !should_submit {
        return Ok(SyncSummary {
            rebased_roots,
            submit_ran: false,
            cleaned_branches,
            pruned_duplicates,
            conflicts,
        });
    }

    if diagnostics.dry_run {
        diagnostics.plan_line("- run submit after sync");
        return Ok(SyncSummary {
            rebased_roots,
            submit_ran: true,
            cleaned_branches,
            pruned_duplicates,
            conflicts,
        });
    }

    diagnostics.phase("sync-submit");
    let mut context = resolve_stack_context(runner, &submit_revset)
        .await
        .map_err(|error| phase_error("sync-submit", &submit_revset, error))?;
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
        submit_yes,
        "forklift sync --submit --yes",
        diagnostics,
    )
    .await
    .map_err(|error| phase_error("sync-submit", "submit", error))?;

    Ok(SyncSummary {
        rebased_roots,
        submit_ran: true,
        cleaned_branches,
        pruned_duplicates,
        conflicts,
    })
}

pub(crate) async fn prune_landed_duplicate_changes(
    runner: &impl CommandRunner,
    config: &AppConfig,
    stack: &[ResolvedChange],
    diagnostics: Diagnostics,
) -> Result<Vec<String>> {
    let mut landed_duplicates = Vec::new();
    for change in stack {
        let landed_commits = landed_change_commits(runner, config, &change.change_id).await?;
        if landed_commits
            .iter()
            .any(|commit_id| commit_id != &change.commit_id)
        {
            landed_duplicates.push(change);
        }
    }
    if landed_duplicates.is_empty() {
        return Ok(Vec::new());
    }

    for change in &landed_duplicates {
        ui_warn_line(&format!(
            "change {} ({}) already exists on `{}@{}`; pruning local duplicate",
            short_change_id(&change.change_id),
            short_commit_id(&change.commit_id),
            config.trunk,
            config.remote
        ));
    }

    let mut args = vec!["abandon"];
    for change in &landed_duplicates {
        args.push(change.commit_id.as_str());
    }

    if diagnostics.dry_run {
        diagnostics.plan_line(&format!("- {}", display_command("jj", &args)));
        return Ok(landed_duplicates
            .iter()
            .map(|change| change.commit_id.clone())
            .collect());
    }

    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    Ok(landed_duplicates
        .iter()
        .map(|change| change.commit_id.clone())
        .collect())
}

async fn landed_change_commits(
    runner: &impl CommandRunner,
    config: &AppConfig,
    change_id: &str,
) -> Result<Vec<String>> {
    let revset = format!("::{} & change_id({change_id})", remote_jj_ref(config));
    let args = [
        "log",
        "--no-graph",
        "-r",
        revset.as_str(),
        "-T",
        "commit_id ++ \"\\n\"",
    ];
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

pub(crate) fn prompt_submit_after_sync(
    rebased_roots: usize,
    conflicts: usize,
    dry_run: bool,
) -> Result<bool> {
    if dry_run || rebased_roots == 0 || conflicts > 0 || !io::stdin().is_terminal() {
        return Ok(false);
    }

    eprint!("Submit updated PRs now? [y/N] ");
    io::stderr().flush().context("flush sync submit prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read sync submit prompt")?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

#[tracing::instrument(skip_all, fields(revset = %revset))]
pub(crate) async fn report_sync_conflicts(runner: &impl CommandRunner, revset: &str) -> Result<usize> {
    let stack = resolve_stack(runner, revset).await?;
    let conflicts = stack
        .iter()
        .filter(|change| change.conflict)
        .collect::<Vec<_>>();
    for change in &conflicts {
        ui_conflict(&format!(
            "{} has unresolved merge conflicts",
            change.change_id
        ));
    }
    Ok(conflicts.len())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncFrozenRefresh {
    github: Option<GitHubContext>,
    active_dependencies: bool,
}

pub(crate) async fn sync_refresh_frozen_dependencies(
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
        .await
        .context("resolve GitHub repository for frozen dependencies")?;
    let mut prs = Vec::new();
    for dependency in &stack_resolution.frozen_dependencies {
        let pr = fetch_pr_by_number(
            runner,
            &github,
            "sync-frozen",
            dependency.bookmark.pr_number,
        )
        .await?;
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
        .await
        .map_err(|error| anyhow!("fetch frozen dependency branches: {error}"))?;
    update_get_frozen_bookmarks(runner, &prs, diagnostics).await?;
    Ok(SyncFrozenRefresh {
        github: Some(github),
        active_dependencies: true,
    })
}

pub(crate) fn validate_sync_frozen_pr_stack(
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
        bail!(CliError::new(format!(
            "partially merged frozen dependency stack: {merged_count}/{} dependencies are merged",
            prs.len()
        ))
        .reason("automatic recovery is not supported yet"));
    }

    for (index, (dependency, pr)) in dependencies.iter().zip(prs).enumerate() {
        validate_sync_frozen_pr_metadata(config, github, dependency, pr)?;
        if index == 0 {
            if pr.base_ref_name != config.trunk
                && !frozen_dependency_base_matches_local_parent(dependency, pr)
            {
                let local_parents = dependency
                    .change
                    .parent_ids
                    .iter()
                    .map(|parent| short_commit_id(parent))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(CliError::new(format!(
                    "unexpected retarget for frozen dependency `{}` PR #{}: base branch is `{}` at {}, expected trunk `{}` or local parent {}",
                    dependency.bookmark.name,
                    pr.number,
                    pr.base_ref_name,
                    short_commit_id(&pr.base_ref_oid),
                    config.trunk,
                    local_parents
                )));
            }
            continue;
        }

        let previous = &prs[index - 1];
        if pr.base_ref_name != previous.head_ref_name {
            bail!(CliError::new(format!(
                "unexpected retarget for frozen dependency `{}` PR #{}: base branch is `{}`, expected previous frozen PR #{} head branch `{}`",
                dependency.bookmark.name,
                pr.number,
                pr.base_ref_name,
                previous.number,
                previous.head_ref_name
            )));
        }
        if pr.base_ref_oid != previous.head_ref_oid {
            bail!(CliError::new(format!(
                "unexpected retarget for frozen dependency `{}` PR #{}: base SHA is {}, expected previous frozen PR #{} head SHA {}",
                dependency.bookmark.name,
                pr.number,
                short_commit_id(&pr.base_ref_oid),
                previous.number,
                short_commit_id(&previous.head_ref_oid)
            )));
        }
    }

    Ok(true)
}

pub(crate) fn frozen_dependency_base_matches_local_parent(
    dependency: &FrozenDependency,
    pr: &GhPr,
) -> bool {
    dependency
        .change
        .parent_ids
        .iter()
        .any(|parent| parent == &pr.base_ref_oid)
}

#[tracing::instrument(skip_all, fields(pr = pr.number))]
pub(crate) fn validate_sync_frozen_pr_metadata(
    _config: &AppConfig,
    github: &GitHubContext,
    dependency: &FrozenDependency,
    pr: &GhPr,
) -> Result<()> {
    validate_get_pr_metadata(github, pr)?;
    if pr.state.eq_ignore_ascii_case("CLOSED") {
        bail!(
            CliError::new(format!(
                "closed-unmerged frozen dependency `{}` points to PR #{}",
                dependency.bookmark.name, pr.number
            ))
            .reason("sync cannot recover a closed PR that was not merged")
        );
    }
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(CliError::new(format!(
            "frozen dependency `{}` PR #{} is `{}`, expected OPEN",
            dependency.bookmark.name, pr.number, pr.state
        )));
    }

    let head_repo = get_pr_repo(pr, "head")?;
    let base_repo = get_pr_repo(pr, "base")?;
    if head_repo.name_with_owner != github.repo {
        bail!(
            CliError::new(format!(
                "frozen dependency `{}` PR #{} is fork-backed from `{}`",
                dependency.bookmark.name, pr.number, head_repo.name_with_owner
            ))
            .resolution("sync only supports same-repo frozen dependencies")
        );
    }
    if base_repo.name_with_owner != github.repo {
        bail!(CliError::new(format!(
            "frozen dependency `{}` PR #{} has base repo `{}`, expected `{}`",
            dependency.bookmark.name, pr.number, base_repo.name_with_owner, github.repo
        )));
    }

    Ok(())
}

/// A fresh `jj new <trunk>` working copy is empty, so the stack revset never
/// includes it and the stack rebase leaves it stranded on the old trunk
/// commit when sync moves trunk forward. Carry it onto the new tip.
/// Conservative: only a conflict-free empty working copy with a single
/// parent that is now strictly behind trunk, and no children, is moved — an
/// empty rev sitting on a stack change already followed the stack rebase.
pub(crate) async fn carry_empty_working_copy_to_trunk(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let info = run_required(
        runner,
        "jj",
        &[
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            r#"if(empty, "true", "false") ++ "\t" ++ if(conflict, "true", "false") ++ "\t" ++ parents.map(|c| c.commit_id()).join(",")"#,
        ],
    )
    .await?;
    let mut fields = info.split('\t');
    let empty = fields.next() == Some("true");
    let conflict = fields.next() == Some("true");
    let parents = fields
        .next()
        .unwrap_or_default()
        .split(',')
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();
    let [parent] = parents.as_slice() else {
        return Ok(());
    };
    if !empty || conflict {
        return Ok(());
    }
    let trunk_tip = resolve_single_rev(runner, &config.trunk).await?;
    if *parent == trunk_tip {
        return Ok(());
    }
    if list_commit_ids(runner, &format!("{parent} & ::{trunk_tip}")).await?.is_empty() {
        return Ok(());
    }
    if !list_commit_ids(runner, "children(@)").await?.is_empty() {
        return Ok(());
    }

    diagnostics.phase("carry-working-copy");
    let args = ["rebase", "-r", "@", "-d", trunk_tip.as_str()];
    if diagnostics.dry_run {
        diagnostics.plan_line(&format!(
            "- move the empty working copy onto trunk `{}`",
            config.trunk
        ));
        return Ok(());
    }
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

pub(crate) async fn fetch_remote(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = ["git", "fetch", "--remote", config.remote.as_str()];
    run_fetch_remote_command(runner, config, diagnostics, &args).await
}

pub(crate) async fn fetch_remote_preserving_local_commits(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = [
        "git",
        "fetch",
        "--config",
        "git.abandon-unreachable-commits=false",
        "--remote",
        config.remote.as_str(),
    ];
    run_fetch_remote_command(runner, config, diagnostics, &args).await
}

async fn run_fetch_remote_command(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
    args: &[&str],
) -> Result<()> {
    if diagnostics.dry_run {
        diagnostics.plan_line(&format!("- {}", display_command("jj", args)));
        return Ok(());
    }

    diagnostics.command("jj", args);
    let output = runner.run("jj", args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", args),
            output.stderr.trim()
        );
    }

    // The fetch is the gate every later trunk-movement and merge step trusts.
    // Verify it actually produced a resolvable remote trunk bookmark, so a wrong
    // remote name or a fetch that exits 0 without updating refs fails loudly here
    // instead of later as a confusing trunk-movement error.
    let remote_jj_ref = remote_jj_ref(config);
    jj_trunk_remote_commit(runner, config).await.map_err(|error| {
        anyhow!(
            "`{}` reported success but `{remote_jj_ref}` is not resolvable; check {CONFIG_PREFIX}.remote and {CONFIG_PREFIX}.trunk: {error}",
            display_command("jj", args)
        )
    })?;

    Ok(())
}

/// Resolve the commit id of the remote trunk bookmark (`<trunk>@<remote>`) as jj
/// sees it. This is the authority jj uses when it moves the local trunk bookmark
/// and rebases, so trunk movement must be based on it rather than the colocated
/// git ref, which can lag in a non-colocated repo.
pub(crate) async fn jj_trunk_remote_commit(
    runner: &impl CommandRunner,
    config: &AppConfig,
) -> Result<String> {
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
    .await
    .with_context(|| format!("resolve jj remote trunk bookmark `{remote_jj_ref}`"))
}

pub(crate) async fn move_trunk_to_remote(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let local = git_rev_parse(runner, &config.trunk).await?;
    let remote_git_ref = remote_git_ref(config);
    let remote = git_rev_parse(runner, &remote_git_ref).await?;

    // jj moves the local trunk bookmark and rebases against its own view of the
    // remote bookmark (`<trunk>@<remote>`). In a non-colocated repo
    // `git rev-parse <remote>/<trunk>` can lag that view, which would make the
    // local==remote check below a false positive and silently skip trunk
    // movement. Require the two views to agree so we never base trunk movement on
    // a stale git ref.
    let remote_jj = jj_trunk_remote_commit(runner, config).await?;
    if remote_jj != remote {
        bail!(
            CliError::new(format!(
                "remote trunk views disagree: git `{}` is {}, jj `{}` is {}",
                remote_git_ref,
                short_commit_id(&remote),
                remote_jj_ref(config),
                short_commit_id(&remote_jj)
            ))
            .resolution(format!(
                "run `jj git export` (or verify {CONFIG_PREFIX}.remote)"
            ))
        );
    }

    if local == remote {
        diagnostics.plan_line(&format!("- leave trunk `{}` at {}", config.trunk, local));
        return Ok(());
    }

    let stranded_recovery = if trunk_can_fast_forward(runner, &local, &remote).await? {
        false
    } else {
        // A merge push that failed after moving the trunk bookmark (e.g. the
        // remote rejected it) leaves local trunk sitting on stack commits the
        // remote never received. If every commit trunk would abandon is still
        // covered by another bookmark, moving trunk back to the remote loses
        // nothing — the covered commits are the stack and get rebased right
        // after. Anything uncovered is genuine divergence and still fails.
        // The cover set must be built from bookmark *names* — `bookmarks() ~
        // bookmarks(exact:<trunk>)` is a commit-set difference, and the stack
        // top carries both trunk and its stack bookmark, so it would drop out.
        let covers = local_bookmark_names(runner)
            .await?
            .into_iter()
            .filter(|name| name != &config.trunk)
            .map(|name| format!("bookmarks(exact:\"{name}\")"))
            .collect::<Vec<_>>()
            .join(" | ");
        let uncovered_revset = if covers.is_empty() {
            format!("::{local} ~ ::{remote}")
        } else {
            format!("::{local} ~ ::{remote} ~ ::({covers})")
        };
        let uncovered = list_commit_ids(runner, &uncovered_revset).await?;
        if !uncovered.is_empty() {
            bail!(CliError::new(format!(
                "trunk `{}` cannot fast-forward to `{}`: local commit {}, remote commit {}",
                config.trunk,
                remote_git_ref,
                short_commit_id(&local),
                short_commit_id(&remote)
            )));
        }
        true
    };

    let remote_jj_ref = remote_jj_ref(config);
    let mut args = vec!["bookmark", "set", config.trunk.as_str()];
    if stranded_recovery {
        args.push("--allow-backwards");
    }
    args.extend(["-r", remote_jj_ref.as_str()]);

    if diagnostics.dry_run {
        if stranded_recovery {
            diagnostics.plan_line(&format!(
                "- move trunk `{}` back from {} to {} (every stranded commit keeps its own bookmark)",
                config.trunk, local, remote
            ));
        } else {
            diagnostics.plan_line(&format!(
                "- fast-forward trunk `{}` from {} to {}",
                config.trunk, local, remote
            ));
        }
        diagnostics.plan_line(&format!("- {}", display_command("jj", &args)));
        return Ok(());
    }

    if stranded_recovery {
        ui_warn!(
            "local trunk `{}` sits on {} which the remote never received (likely an interrupted merge push); moving it back to {} — every stranded commit keeps its own bookmark",
            config.trunk,
            short_commit_id(&local),
            short_commit_id(&remote)
        );
    }
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

#[tracing::instrument(skip_all, fields(local = %local, remote = %remote))]
pub(crate) async fn trunk_can_fast_forward(
    runner: &impl CommandRunner,
    local: &str,
    remote: &str,
) -> Result<bool> {
    let args = ["merge-base", "--is-ancestor", local, remote];
    Ok(git_run(runner, &args).await?.success)
}
