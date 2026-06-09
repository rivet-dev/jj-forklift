use super::super::cli::*;
use super::super::*;

pub(crate) fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: PrOptions,
    _diagnostics: Diagnostics,
    _verbose: bool,
    dry_run: bool,
) -> Result<()> {
    let target_label = options.target.as_deref().unwrap_or("@");
    let github = GitHubContext::resolve(runner)
        .map_err(|error| phase_error("resolve-github", target_label, error))?;
    let (number, url) = resolve_pr_url(runner, &config, &github, options.target.as_deref())
        .map_err(|error| phase_error("resolve-pr", target_label, error))?;
    if dry_run {
        ui_progress(
            "Finished",
            &format!("pr (dry run) — would open PR #{number} at {url}"),
        );
        return Ok(());
    }
    ui_progress("Opening", &format!("PR #{number} — {url}"));
    open_url(runner, &url).map_err(|error| phase_error("open-pr", &url, error))?;

    Ok(())
}
