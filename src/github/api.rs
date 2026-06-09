use super::super::*;
use super::*;

pub(crate) fn create_pr(
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

    run_pr_api(runner, &args, "create", &plan.change.change_id, diagnostics)
}

#[tracing::instrument(skip_all, fields(pr = pr_number))]
pub(crate) fn update_pr(
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

    run_pr_api(runner, &args, "update", &plan.change.change_id, diagnostics)
}

#[tracing::instrument(skip_all, fields(action = %action, change = %change_id))]
pub(crate) fn run_pr_api(
    runner: &impl CommandRunner,
    args: &[&str],
    action: &str,
    change_id: &str,
    diagnostics: Diagnostics,
) -> Result<GhPr> {
    diagnostics.command("gh", args);
    let output = gh_run(runner, args)?;
    if !output.success {
        bail!(
            "phase=github-pr-{action} object=change:{change_id} failed-api=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            display_command("gh", args),
            output.stderr.trim()
        );
    }

    serde_json::from_str(&output.stdout)
        .with_context(|| format!("parse GitHub PR response while trying to {action} {change_id}"))
}
