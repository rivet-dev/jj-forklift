use super::super::*;
use super::*;

#[tracing::instrument(skip_all, fields(target = target))]
pub(crate) async fn repair_stack_comments(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: &str,
    yes: bool,
    diagnostics: Diagnostics,
) -> Result<RepairSummary> {
    diagnostics.phase("resolve-github");
    let mut github = GitHubContext::resolve(runner)
        .await
        .map_err(|error| phase_error("resolve-github", "github", error))?;
    let target = parse_get_target(target, &github.repo)?;
    github.repo = target.repo().to_owned();

    diagnostics.phase("resolve-stack-comment");
    let target_pr = resolve_get_target_pr(runner, &github, target).await?;
    if !target_pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(
            CliError::new(format!(
                "repair target PR #{} is {}",
                target_pr.number, target_pr.state
            ))
            .resolution("choose an open PR whose stack comment should be repaired")
        );
    }
    let comment = latest_stack_comment(runner, &github, target_pr.number, "repair").await?;
    let mut pr_numbers = comment
        .as_ref()
        .map(|comment| parse_stack_pr_numbers(&comment.body))
        .unwrap_or_default();
    pr_numbers.reverse();
    if pr_numbers.is_empty() {
        pr_numbers.push(target_pr.number);
    }
    if !pr_numbers.contains(&target_pr.number) {
        bail!(
            "stack comment for PR #{} did not include the target PR",
            target_pr.number
        );
    }

    diagnostics.phase("resolve-prs");
    let plan =
        plan_stack_comment_repair(runner, config, &github, target_pr.number, pr_numbers).await?;
    let actions = repair_actions(&github, config, &plan);
    print_repair_action_plan(&plan, &actions, diagnostics);

    let mut summary = RepairSummary {
        open_prs: plan.open_prs.len(),
        pruned_merged_prs: plan.pruned_merged_prs.len(),
        comments_changed: 0,
    };
    if diagnostics.dry_run {
        summary.comments_changed = actions.len();
        return Ok(summary);
    }
    if actions.is_empty() {
        return Ok(summary);
    }
    confirm_repair_stack_comments(&plan, &actions, target_pr.number, yes)?;

    diagnostics.phase("stack-comments");
    for action in &actions {
        if execute_repair_action(runner, &github, action, diagnostics).await? {
            summary.comments_changed += 1;
        }
    }

    diagnostics.phase("repair-validate");
    validate_repair_result(runner, config, &github, target_pr.number, &plan).await?;

    Ok(summary)
}

pub(crate) fn repair_actions(
    github: &GitHubContext,
    config: &AppConfig,
    plan: &RepairPlan,
) -> Vec<RepairAction> {
    if plan.pruned_merged_prs.is_empty() {
        return Vec::new();
    }

    plan.open_prs
        .iter()
        .map(|pr| RepairAction::UpsertStackComment {
            pr_number: pr.number,
            removed_prs: plan.pruned_merged_prs.clone(),
            body: repaired_stack_comment_body(github, &plan.open_prs, pr.number, &config.trunk),
        })
        .collect()
}

pub(crate) fn print_repair_action_plan(
    plan: &RepairPlan,
    actions: &[RepairAction],
    diagnostics: Diagnostics,
) {
    render_repair_action_plan(plan, actions, |line| diagnostics.plan_line(line));
}

pub(crate) fn render_repair_action_plan(
    plan: &RepairPlan,
    actions: &[RepairAction],
    mut emit: impl FnMut(&str),
) {
    let pruned = repair_pr_list(&plan.pruned_merged_prs);

    emit("");
    emit("problems:");
    emit(&format!(
        "  merged PRs still listed in stack comment: {pruned}"
    ));
    emit("");
    emit("actions:");
    if actions.is_empty() {
        emit("  <none>");
    } else {
        for (index, action) in actions.iter().enumerate() {
            emit(&format!("  {}. {}", index + 1, action.describe()));
        }
        emit(&format!(
            "  {}. revalidate repaired stack comment topology",
            actions.len() + 1
        ));
    }
}

pub(crate) fn repair_pr_list(numbers: &[u64]) -> String {
    if numbers.is_empty() {
        "<none>".to_owned()
    } else {
        numbers
            .iter()
            .map(|number| format!("#{number}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// How a colored presenter tints an action row's verb. The renderer picks the
/// variant from the plan itself, so color follows structure — a layout change
/// (e.g. dropping a row's number) can no longer strip color the way re-parsing a
/// finished string once did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitActionStyle {
    Create,
    Update,
    Unchanged,
    Sync,
}

/// One rendered line of the submit action plan. Presenters turn these into
/// strings — coloring `Header`/`Action` by kind — instead of reverse-engineering
/// structure out of already-built text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubmitPlanLine {
    Blank,
    Header(&'static str),
    Action {
        /// Left margin including the number-or-spaces prefix, e.g. `"  1. "` or
        /// `"     "`. Computed once at render time so rows stay aligned.
        indent: String,
        style: SubmitActionStyle,
        /// The colored portion (`"update"`, `"create new PR"`, ...).
        verb: &'static str,
        /// The uncolored remainder (` PR #1 \`title\``).
        detail: String,
    },
}

pub(crate) fn print_submit_action_plan(repo: &str, plans: &[SubmitPlan]) {
    let color = ui_color_enabled();
    for line in submit_action_plan_lines(repo, plans) {
        eprintln!("{}", present_submit_plan_line(&line, color));
    }
}

pub(crate) fn submit_action_plan_lines(repo: &str, plans: &[SubmitPlan]) -> Vec<SubmitPlanLine> {
    let mut lines = vec![SubmitPlanLine::Blank, SubmitPlanLine::Header("actions")];
    // Only changed PRs (create/update) get a number; unchanged PRs carry no
    // number and are indented to line up under the numbered descriptions, so the
    // numbers count actual work and can skip (`1`, then `3`). Right-align them so
    // periods and descriptions line up once the count crosses into double digits
    // (`  9.` vs ` 10.`). The trailing sync-comments action takes the final
    // number, so the count of numbered lines sets the column width.
    let numbered = plans
        .iter()
        .filter(|plan| plan.existing_pr.is_none() || plan.pr_update_needed)
        .count();
    let width = (numbered + 1).ilog10() as usize + 1;
    let mut number = 0usize;
    for plan in plans.iter() {
        let unchanged = plan.existing_pr.is_some() && !plan.pr_update_needed;
        // Blank column plus two spaces stands in for the `N. ` prefix so the
        // unchanged row lines up with the numbered rows above and below it.
        let indent = if unchanged {
            format!("  {:width$}  ", "")
        } else {
            number += 1;
            format!("  {number:>width$}. ")
        };
        let (style, verb, detail) = submit_action_parts(repo, plan);
        lines.push(SubmitPlanLine::Action {
            indent,
            style,
            verb,
            detail,
        });
    }
    if !plans.is_empty() {
        number += 1;
        lines.push(SubmitPlanLine::Action {
            indent: format!("  {number:>width$}. "),
            style: SubmitActionStyle::Sync,
            verb: "sync",
            detail: " stack comments for submitted stack".to_owned(),
        });
    }
    lines.push(SubmitPlanLine::Blank);
    lines
}

/// Splits a plan into its colored verb and uncolored detail. Building the two
/// halves here means the presenter never has to find the verb inside a rendered
/// string.
fn submit_action_parts(repo: &str, plan: &SubmitPlan) -> (SubmitActionStyle, &'static str, String) {
    let pr_ref =
        |pr_number: u64| ui_hyperlink(&github_pr_url(repo, pr_number), &format!("#{pr_number}"));
    match &plan.existing_pr {
        None => (
            SubmitActionStyle::Create,
            "create new PR",
            format!(" `{}`", plan.change.title),
        ),
        Some(existing) if plan.pr_update_needed => (
            SubmitActionStyle::Update,
            "update",
            format!(" PR {} `{}`", pr_ref(existing.pr_number), plan.change.title),
        ),
        Some(existing) => (
            SubmitActionStyle::Unchanged,
            "unchanged",
            format!(" PR {} `{}`", pr_ref(existing.pr_number), plan.change.title),
        ),
    }
}

pub(crate) fn present_submit_plan_line(line: &SubmitPlanLine, color: bool) -> String {
    match line {
        SubmitPlanLine::Blank => String::new(),
        SubmitPlanLine::Header(label) => {
            if color {
                format!("{}:", label.cyan().bold())
            } else {
                format!("{label}:")
            }
        }
        SubmitPlanLine::Action {
            indent,
            style,
            verb,
            detail,
        } => {
            let verb = if color {
                match style {
                    SubmitActionStyle::Create => verb.green().to_string(),
                    SubmitActionStyle::Update => verb.yellow().to_string(),
                    SubmitActionStyle::Unchanged => verb.dimmed().to_string(),
                    SubmitActionStyle::Sync => verb.cyan().to_string(),
                }
            } else {
                (*verb).to_string()
            };
            format!("{indent}{verb}{detail}")
        }
    }
}

pub(crate) fn short_commit_id(commit_id: &str) -> &str {
    commit_id.get(..8).unwrap_or(commit_id)
}

pub(crate) fn short_change_id(change_id: &str) -> &str {
    change_id.get(..8).unwrap_or(change_id)
}

pub(crate) fn confirm_submit_stack(yes: bool, yes_command: &str) -> Result<()> {
    if yes {
        return Ok(());
    }

    if !io::stdin().is_terminal() {
        return Err(CliError::new("submit requires confirmation")
            .reason("submit would update GitHub branches, PRs, or stack comments, but stdin is not a terminal")
            .resolution(format!("rerun with `{yes_command}`"))
            .into());
    }

    eprint!("Apply submit? [y/N] ");
    io::stderr()
        .flush()
        .context("flush submit confirmation prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read submit confirmation")?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        return Ok(());
    }

    Err(CliError::new("submit cancelled")
        .reason("user declined to apply the submit plan")
        .resolution(format!("rerun with `{yes_command}`"))
        .into())
}

/// Prompt the user, then run a full sync + submit (fetch, rebase onto trunk,
/// submit) before merge retries.
///
/// Shared by both the submit-required and sync-required recovery paths. A bare
/// submit cannot land a stack whose root is based on a stale trunk — submit's
/// base validation refuses it ("base `main` is not an ancestor of the stack
/// root") — so both paths route through sync, which fetches and rebases the
/// stack onto trunk first and *then* submits. This lets a single `forklift
/// merge` carry the user through the whole sync+submit system instead of failing
/// with a "run sync yourself" hint.
async fn prompt_then_sync_submit_before_merge(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: Option<&str>,
    announce: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let command = merge_sync_command(target);
    eprintln!("{announce}");
    eprint!("Run `{command}` now? [y/N] ");
    io::stderr().flush().context("flush merge sync prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read merge sync prompt")?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        return Err(CliError::new("merge cancelled")
            .reason("sync and submit were not run")
            .resolution(format!("run `{command}`"))
            .into());
    }

    let target_label = target.unwrap_or(DEFAULT_STACK_REVSET);
    let sync_revset = effective_sync_revset(runner, target)
        .await
        .map_err(|error| phase_error("resolve-sync-target", target_label, error))?;
    sync_stack(
        runner,
        config,
        &sync_revset.revset,
        sync_revset.target.as_ref(),
        true,
        true,
        diagnostics,
    )
    .await?;
    Ok(())
}

pub(crate) async fn submit_before_retrying_merge(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: Option<&str>,
    submit_required: &MergeSubmitRequired,
    diagnostics: Diagnostics,
) -> Result<()> {
    if !io::stdin().is_terminal() {
        let diagnostic = submit_required.cli_error();
        return Err(CliError::new(diagnostic.message)
            .reason(diagnostic.reason.unwrap_or_else(|| {
                "merge found local changes that have not been submitted".to_owned()
            }))
            .resolution(format!("run `{}`", merge_sync_command(target)))
            .into());
    }

    tracing::debug!(
        reason = %submit_required.reason,
        resolution = %submit_required.resolution,
        "merge requires sync and submit before retry"
    );
    prompt_then_sync_submit_before_merge(
        runner,
        config,
        target,
        "Merge needs the stack synced and submitted before it can continue.",
        diagnostics,
    )
    .await?;
    Ok(())
}

pub(crate) async fn sync_submit_before_retrying_merge(
    runner: &impl CommandRunner,
    config: &AppConfig,
    target: Option<&str>,
    sync_required: &MergeSyncRequired,
    diagnostics: Diagnostics,
) -> Result<()> {
    if !io::stdin().is_terminal() {
        let diagnostic = sync_required.cli_error();
        return Err(CliError::new(diagnostic.message)
            .reason(
                diagnostic
                    .reason
                    .unwrap_or_else(|| "merge found a stack that is not synced".to_owned()),
            )
            .resolution(format!("run `{}`", merge_sync_command(target)))
            .into());
    }

    tracing::debug!(
        reason = %sync_required.reason,
        resolution = %sync_required.resolution,
        "merge requires sync and submit before retry"
    );
    prompt_then_sync_submit_before_merge(
        runner,
        config,
        target,
        "Merge needs sync and submit before it can continue.",
        diagnostics,
    )
    .await?;
    Ok(())
}

pub(crate) async fn unfreeze_before_retrying_merge(
    runner: &impl CommandRunner,
    config: &AppConfig,
    unfreeze_required: &MergeUnfreezeRequired,
    diagnostics: Diagnostics,
) -> Result<()> {
    let unfreeze_commands = merge_unfreeze_commands(&unfreeze_required.unfreeze_targets);
    let (merge_command, sync_command) = match &unfreeze_required.target {
        Some(target) => (
            format!("forklift merge {target}"),
            format!("forklift sync {target} --submit --yes"),
        ),
        None => (
            "forklift merge".to_owned(),
            "forklift sync --submit --yes".to_owned(),
        ),
    };
    if !io::stdin().is_terminal() {
        let diagnostic = unfreeze_required.cli_error();
        return Err(CliError::new(diagnostic.message)
            .reason(
                diagnostic
                    .reason
                    .unwrap_or_else(|| "merge target is frozen".to_owned()),
            )
            .resolution(format!(
                "run {unfreeze_commands}, then `{sync_command}`, then rerun `{merge_command}`"
            ))
            .into());
    }

    tracing::debug!(
        target = unfreeze_required.target.as_deref().unwrap_or("<stack>"),
        reason = %unfreeze_required.reason,
        "merge requires unfreeze before retry"
    );
    eprintln!("{}.", unfreeze_required.message);
    eprint!(
        "Run {unfreeze_commands}, then sync+submit the adopted stack, then retry merge? [y/N] "
    );
    io::stderr()
        .flush()
        .context("flush merge unfreeze prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read merge unfreeze prompt")?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        return Err(CliError::new("merge cancelled")
            .reason("unfreeze was not run")
            .resolution(format!(
                "run {unfreeze_commands}, then `{sync_command}`, then rerun `{merge_command}`"
            ))
            .into());
    }

    for target in &unfreeze_required.unfreeze_targets {
        unfreeze_stack(runner, config, target, diagnostics).await?;
    }
    Ok(())
}

pub(crate) fn print_repair_plan_line(line: &str) {
    if ui_color_enabled() {
        if let Some((label, rest)) = line.split_once(':') {
            match label.trim() {
                "problems" => {
                    eprintln!("{}:{rest}", label.red().bold());
                    return;
                }
                "actions" => {
                    eprintln!("{}:{rest}", label.cyan().bold());
                    return;
                }
                _ => {}
            }
        }
    }
    eprintln!("{line}");
}

pub(crate) async fn execute_repair_action(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    action: &RepairAction,
    diagnostics: Diagnostics,
) -> Result<bool> {
    match action {
        RepairAction::UpsertStackComment {
            pr_number, body, ..
        } => match upsert_stack_comment(runner, github, *pr_number, "repair", body, diagnostics)
            .await?
        {
            StackCommentAction::Created(_) | StackCommentAction::Updated(_, _) => Ok(true),
            StackCommentAction::Unchanged(_) => Ok(false),
        },
    }
}

pub(crate) fn confirm_repair_stack_comments(
    plan: &RepairPlan,
    actions: &[RepairAction],
    target_pr_number: u64,
    yes: bool,
) -> Result<()> {
    if yes {
        return Ok(());
    }
    render_repair_action_plan(plan, actions, |line| print_repair_plan_line(line));
    eprintln!();
    if !io::stdin().is_terminal() {
        return Err(anyhow::Error::new(
            CliError::new("repair requires confirmation")
                .reason("repair would update GitHub stack comments, but stdin is not a terminal")
                .resolution(format!(
                    "rerun with `forklift repair {target_pr_number} --yes`"
                ))
                .detail("target", format!("#{target_pr_number}"))
                .detail("comments", plan.open_prs.len()),
        ));
    }
    eprint!("Apply repair? [y/N] ");
    io::stderr()
        .flush()
        .context("flush repair confirmation prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read repair confirmation")?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        Ok(())
    } else {
        Err(anyhow::Error::new(
            CliError::new("repair cancelled")
                .reason("confirmation was not accepted")
                .resolution(format!(
                    "rerun with `forklift repair {target_pr_number} --yes`"
                )),
        ))
    }
}

pub(crate) async fn validate_repair_result(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    target_pr_number: u64,
    plan: &RepairPlan,
) -> Result<()> {
    let comment = latest_stack_comment(runner, github, target_pr_number, "repair-validate")
        .await?
        .with_context(|| format!("repaired PR #{target_pr_number} has no stack comment"))?;
    let mut pr_numbers = parse_stack_pr_numbers(&comment.body);
    pr_numbers.reverse();
    let validation_plan =
        plan_stack_comment_repair(runner, config, github, target_pr_number, pr_numbers)
            .await
            .context("re-run repair detection on repaired stack comment")?;
    if !validation_plan.pruned_merged_prs.is_empty() {
        bail!(CliError::new(format!(
            "repair validation failed: repaired stack comment still lists merged PR(s): {}",
            repair_pr_list(&validation_plan.pruned_merged_prs)
        )));
    }

    let expected = plan.open_prs.iter().map(|pr| pr.number).collect::<Vec<_>>();
    let actual = validation_plan
        .open_prs
        .iter()
        .map(|pr| pr.number)
        .collect::<Vec<_>>();
    if actual != expected {
        bail!(CliError::new(format!(
            "repair validation failed: stack comment lists {actual:?}, expected {expected:?}"
        )));
    }

    Ok(())
}

pub(crate) async fn plan_stack_comment_repair(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    target_pr_number: u64,
    pr_numbers: Vec<u64>,
) -> Result<RepairPlan> {
    let mut seen = HashSet::new();
    // The duplicate check is on the PR number alone, so it runs up front before
    // any network call; the fetches then fan out concurrently, order preserved.
    for pr_number in &pr_numbers {
        if !seen.insert(*pr_number) {
            bail!("stack comment listed PR #{} more than once", pr_number);
        }
    }
    let fetched = stream::iter(
        pr_numbers
            .iter()
            .map(|pr_number| fetch_pr_by_number(runner, github, "repair", *pr_number)),
    )
    .buffered(NETWORK_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut open_prs = Vec::new();
    let mut pruned_merged_prs = Vec::new();
    for pr in fetched {
        let pr = pr?;
        validate_get_pr_metadata(github, &pr)?;
        if pr.state.eq_ignore_ascii_case("OPEN") {
            open_prs.push(pr);
        } else if pr_was_merged(&pr) {
            pruned_merged_prs.push(pr.number);
        } else {
            return Err(anyhow::Error::new(
                CliError::new("cannot repair stack comment automatically")
                    .reason(format!(
                        "PR #{} is {} but not merged",
                        pr.number, pr.state
                    ))
                    .resolution(format!(
                        "reopen or merge PR #{}, or remove it from the stack comment manually, then run `forklift repair {target_pr_number}`",
                        pr.number
                    ))
                    .detail("target", format!("#{target_pr_number}"))
                    .detail("pr", format!("#{}", pr.number))
                    .detail("state", &pr.state),
            ));
        }
    }

    if !open_prs.iter().any(|pr| pr.number == target_pr_number) {
        bail!(
            CliError::new(format!("repair would remove target PR #{target_pr_number}"))
                .resolution("choose an open PR still in the stack")
        );
    }
    validate_get_pr_stack(config, github, target_pr_number, &open_prs)?;

    Ok(RepairPlan {
        open_prs,
        pruned_merged_prs,
    })
}

#[tracing::instrument(level = "trace", skip_all, fields(pr = pr.number))]
pub(crate) fn pr_was_merged(pr: &GhPr) -> bool {
    pr.merged || pr.state.eq_ignore_ascii_case("MERGED")
}
