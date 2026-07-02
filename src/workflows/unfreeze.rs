use super::super::*;
use super::*;

#[tracing::instrument(skip_all, fields(target = target))]
pub(crate) async fn unfreeze_stack(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: &str,
    diagnostics: Diagnostics,
) -> Result<u64> {
    diagnostics.phase("resolve-github");
    let mut github = GitHubContext::resolve(runner)
        .await
        .map_err(|error| phase_error("resolve-github", "github", error))?;
    let target = parse_get_target(target, &github.repo)?;
    github.repo = target.repo().to_owned();

    diagnostics.phase("resolve-target");
    let pr = resolve_get_target_pr(runner, &github, target)
        .await
        .map_err(|error| phase_error("resolve-target", "unfreeze target", error))?;
    validate_unfreeze_pr(config, &github, &pr)
        .map_err(|error| phase_error("resolve-target", format!("PR #{}", pr.number), error))?;
    verify_repo_push_permission(runner, &github)
        .await
        .map_err(|error| phase_error("resolve-target", &github.repo, error))?;

    diagnostics.phase("validate-frozen");
    let frozen_name = frozen_bookmark_name(pr.number);
    let frozen_bookmark = frozen_bookmarks(runner)
        .await
        .map_err(|error| phase_error("validate-frozen", "frozen bookmarks", error))?
        .into_iter()
        .find(|bookmark| bookmark.name == frozen_name);
    let frozen_present = if let Some(frozen_bookmark) = frozen_bookmark {
        if frozen_bookmark.commit_id != pr.head_ref_oid {
            bail!(
                "phase=validate-frozen object={} error=frozen bookmark points at {}, but GitHub PR #{} head is {}; run `forklift sync` first before unfreezing safe-next-command=`forklift sync`",
                frozen_name,
                frozen_bookmark.commit_id,
                pr.number,
                pr.head_ref_oid
            );
        }
        true
    } else {
        ui_warn!(
            "frozen bookmark `{}` is missing; continuing adoption from PR #{} head",
            frozen_name,
            pr.number
        );
        false
    };

    diagnostics.phase("fetch-branch");
    fetch_get_branches(runner, config, std::slice::from_ref(&pr), diagnostics)
        .await
        .map_err(|error| phase_error("fetch-branch", &pr.head_ref_name, error))?;

    diagnostics.phase("track-branch");
    track_remote_bookmark(runner, config, &pr.head_ref_name, diagnostics)
        .await
        .map_err(|error| phase_error("track-branch", &pr.head_ref_name, error))?;

    diagnostics.phase("track-blockers");
    track_untracked_remote_bookmark_blockers(
        runner,
        config,
        &pr.head_ref_oid,
        &pr.head_ref_name,
        diagnostics,
    )
    .await
    .map_err(|error| phase_error("track-blockers", format!("PR #{}", pr.number), error))?;

    diagnostics.phase("remove-frozen");
    if frozen_present {
        delete_bookmark(runner, &frozen_name, diagnostics)
            .await
            .map_err(|error| phase_error("remove-frozen", &frozen_name, error))?;
    }

    diagnostics.phase("verify-mutable");
    if !diagnostics.dry_run {
        verify_unfrozen_revision_mutable(runner, config, &pr.head_ref_oid, pr.number)
            .await
            .map_err(|error| phase_error("verify-mutable", &pr.head_ref_oid, error))?;
    }

    diagnostics.phase("write-cache");
    if diagnostics.dry_run {
        diagnostics.plan_line("- SQLite cache writes are skipped");
    } else {
        let mut store = CacheStore::load_current_best_effort(runner, diagnostics, "write-cache")
            .await
            .map_err(|error| phase_error("write-cache", "cache", error))?;
        let change = resolve_stack(runner, &pr.head_ref_oid)
            .await
            .and_then(|stack| {
                let [change] = stack.as_slice() else {
                    bail!(CliError::new(format!(
                        "adopted PR #{} head {} resolved to {} changes, expected one",
                        pr.number,
                        short_commit_id(&pr.head_ref_oid),
                        stack.len()
                    )));
                };
                Ok(change.clone())
            })
            .map_err(|error| phase_error("write-cache", format!("PR #{}", pr.number), error))?;
        store.upsert_pr(
            &github.repo,
            &change.change_id,
            pr.clone().into_cache_entry(None),
        );
        store.save_best_effort(diagnostics, "write-cache");
    }

    ui_info!(
        "future submit will update `{}` through tracked jj bookmark `{}`",
        github_pr_url(&github.repo, pr.number),
        pr.head_ref_name
    );

    Ok(pr.number)
}

#[tracing::instrument(skip_all, fields(pr = pr.number))]
pub(crate) fn validate_unfreeze_pr(
    config: &AppConfig,
    github: &GitHubContext,
    pr: &GhPr,
) -> Result<()> {
    validate_get_pr_metadata(github, pr)?;
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            CliError::new(format!("PR #{} is {}, expected open", pr.number, pr.state))
                .resolution("unfreeze only supports open PRs")
        );
    }
    let head_repo = get_pr_repo(pr, "head")?;
    let base_repo = get_pr_repo(pr, "base")?;
    if head_repo.name_with_owner != github.repo {
        bail!(CliError::new(format!(
            "cannot unfreeze fork-backed PR #{}: head repo is `{}`, expected `{}`",
            pr.number, head_repo.name_with_owner, github.repo
        )));
    }
    if base_repo.name_with_owner != github.repo {
        bail!(CliError::new(format!(
            "cannot unfreeze PR #{}: base repo is `{}`, expected `{}`",
            pr.number, base_repo.name_with_owner, github.repo
        )));
    }
    if pr.base_ref_name.is_empty() || pr.head_ref_name.is_empty() {
        bail!(CliError::new(format!(
            "cannot unfreeze PR #{} with empty head/base branch",
            pr.number
        )));
    }
    validate_ref_component("head branch", pr.head_ref_name.clone())?;
    validate_ref_component("base branch", pr.base_ref_name.clone())?;
    validate_ref_component("configured remote", config.remote.clone())?;
    Ok(())
}

pub(crate) async fn verify_repo_push_permission(
    runner: &impl CommandRunner,
    github: &GitHubContext,
) -> Result<()> {
    let endpoint = format!("repos/{}", github.repo);
    let args = ["api", endpoint.as_str(), "--jq", ".permissions.push"];
    let output = gh_run(runner, &args).await?;
    if !output.success {
        bail!(
            "`{}` failed while checking push permission: {}",
            display_command("gh", &args),
            output.stderr.trim()
        );
    }
    if output.stdout.trim() != "true" {
        bail!(CliError::new(format!(
            "GitHub actor `{}` lacks push permission to `{}`",
            github.username, github.repo
        )));
    }
    Ok(())
}

pub(crate) async fn verify_revision_mutable(runner: &impl CommandRunner, rev: &str) -> Result<()> {
    let args = ["log", "--no-graph", "-r", rev, "-T", "immutable"];
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    let immutable = output.stdout.trim();
    if immutable == "false" {
        return Ok(());
    }
    bail!(CliError::new(format!(
        "revision {} is still immutable after removing the frozen bookmark",
        short_commit_id(rev)
    ))
    .reason("remaining blocker may be another frozen bookmark, tag, trunk, or untracked remote bookmark"))
}

#[tracing::instrument(skip_all, fields(rev = %rev, pr = pr_number))]
pub(crate) async fn verify_unfrozen_revision_mutable(
    runner: &impl CommandRunner,
    config: &AppConfig,
    rev: &str,
    pr_number: u64,
) -> Result<()> {
    match verify_revision_mutable(runner, rev).await {
        Ok(()) => Ok(()),
        Err(_) => Err(anyhow::Error::new(
            diagnose_unfrozen_revision_immutable(runner, config, rev, pr_number).await?,
        )),
    }
}

#[tracing::instrument(skip_all, fields(rev = %rev, pr = pr_number))]
pub(crate) async fn diagnose_unfrozen_revision_immutable(
    runner: &impl CommandRunner,
    config: &AppConfig,
    rev: &str,
    pr_number: u64,
) -> Result<CliError> {
    let short_rev = short_commit_id(rev);
    if git_commit_is_ancestor(runner, rev, &format!("{}@{}", config.trunk, config.remote)).await? {
        return Ok(CliError::new(format!(
            "cannot unfreeze PR #{pr_number} because it is already reachable from trunk {}",
            config.trunk
        ))
        .reason(format!(
            "PR #{pr_number} resolves to {short_rev}, which is already contained in `{}`.",
            config.trunk
        ))
        .resolution(format!(
            "run `forklift sync` or stop trying to adopt PR #{pr_number}"
        )));
    }

    let blockers = untracked_remote_bookmark_blockers(runner, config, rev, "").await?;
    if !blockers.is_empty() {
        let labels = blockers
            .iter()
            .take(8)
            .map(|bookmark| format!("`{}@{}`", bookmark.name, bookmark.remote))
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if blockers.len() > 8 {
            format!(" and {} more", blockers.len() - 8)
        } else {
            String::new()
        };
        return Ok(CliError::new(format!(
            "cannot unfreeze PR #{pr_number} because untracked remote bookmarks still make it immutable"
        ))
        .reason(format!(
            "PR #{pr_number} resolves to {short_rev}, but it is still an ancestor of untracked remote bookmark(s): {labels}{suffix}."
        ))
        .resolution("track or delete the listed remote bookmarks, then rerun `forklift unfreeze`"));
    }

    Ok(CliError::new(format!(
        "cannot unfreeze PR #{pr_number} because it is still immutable"
    ))
    .reason(format!(
        "PR #{pr_number} resolves to {short_rev}, but jj still reports that revision as immutable after removing the frozen bookmark."
    ))
    .resolution(
        "inspect `immutable_heads()` for another blocker such as a tag, custom immutable alias, or another frozen namespace",
    ))
}
