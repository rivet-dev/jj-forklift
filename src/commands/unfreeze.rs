use super::super::cli::*;
use super::super::*;

pub(crate) async fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: UnfreezeOptions,
    diagnostics: Diagnostics,
    _verbose: bool,
    dry_run: bool,
) -> Result<()> {
    let pr_number = unfreeze_stack(runner, &config, &options.target, diagnostics).await?;
    if dry_run {
        ui_progress(
            "Finished",
            &format!("unfreeze (dry run) — PR #{pr_number} planned"),
        );
    } else {
        ui_progress("Finished", &format!("unfreeze — PR #{pr_number} adopted"));
    }

    Ok(())
}
