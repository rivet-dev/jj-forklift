use super::super::cli::*;
use super::super::*;

pub(crate) async fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: StatusOptions,
    diagnostics: Diagnostics,
    _verbose: bool,
    _dry_run: bool,
) -> Result<()> {
    let report = status_report(runner, &config, DEFAULT_STACK_REVSET, diagnostics)
        .await
        .map_err(|error| phase_error("status", DEFAULT_STACK_REVSET, error))?;
    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize status json")?
        );
    } else {
        print_status_stack_log(runner, &report.stack_log_revset).await?;
    }

    Ok(())
}
