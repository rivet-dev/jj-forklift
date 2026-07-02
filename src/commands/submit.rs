use super::super::cli::*;
use super::super::*;

pub(crate) async fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: SubmitOptions,
    diagnostics: Diagnostics,
    verbose: bool,
    dry_run: bool,
) -> Result<()> {
    diagnostics.phase("submit-fetch");
    fetch_remote_preserving_local_commits(runner, config, diagnostics)
        .await
        .map_err(|error| phase_error("submit-fetch", &config.remote, error))?;

    let mut context = resolve_stack_context(runner, DEFAULT_STACK_REVSET)
        .await
        .map_err(|error| phase_error("resolve-stack", DEFAULT_STACK_REVSET, error))?;

    // The pre-submit fetch fast-forwards the local trunk bookmark whenever
    // upstream moved, stranding the stack root behind the new trunk — a state
    // submit base validation rejects. Instead of failing and demanding a manual
    // `forklift sync`, offer to perform sync's trunk-move + rebase here
    // (confirmed interactively unless `--yes`) and carry on.
    if stack_behind_trunk(runner, config, &context)
        .await
        .map_err(|error| phase_error("validate-submit-bases", "stack", error))?
    {
        // A dry run only plans the rebase, so there is nothing to confirm.
        if !dry_run {
            confirm_sync_before_submit(&config.trunk, options.yes)?;
        }
        diagnostics.phase("move-trunk");
        move_trunk_to_remote(runner, config, diagnostics)
            .await
            .map_err(|error| phase_error("move-trunk", &config.trunk, error))?;
        diagnostics.phase("rebase-stack");
        rebase_stack_roots(
            runner,
            &context.stack,
            RebaseDestination::Trunk(config.trunk.clone()),
            diagnostics,
        )
        .await
        .map_err(|error| phase_error("rebase-stack", DEFAULT_STACK_REVSET, error))?;
        if dry_run {
            // The rebase was only planned, so the live commits still fail base
            // validation; stop at the plan rather than submitting stale ids.
            diagnostics.plan_line("- run submit after the rebase");
            ui_progress(
                "Finished",
                "submit (dry run) — stack is behind trunk; a real run rebases it, then submits",
            );
            return Ok(());
        }
        let conflicts = report_sync_conflicts(runner, DEFAULT_STACK_REVSET)
            .await
            .map_err(|error| phase_error("rebase-stack", DEFAULT_STACK_REVSET, error))?;
        if conflicts > 0 {
            return Err(phase_error(
                "rebase-stack",
                "stack",
                anyhow::Error::new(
                    CliError::new(format!(
                        "rebasing the stack onto trunk `{}` left {conflicts} conflicted change(s)",
                        config.trunk
                    ))
                    .resolution("resolve the conflicts, then rerun `forklift submit`"),
                ),
            ));
        }
        context = resolve_stack_context(runner, DEFAULT_STACK_REVSET)
            .await
            .map_err(|error| phase_error("resolve-stack", DEFAULT_STACK_REVSET, error))?;
    }

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
    )
    .await?;
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
