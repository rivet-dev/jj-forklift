use super::super::cli::*;
use super::super::*;

pub(crate) async fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: UiOptions,
    _diagnostics: Diagnostics,
    _verbose: bool,
    dry_run: bool,
) -> Result<()> {
    // `--all` hands off to jjui's own default revset; otherwise scope the view
    // to the stacks forklift tracks, unless the user supplied an explicit revset.
    let revset = if options.all {
        None
    } else {
        Some(
            options
                .revset
                .clone()
                .unwrap_or_else(|| crate::tracked_stacks_revset(&config.branch_prefix)),
        )
    };

    let mut args: Vec<String> = Vec::new();
    if let Some(revset) = &revset {
        args.push("-r".to_owned());
        args.push(revset.clone());
    }
    args.extend(options.args.iter().cloned());

    if dry_run {
        let rendered = display_command("jjui", &args.iter().map(String::as_str).collect::<Vec<_>>());
        ui_progress("Finished", &format!("ui (dry run) — would run `{rendered}`"));
        return Ok(());
    }

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    runner
        .run_interactive("jjui", &borrowed)
        .await
        .map_err(|error| phase_error("ui", "jjui", error))?;

    Ok(())
}
