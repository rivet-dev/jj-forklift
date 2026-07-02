use super::super::*;

#[tracing::instrument(skip_all, fields(target = target))]
pub(crate) async fn get_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: &str,
    auto_edit: bool,
    diagnostics: Diagnostics,
) -> Result<GetSummary> {
    diagnostics.phase("resolve-github");
    let mut github = GitHubContext::resolve(runner)
        .await
        .map_err(|error| phase_error("resolve-github", "github", error))?;
    let target = parse_get_target(target, &github.repo)?;
    github.repo = target.repo().to_owned();

    diagnostics.phase("resolve-stack-comment");
    let target_pr = resolve_get_target_pr(runner, &github, target).await?;
    let target_pr_number = target_pr.number;
    let comment = latest_stack_comment(runner, &github, target_pr_number, "get").await?;
    let mut pr_numbers = comment
        .as_ref()
        .map(|comment| parse_stack_pr_numbers(&comment.body))
        .unwrap_or_default();
    // The stack comment lists PRs top-to-bottom; downstream resolution expects
    // bottom-to-top topology order (trunk-adjacent first).
    pr_numbers.reverse();
    if pr_numbers.is_empty() {
        pr_numbers.push(target_pr_number);
    }

    diagnostics.phase("resolve-prs");
    let mut prs = Vec::new();
    let progress = diagnostics.progress_bar("Fetching", "pull requests", pr_numbers.len());
    for (index, pr_number) in pr_numbers.into_iter().enumerate() {
        prs.push(fetch_pr_by_number(runner, &github, "get", pr_number).await?);
        if let Some(progress) = &progress {
            progress.set_position((index + 1) as u64);
        }
    }
    if let Some(progress) = progress {
        ui_finish_progress_bar(progress);
    }
    validate_get_pr_stack(config, &github, target_pr_number, &prs)?;

    diagnostics.phase("get-fetch-trunk");
    fetch_remote(runner, config, diagnostics)
        .await
        .map_err(|error| phase_error("get-fetch-trunk", &config.remote, error))?;
    if !diagnostics.dry_run {
        warn_if_get_trunk_is_behind(runner, config)
            .await
            .map_err(|error| phase_error("get-fetch-trunk", &config.trunk, error))?;
    }

    diagnostics.phase("fetch-stack");
    fetch_get_branches(runner, config, &prs, diagnostics).await?;
    let pr_count = prs.len();

    if prs.is_empty() {
        bail!("stack comment did not resolve any PRs");
    }
    // Land the working copy on the PR the user targeted, not the stack tip:
    // `forklift get <pr>` should leave you at that PR's code. The frozen rev is
    // immutable, so we new an empty child above it.
    let target_frozen = frozen_bookmark_name(target_pr_number);
    let next_command = format!("jj new {target_frozen}");
    if diagnostics.dry_run {
        update_get_frozen_bookmarks(runner, &prs, diagnostics).await?;
        if auto_edit {
            diagnostics.plan_line(&format!("- move working copy: {next_command}"));
        } else {
            diagnostics.plan_line(&format!(
                "- skip editing: run `{next_command}` to start editing above the targeted PR"
            ));
        }
        diagnostics.plan_line(
            "- live GitHub discovery ran during planning; SQLite cache writes are skipped",
        );
        return Ok(GetSummary {
            prs: pr_count,
            fetched_branches: pr_count,
            cache_entries: 0,
            edited: false,
        });
    }

    diagnostics.phase("resolve-fetched-heads");
    let changes_by_pr = resolve_get_pr_changes(runner, &prs)
        .await
        .map_err(|error| phase_error("resolve-fetched-heads", "fetched PR heads", error))?;

    diagnostics.phase("freeze-stack");
    update_get_frozen_bookmarks(runner, &prs, diagnostics)
        .await
        .map_err(|error| phase_error("freeze-stack", "frozen bookmarks", error))?;

    diagnostics.phase("write-cache");
    let mut store = CacheStore::load_current_best_effort(runner, diagnostics, "write-cache")
        .await
        .map_err(|error| phase_error("write-cache", "cache", error))?;
    let mut cache_entries = 0;
    for pr in prs {
        let change = changes_by_pr
            .get(&pr.number)
            .with_context(|| format!("missing resolved jj change for PR #{}", pr.number))?;
        store.upsert_pr(&github.repo, &change.change_id, pr.into_cache_entry(None));
        cache_entries += 1;
    }
    if !store.save_best_effort(diagnostics, "write-cache") {
        cache_entries = 0;
    }

    let edited = if auto_edit {
        diagnostics.phase("edit-stack");
        edit_get_stack(runner, &target_frozen, diagnostics)
            .await
            .map_err(|error| phase_error("edit-stack", &target_frozen, error))?;
        true
    } else {
        ui_info!("skip editing: run `{next_command}` to start editing above the targeted PR");
        false
    };

    Ok(GetSummary {
        prs: pr_count,
        fetched_branches: pr_count,
        cache_entries: cache_entries,
        edited,
    })
}

pub(crate) async fn warn_if_get_trunk_is_behind(
    runner: &impl CommandRunner,
    config: &AppConfig,
) -> Result<()> {
    let local = resolve_single_rev(runner, &config.trunk).await?;
    let remote = jj_trunk_remote_commit(runner, config).await?;
    if local != remote && git_commit_is_ancestor(runner, &local, &remote).await? {
        ui_warn!(
            "local trunk `{}` is behind `{}@{}`; run `forklift sync` before editing or submitting this stack",
            config.trunk,
            config.trunk,
            config.remote
        );
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(top = top_frozen))]
pub(crate) async fn edit_get_stack(
    runner: &impl CommandRunner,
    top_frozen: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = ["new", top_frozen];
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
