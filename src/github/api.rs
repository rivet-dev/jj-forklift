use super::super::*;
use super::*;

pub(crate) async fn create_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    plan: &SubmitPlan,
    diagnostics: Diagnostics,
) -> Result<GhPr> {
    let title = format!("title={}", plan.change.title);
    let head = format!("head={}", plan.head_branch);
    let base = format!("base={}", plan.base_branch);
    let body = format!("body={}", plan.change.body);
    let endpoint = format!("repos/{}/pulls", github.repo);
    let args = [
        "api",
        "-X",
        "POST",
        endpoint.as_str(),
        "-f",
        title.as_str(),
        "-f",
        head.as_str(),
        "-f",
        base.as_str(),
        "-f",
        body.as_str(),
        "--jq",
        PR_API_JQ,
    ];

    run_pr_api(runner, &args, "create", &plan.change.change_id, diagnostics).await
}

#[tracing::instrument(skip_all, fields(pr = pr_number))]
pub(crate) async fn update_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    plan: &SubmitPlan,
    diagnostics: Diagnostics,
) -> Result<GhPr> {
    let title = format!("title={}", plan.change.title);
    let base = format!("base={}", plan.base_branch);
    let body = format!("body={}", plan.change.body);
    let endpoint = format!("repos/{}/pulls/{}", github.repo, pr_number);
    let args = [
        "api",
        "-X",
        "PATCH",
        endpoint.as_str(),
        "-f",
        title.as_str(),
        "-f",
        base.as_str(),
        "-f",
        body.as_str(),
        "--jq",
        PR_API_JQ,
    ];

    run_pr_api(runner, &args, "update", &plan.change.change_id, diagnostics).await
}

/// Patch only a PR's base branch.
///
/// Submit uses this to move a PR off a base branch it is about to force-push
/// past that PR's own head. The full [`update_pr`] below still runs afterwards
/// and sets the final base, title, and body.
#[tracing::instrument(skip_all, fields(pr = pr_number))]
pub(crate) async fn retarget_pr_base(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    base_branch: &str,
    change_id: &str,
    diagnostics: Diagnostics,
) -> Result<GhPr> {
    let base = format!("base={base_branch}");
    let endpoint = format!("repos/{}/pulls/{}", github.repo, pr_number);
    let args = [
        "api",
        "-X",
        "PATCH",
        endpoint.as_str(),
        "-f",
        base.as_str(),
        "--jq",
        PR_API_JQ,
    ];

    run_pr_api(runner, &args, "retarget", change_id, diagnostics).await
}

#[tracing::instrument(skip_all, fields(action = %action, change = %change_id))]
pub(crate) async fn run_pr_api(
    runner: &impl CommandRunner,
    args: &[&str],
    action: &str,
    change_id: &str,
    diagnostics: Diagnostics,
) -> Result<GhPr> {
    diagnostics.command("gh", args);
    let output = gh_run(runner, args).await?;
    if !output.success {
        bail!(
            "phase=github-pr-{action} object=change:{change_id} failed-api=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            display_command("gh", args),
            output.stderr.trim()
        );
    }

    let mut pr: GhPr = serde_json::from_str(&output.stdout)
        .with_context(|| format!("parse GitHub PR response while trying to {action} {change_id}"))?;
    normalize_rest_pr_state(&mut pr);
    Ok(pr)
}
