use super::super::cli::*;
use super::super::*;

pub(crate) fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: SyncOptions,
    diagnostics: Diagnostics,
    _verbose: bool,
    dry_run: bool,
) -> Result<()> {
    let target_label = options.target.as_deref().unwrap_or(DEFAULT_STACK_REVSET);
    let sync_revset = effective_sync_revset(runner, options.target.as_deref())
        .map_err(|error| phase_error("resolve-sync-target", target_label, error))?;
    let summary = sync_stack(
        runner,
        &config,
        &sync_revset.revset,
        sync_revset.target.as_ref(),
        options.submit,
        options.yes,
        diagnostics,
    )?;
    if dry_run {
        ui_progress(
            "Finished",
            &format!(
                "sync (dry run) — {} roots, submit {}, {} merged branch(es) to clean",
                summary.rebased_roots,
                if summary.submit_ran {
                    "planned"
                } else {
                    "skipped"
                },
                summary.cleaned_branches
            ),
        );
    } else {
        ui_progress(
            "Finished",
            &format!(
                "sync — {} roots rebased, {} conflict(s), submit {}, {} merged branch(es) cleaned",
                summary.rebased_roots,
                summary.conflicts,
                if summary.submit_ran { "ran" } else { "skipped" },
                summary.cleaned_branches
            ),
        );
    }

    Ok(())
}
