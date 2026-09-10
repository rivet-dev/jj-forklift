use super::super::cli::*;
use super::super::*;

pub(crate) async fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: SyncOptions,
    diagnostics: Diagnostics,
    _verbose: bool,
    dry_run: bool,
) -> Result<()> {
    // `--unfreeze` adopts every frozen dependency first so the rebase below
    // treats those imported commits as owned and carries them onto trunk too.
    if options.unfreeze {
        let unfrozen = unfreeze_all_dependencies(runner, config, diagnostics).await?;
        ui_progress(
            "Unfreezing",
            &format!(
                "{unfrozen} dependency(ies) adopted before {}sync",
                if dry_run { "dry-run " } else { "" }
            ),
        );
    }

    // With no explicit target and without `--current`, sync every tracked stack
    // (like `gt sync`) instead of only the stack containing `@`.
    if options.target.is_none() && !options.current {
        let summary =
            sync_all_stacks(runner, &config, options.submit, options.yes, diagnostics).await?;
        reconcile_git_refs_after_sync(runner, diagnostics, dry_run).await;
        let verb = if dry_run { "sync (dry run)" } else { "sync" };
        let failed_note = if summary.failed > 0 {
            format!(", {} stack(s) failed", summary.failed)
        } else {
            String::new()
        };
        ui_progress(
            "Finished",
            &format!(
                "{verb} — {} stack(s){failed_note}, {} roots {}, {} conflict(s), submit {}, {} merged branch(es) {}, {} duplicate(s) {}",
                summary.stacks,
                summary.rebased_roots,
                if dry_run { "to rebase" } else { "rebased" },
                summary.conflicts,
                submit_state(summary.submit_ran, dry_run),
                summary.cleaned_branches,
                if dry_run { "to clean" } else { "cleaned" },
                summary.pruned_duplicates,
                if dry_run { "to prune" } else { "pruned" },
            ),
        );
        // Best-effort already synced the healthy stacks; still exit non-zero so
        // callers/CI notice the ones that need attention (warnings printed above).
        if summary.failed > 0 {
            bail!(
                CliError::new(format!(
                    "{} of {} tracked stack(s) failed to sync",
                    summary.failed,
                    summary.failed + summary.stacks
                ))
                .resolution(
                    "see the per-stack warnings above; fix each stack (e.g. resolve divergence or conflicts) and re-run"
                )
            );
        }
        return Ok(());
    }

    let target_label = options.target.as_deref().unwrap_or(DEFAULT_STACK_REVSET);
    let sync_revset = effective_sync_revset(runner, options.target.as_deref())
        .await
        .map_err(|error| phase_error("resolve-sync-target", target_label, error))?;
    let summary = sync_stack(
        runner,
        &config,
        &sync_revset.revset,
        sync_revset.target.as_ref(),
        options.submit,
        options.yes,
        diagnostics,
    )
    .await?;
    reconcile_git_refs_after_sync(runner, diagnostics, dry_run).await;
    if dry_run {
        ui_progress(
            "Finished",
            &format!(
                "sync (dry run) — {} roots, submit {}, {} merged branch(es) to clean, {} duplicate(s) to prune",
                summary.rebased_roots,
                if summary.submit_ran {
                    "planned"
                } else {
                    "skipped"
                },
                summary.cleaned_branches,
                summary.pruned_duplicates
            ),
        );
    } else {
        ui_progress(
            "Finished",
            &format!(
                "sync — {} roots rebased, {} conflict(s), submit {}, {} merged branch(es) cleaned, {} duplicate(s) pruned",
                summary.rebased_roots,
                summary.conflicts,
                if summary.submit_ran { "ran" } else { "skipped" },
                summary.cleaned_branches,
                summary.pruned_duplicates
            ),
        );
    }

    Ok(())
}

/// Adopt every frozen dependency bookmark so its commits become mutable and
/// owned, then return how many were unfrozen. Each is run through the same
/// adoption as `forklift unfreeze <pr>`, lowest PR first.
async fn unfreeze_all_dependencies(
    runner: &impl CommandRunner,
    config: &AppConfig,
    diagnostics: Diagnostics,
) -> Result<usize> {
    let frozen = frozen_bookmarks(runner)
        .await
        .map_err(|error| phase_error("unfreeze-deps", "frozen bookmarks", error))?;
    for bookmark in &frozen {
        unfreeze_stack(runner, config, &bookmark.pr_number.to_string(), diagnostics).await?;
    }
    Ok(frozen.len())
}

/// Best-effort divergence guard, run after every real sync: flush jj's pending
/// bookmark moves into the colocated `refs/heads/*`. Forklift mutates bookmarks
/// from whichever workspace it runs in, and a secondary (non-colocated) workspace
/// does not auto-export those moves into the primary's `.git`. Left un-exported, a
/// stack ref lingers at a pre-rewrite commit; a later `jj git import` (which every
/// `jj git fetch`, and even a plain `jj log`, performs) can resurrect that old
/// commit, and if its rewritten copy is kept alive — e.g. it merged to trunk — the
/// change becomes divergent and every subsequent sync bails on it. The import that
/// does the damage fires on the *first* jj command of the next invocation, before
/// any in-run guard could act, so the fix is to never leave a stale ref behind.
/// Never fails the sync — an export hiccup only risks the drift it was clearing,
/// and the summary/exit code already stand. Skipped on dry runs, which mutate
/// nothing.
async fn reconcile_git_refs_after_sync(
    runner: &impl CommandRunner,
    diagnostics: Diagnostics,
    dry_run: bool,
) {
    if dry_run {
        return;
    }
    if let Err(error) = git_export(runner, diagnostics).await {
        diagnostics.warn(format!(
            "could not export bookmarks to git after sync (harmless; drift may cause a future divergence warning): {error:#}"
        ));
    }
}

fn submit_state(submit_ran: bool, dry_run: bool) -> &'static str {
    match (submit_ran, dry_run) {
        (false, _) => "skipped",
        (true, true) => "planned",
        (true, false) => "ran",
    }
}
