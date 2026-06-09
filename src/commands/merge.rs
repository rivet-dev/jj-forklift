use super::super::cli::*;
use super::super::*;

pub(crate) fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: MergeOptions,
    diagnostics: Diagnostics,
    verbose: bool,
    dry_run: bool,
) -> Result<()> {
    let mut merge_config = config.clone();
    if options.no_require_approval || options.admin {
        merge_config.require_approval = false;
    }
    if options.sync {
        let target_label = options.target.as_deref().unwrap_or(DEFAULT_STACK_REVSET);
        let sync_revset = effective_sync_revset(runner, options.target.as_deref())
            .map_err(|error| phase_error("resolve-sync-target", target_label, error))?;
        sync_stack(
            runner,
            &config,
            &sync_revset.revset,
            sync_revset.target.as_ref(),
            true,
            true,
            diagnostics,
        )?;
    }
    let target_label = options.target.as_deref().unwrap_or(DEFAULT_STACK_REVSET);
    let merge_revset = effective_merge_revset(runner, options.target.as_deref())
        .map_err(|error| phase_error("resolve-merge-target", target_label, error))?;
    let sync_command = merge_sync_command(options.target.as_deref());
    let summary = match merge_stack(
        runner,
        &merge_config,
        &merge_revset.revset,
        merge_revset.target.as_ref(),
        &sync_command,
        options.admin,
        diagnostics,
    ) {
        Ok(summary) => summary,
        Err(error) if !dry_run => {
            if let Some(submit_required) = find_merge_submit_required(&error) {
                submit_before_retrying_merge(
                    runner,
                    &config,
                    &merge_revset.revset,
                    &submit_required,
                    diagnostics,
                )?;
            } else if let Some(sync_required) = find_merge_sync_required(&error) {
                sync_submit_before_retrying_merge(
                    runner,
                    &config,
                    options.target.as_deref(),
                    &sync_required,
                    diagnostics,
                )?;
            } else {
                return Err(error);
            }
            merge_stack(
                runner,
                &merge_config,
                &merge_revset.revset,
                merge_revset.target.as_ref(),
                &sync_command,
                options.admin,
                diagnostics,
            )?
        }
        Err(error) => return Err(error),
    };
    // A targeted merge only re-submits the merged range, so PRs *above*
    // the target still list the now-merged PRs in their stack comments
    // (and their branches were rebased when the merged changes were
    // abandoned). Refresh the full stack so the merged PRs drop out of
    // those comments and the rebased branches are republished. The
    // no-target merge already refreshes remaining PRs each iteration.
    if !dry_run && summary.merged_prs > 0 && options.target.is_some() {
        refresh_stack_above_merge(runner, &config, DEFAULT_STACK_REVSET, diagnostics)
            .map_err(|error| phase_error("merge-refresh-above", DEFAULT_STACK_REVSET, error))?;
    }
    if verbose {
        eprintln!(
            "merge: {} merged, {} local updates, {} submits, {} branches cleaned",
            summary.merged_prs,
            summary.local_updates,
            summary.submit_runs,
            summary.cleaned_branches
        );
    }
    if dry_run {
        ui_progress(
            "Finished",
            &format!("merge (dry run) — {} PRs checked", summary.checked_prs),
        );
    } else {
        ui_progress(
            "Finished",
            &format!(
                "merge — {} PRs merged, {} branches cleaned",
                summary.merged_prs, summary.cleaned_branches
            ),
        );
    }

    Ok(())
}
