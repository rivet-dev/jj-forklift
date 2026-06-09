use super::super::cli::*;
use super::super::*;

pub(crate) fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: GetOptions,
    diagnostics: Diagnostics,
    _verbose: bool,
    dry_run: bool,
) -> Result<()> {
    let summary = get_stack(
        runner,
        &config,
        &options.target,
        !options.no_edit,
        diagnostics,
    )?;
    if dry_run {
        ui_progress(
            "Finished",
            &format!(
                "get (dry run) — {} PRs, {} branches planned",
                summary.prs, summary.fetched_branches
            ),
        );
    } else {
        ui_progress(
            "Finished",
            &format!(
                "get — {} PRs fetched, {} cache entries written{}",
                summary.prs,
                summary.cache_entries,
                if summary.edited {
                    ", editing new change"
                } else {
                    ""
                }
            ),
        );
    }

    Ok(())
}
