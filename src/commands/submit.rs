use super::super::cli::*;
use super::super::*;

pub(crate) fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: SubmitOptions,
    diagnostics: Diagnostics,
    verbose: bool,
    dry_run: bool,
) -> Result<()> {
    let context = resolve_stack_context(runner, DEFAULT_STACK_REVSET)
        .map_err(|error| phase_error("resolve-stack", DEFAULT_STACK_REVSET, error))?;
    if verbose {
        print_github_context(&context.github);
        print_stack(&context.stack);
    }
    let summary = submit_stack(
        runner,
        &config,
        &context,
        options.yes,
        "forklift submit --yes",
        diagnostics,
    )?;
    if verbose {
        eprintln!(
            "submit: {} pushed, {} created, {} updated, {} unchanged, {} comments created, {} comments updated, {} comments unchanged",
            summary.pushed_refs,
            summary.created_prs,
            summary.updated_prs,
            summary.unchanged_prs,
            summary.created_comments,
            summary.updated_comments,
            summary.unchanged_comments
        );
    }
    if dry_run {
        ui_progress(
            "Finished",
            &format!(
                "submit (dry run) — {} changes, {} pushes, {} creates, {} updates planned",
                context.stack.len(),
                summary.pushed_refs,
                summary.created_prs,
                summary.updated_prs
            ),
        );
        return Ok(());
    }
    let closed_orphans = if summary.closed_orphans > 0 {
        format!(", {} orphans closed", summary.closed_orphans)
    } else {
        String::new()
    };
    ui_progress(
        "Finished",
        &format!(
            "submit — {} changes, {} pushed, {} created, {} updated{closed_orphans}",
            context.stack.len(),
            summary.pushed_refs,
            summary.created_prs,
            summary.updated_prs
        ),
    );

    Ok(())
}
