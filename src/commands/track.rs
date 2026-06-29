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

    let prefix = if dry_run { "track (dry run) — would adopt" } else { "track — adopted" };
    let summary = match outcome.pr_number {
        Some(number) => format!(
            "{prefix} PR #{number} as `{}` ({})",
            outcome.head_branch,
            short_change_id(&outcome.change_id),
        ),
        None => format!(
            "{prefix} branch `{}` locally ({}) — no open PR; run `forklift submit` to open one",
            outcome.head_branch,
            short_change_id(&outcome.change_id),
        ),
    };
    ui_progress("Finished", &summary);
    if outcome.outside_current_stack {
        let tail = match outcome.pr_number {
            Some(number) => format!("update PR #{number}"),
            None => "submit it".to_owned(),
        };
        ui_progress(
            "Note",
            &format!(
                "{} is not in the current stack (trunk()..@); `forklift submit`/`sync` will {tail} once @ moves onto it",
                short_commit_id(&outcome.commit_id),
            ),
        );
    }

    Ok(())
}
