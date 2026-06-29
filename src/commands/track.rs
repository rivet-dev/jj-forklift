use super::super::cli::*;
use super::super::*;

pub(crate) fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: TrackOptions,
    diagnostics: Diagnostics,
    _verbose: bool,
    dry_run: bool,
) -> Result<()> {
    let outcome = track_target(runner, config, &options.target, diagnostics)?;

    let verb = if dry_run { "track (dry run) — would adopt" } else { "track — adopted" };
    ui_progress(
        "Finished",
        &format!(
            "{verb} PR #{} as `{}` ({})",
            outcome.pr_number,
            outcome.head_branch,
            short_change_id(&outcome.change_id),
        ),
    );
    if outcome.outside_current_stack {
        ui_progress(
            "Note",
            &format!(
                "{} is not in the current stack (trunk()..@); `forklift submit`/`sync` will update PR #{} once @ moves onto it",
                short_commit_id(&outcome.commit_id),
                outcome.pr_number,
            ),
        );
    }

    Ok(())
}
