use super::super::*;
use super::*;

pub(crate) fn sync_stack(
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
        .map_err(|error| phase_error("sync-fetch", &config.remote, error))?;

    // Remove stack branches whose commits already landed in trunk (e.g. merged
    // by a prior `forklift merge` or directly on GitHub). Done before resolving
    // the stack so it still runs when no owned stack remains after a merge.
    let cleaned_branches = cleanup_landed_branches(runner, config, diagnostics)
        .map_err(|error| phase_error("cleanup-merged", "branches", error))?;

    diagnostics.phase("resolve-stack");
    resolve_single_rev(runner, "trunk()")
        .map_err(|error| phase_error("resolve-stack", "trunk()", error))?;
    let frozen_bookmarks = frozen_bookmarks(runner)
        .map_err(|error| phase_error("resolve-stack", "frozen-bookmarks", error))?;
    let stack = resolve_stack(runner, revset)
        .map_err(|error| phase_error("resolve-stack", revset, error))?;
    // Nothing left to sync (e.g. the whole stack just merged). Move trunk to the
    // fetched remote tip and finish, reporting any branches we cleaned up rather
    // than failing on the empty stack.
    if stack.is_empty() && frozen_bookmarks.is_empty() {
        diagnostics.phase("move-trunk");
        move_trunk_to_remote(runner, config, diagnostics)
            .map_err(|error| phase_error("move-trunk", &config.trunk, error))?;
        return Ok(SyncSummary {
            rebased_roots: 0,
            submit_ran: false,
            cleaned_branches,
            conflicts: 0,
        });
    }
    let stack_resolution = if stack.is_empty() {
        resolve_purely_frozen_stack(runner, frozen_bookmarks)
    } else {
        (|| {
            validate_stack_shape(&stack, revset)?;
            resolve_stack_resolution(runner, stack, frozen_bookmarks)
        })()
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
            .map_err(|error| phase_error("sync-frozen", "frozen dependencies", error))?;

    diagnostics.phase("move-trunk");
    move_trunk_to_remote(runner, config, diagnostics)
        .map_err(|error| phase_error("move-trunk", &config.trunk, error))?;

    if stack_resolution.owned.is_empty() {
        return Ok(SyncSummary {
            rebased_roots: 0,
            submit_ran: false,
            cleaned_branches,
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
    } else {
        rebase_stack_roots(runner, &stack_resolution.owned, destination, diagnostics)
    }
    .map_err(|error| phase_error("rebase-stack", revset, error))?;

    let conflicts = if diagnostics.dry_run {
        0
    } else {
        report_sync_conflicts(runner, &submit_revset)
            .map_err(|error| phase_error("resolve-stack", &submit_revset, error))?
    };

    if !submit {
        return Ok(SyncSummary {
            rebased_roots,
            submit_ran: false,
            cleaned_branches,
            conflicts,
        });
    }

    if diagnostics.dry_run {
        diagnostics.plan_line("- run submit after sync");
        return Ok(SyncSummary {
            rebased_roots,
            submit_ran: true,
            cleaned_branches,
            conflicts,
        });
    }

    diagnostics.phase("sync-submit");
    let mut context = resolve_stack_context(runner, &submit_revset)
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
        yes,
        "forklift sync --submit --yes",
        diagnostics,
    )
    .map_err(|error| phase_error("sync-submit", "submit", error))?;

    Ok(SyncSummary {
        rebased_roots,
        submit_ran: true,
        cleaned_branches,
        conflicts,
    })
}

#[tracing::instrument(skip_all, fields(revset = %revset))]
pub(crate) fn report_sync_conflicts(runner: &impl CommandRunner, revset: &str) -> Result<usize> {
    let stack = resolve_stack(runner, revset)?;
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

pub(crate) fn sync_refresh_frozen_dependencies(
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
        .context("resolve GitHub repository for frozen dependencies")?;
    let mut prs = Vec::new();
    for dependency in &stack_resolution.frozen_dependencies {
        let pr = fetch_pr_by_number(
            runner,
            &github,
            "sync-frozen",
            dependency.bookmark.pr_number,
        )?;
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
        .map_err(|error| anyhow!("fetch frozen dependency branches: {error}"))?;
    update_get_frozen_bookmarks(runner, &prs, diagnostics)?;
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
            if pr.base_ref_name != config.trunk {
                bail!(CliError::new(format!(
                    "unexpected retarget for frozen dependency `{}` PR #{}: base branch is `{}`, expected trunk `{}`",
                    dependency.bookmark.name, pr.number, pr.base_ref_name, config.trunk
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

pub(crate) fn fetch_remote(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = ["git", "fetch", "--remote", config.remote.as_str()];
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

    // The fetch is the gate every later trunk-movement and merge step trusts.
    // Verify it actually produced a resolvable remote trunk bookmark, so a wrong
    // remote name or a fetch that exits 0 without updating refs fails loudly here
    // instead of later as a confusing trunk-movement error.
    let remote_jj_ref = remote_jj_ref(config);
    jj_trunk_remote_commit(runner, config).map_err(|error| {
        anyhow!(
            "`{}` reported success but `{remote_jj_ref}` is not resolvable; check {CONFIG_PREFIX}.remote and {CONFIG_PREFIX}.trunk: {error}",
            display_command("jj", &args)
        )
    })?;

    Ok(())
}

/// Resolve the commit id of the remote trunk bookmark (`<trunk>@<remote>`) as jj
/// sees it. This is the authority jj uses when it moves the local trunk bookmark
/// and rebases, so trunk movement must be based on it rather than the colocated
/// git ref, which can lag in a non-colocated repo.
pub(crate) fn jj_trunk_remote_commit(
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
    .with_context(|| format!("resolve jj remote trunk bookmark `{remote_jj_ref}`"))
}

pub(crate) fn move_trunk_to_remote(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<()> {
    let local = git_rev_parse(runner, &config.trunk)?;
    let remote_git_ref = remote_git_ref(config);
    let remote = git_rev_parse(runner, &remote_git_ref)?;

    // jj moves the local trunk bookmark and rebases against its own view of the
    // remote bookmark (`<trunk>@<remote>`). In a non-colocated repo
    // `git rev-parse <remote>/<trunk>` can lag that view, which would make the
    // local==remote check below a false positive and silently skip trunk
    // movement. Require the two views to agree so we never base trunk movement on
    // a stale git ref.
    let remote_jj = jj_trunk_remote_commit(runner, config)?;
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

    ensure_trunk_can_fast_forward(runner, config, &local, &remote)?;
    let remote_jj_ref = remote_jj_ref(config);
    let args = [
        "bookmark",
        "set",
        config.trunk.as_str(),
        "-r",
        remote_jj_ref.as_str(),
    ];

    if diagnostics.dry_run {
        diagnostics.plan_line(&format!(
            "- fast-forward trunk `{}` from {} to {}",
            config.trunk, local, remote
        ));
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

#[tracing::instrument(skip_all, fields(local = %local, remote = %remote))]
pub(crate) fn ensure_trunk_can_fast_forward(
    runner: &impl CommandRunner,
    config: &AppConfig,
    local: &str,
    remote: &str,
) -> Result<()> {
    let remote_ref = remote_git_ref(config);
    let args = ["merge-base", "--is-ancestor", local, remote];
    let output = git_run(runner, &args)?;
    if !output.success {
        bail!(CliError::new(format!(
            "trunk `{}` cannot fast-forward to `{}`: local commit {}, remote commit {}",
            config.trunk,
            remote_ref,
            short_commit_id(local),
            short_commit_id(remote)
        )));
    }

    Ok(())
}
