use super::super::cli::*;
use super::super::*;

pub(crate) async fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: RepairOptions,
    diagnostics: Diagnostics,
    _verbose: bool,
    dry_run: bool,
) -> Result<()> {
    let summary =
        repair_stack_comments(runner, &config, &options.target, options.yes, diagnostics).await?;
    if dry_run {
        ui_progress(
            "Finished",
            &format!(
                "repair (dry run) — {} open PRs, {} merged PRs to prune, {} comments planned",
                summary.open_prs, summary.pruned_merged_prs, summary.comments_changed
            ),
        );
    } else {
        ui_progress(
            "Finished",
            &format!(
                "repair — {} open PRs, {} merged PRs pruned, {} comments changed",
                summary.open_prs, summary.pruned_merged_prs, summary.comments_changed
            ),
        );
    }

    Ok(())
}
