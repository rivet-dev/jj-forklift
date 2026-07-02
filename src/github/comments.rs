use super::super::*;
use super::*;

#[tracing::instrument(skip_all, fields(pr = pr_number, change = %change_id))]
pub(crate) async fn latest_stack_comment(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    change_id: &str,
) -> Result<Option<GhStackComment>> {
    let mut comments = list_stack_comments(runner, github, pr_number, change_id)
        .await?
        .into_iter()
        .filter(|comment| comment.body.contains(STACK_COMMENT_MARKER))
        .collect::<Vec<_>>();
    comments.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(comments.pop())
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn parse_stack_pr_numbers(body: &str) -> Vec<u64> {
    let mut numbers = Vec::new();
    for line in body.lines() {
        let Some(after_pull) = line.split("/pull/").nth(1) else {
            continue;
        };
        let digits = after_pull
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        let Ok(number) = digits.parse::<u64>() else {
            continue;
        };
        if !numbers.contains(&number) {
            numbers.push(number);
        }
    }
    numbers
}

pub(crate) enum StackCommentAction {
    Created(String),
    Updated(String, usize),
    Unchanged(String),
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GhStackComment {
    pub(crate) id: u64,
    pub(crate) body: String,
    #[serde(rename = "userLogin")]
    pub(crate) user_login: String,
    #[serde(rename = "updatedAt")]
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GhCommentId {
    pub(crate) id: u64,
}

#[tracing::instrument(skip_all, fields(pr = pr_number, change = %change_id))]
pub(crate) async fn upsert_stack_comment(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    change_id: &str,
    body: &str,
    diagnostics: Diagnostics,
) -> Result<StackCommentAction> {
    let mut matches = list_stack_comments(runner, github, pr_number, change_id)
        .await?
        .into_iter()
        .filter(|comment| {
            comment.user_login == github.username && comment.body.contains(STACK_COMMENT_MARKER)
        })
        .collect::<Vec<_>>();

    if matches.is_empty() {
        return create_stack_comment(runner, github, pr_number, change_id, body, diagnostics)
            .await
            .map(StackCommentAction::Created);
    }

    matches.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let newest = matches.last().cloned().with_context(|| {
        format!("phase=stack-comments object=PR #{pr_number} select newest stack comment")
    })?;
    let duplicate_count = matches.len().saturating_sub(1);

    if duplicate_count > 0 {
        ui_warn!(
            "phase=stack-comments object=PR #{} found {} duplicate stack comments; updating newest comment {}",
            pr_number,
            duplicate_count,
            newest.id
        );
    }

    if duplicate_count == 0 && newest.body == body {
        return Ok(StackCommentAction::Unchanged(newest.id.to_string()));
    }

    update_stack_comment(
        runner,
        github,
        newest.id,
        pr_number,
        change_id,
        body,
        diagnostics,
    )
    .await
    .map(|comment_id| StackCommentAction::Updated(comment_id, duplicate_count))
}

#[tracing::instrument(skip_all, fields(pr = pr_number, change = %change_id))]
pub(crate) async fn list_stack_comments(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    change_id: &str,
) -> Result<Vec<GhStackComment>> {
    let endpoint = format!("repos/{}/issues/{}/comments", github.repo, pr_number);
    let args = [
        "api",
        "--paginate",
        endpoint.as_str(),
        "--jq",
        STACK_COMMENT_JQ,
    ];
    let output = gh_run(runner, &args).await?;
    if !output.success {
        bail!(
            "phase=stack-comments object=PR #{} change:{} failed-api=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            pr_number,
            change_id,
            display_command("gh", &args),
            output.stderr.trim()
        );
    }

    output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| parse_stack_comment_line(line, pr_number, change_id))
        .collect()
}

/// Best-effort lookup of an existing stack comment id, used when rebuilding cache
/// from GitHub during recovery. Returns `None` (rather than failing recovery) if
/// the scan errors or finds no matching comment; a later submit re-scans and will
/// create or update the comment as needed. Backfilling the id here keeps the
/// rebuilt cache complete instead of relying solely on the scan to avoid a
/// duplicate comment.
#[tracing::instrument(skip_all, fields(pr = pr_number, change = %change_id))]
pub(crate) async fn find_stack_comment_id(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    change_id: &str,
) -> Option<String> {
    let mut matches = list_stack_comments(runner, github, pr_number, change_id)
        .await
        .ok()?
        .into_iter()
        .filter(|comment| {
            comment.user_login == github.username && comment.body.contains(STACK_COMMENT_MARKER)
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    matches.last().map(|comment| comment.id.to_string())
}

#[tracing::instrument(level = "trace", skip_all, fields(pr = pr_number, change = %change_id))]
pub(crate) fn parse_stack_comment_line(
    line: &str,
    pr_number: u64,
    change_id: &str,
) -> Result<GhStackComment> {
    serde_json::from_str::<GhStackComment>(line)
        .or_else(|_| {
            let encoded = serde_json::from_str::<String>(line)?;
            serde_json::from_str::<GhStackComment>(&encoded)
        })
        .with_context(|| {
            format!(
                "phase=stack-comments object=PR #{} change:{} parse stack comment lookup",
                pr_number, change_id
            )
        })
}

#[tracing::instrument(skip_all, fields(pr = pr_number, change = %change_id))]
pub(crate) async fn create_stack_comment(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    pr_number: u64,
    change_id: &str,
    body: &str,
    diagnostics: Diagnostics,
) -> Result<String> {
    let endpoint = format!("repos/{}/issues/{}/comments", github.repo, pr_number);
    let body_field = format!("body={body}");
    let args = [
        "api",
        "-X",
        "POST",
        endpoint.as_str(),
        "-f",
        body_field.as_str(),
        "--jq",
        "{id}",
    ];

    run_comment_mutation(runner, &args, "create", pr_number, change_id, diagnostics).await
}

#[tracing::instrument(skip_all, fields(comment = comment_id, pr = pr_number, change = %change_id))]
pub(crate) async fn update_stack_comment(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    comment_id: u64,
    pr_number: u64,
    change_id: &str,
    body: &str,
    diagnostics: Diagnostics,
) -> Result<String> {
    let endpoint = format!("repos/{}/issues/comments/{}", github.repo, comment_id);
    let body_field = format!("body={body}");
    let args = [
        "api",
        "-X",
        "PATCH",
        endpoint.as_str(),
        "-f",
        body_field.as_str(),
        "--jq",
        "{id}",
    ];

    run_comment_mutation(runner, &args, "update", pr_number, change_id, diagnostics).await
}

#[tracing::instrument(skip_all, fields(action = %action, pr = pr_number, change = %change_id))]
pub(crate) async fn run_comment_mutation(
    runner: &impl CommandRunner,
    args: &[&str],
    action: &str,
    pr_number: u64,
    change_id: &str,
    diagnostics: Diagnostics,
) -> Result<String> {
    diagnostics.command("gh", args);
    let output = gh_run(runner, args).await?;
    if !output.success {
        bail!(
            "phase=stack-comments object=PR #{} change:{} failed-api=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            pr_number,
            change_id,
            display_command("gh", args),
            output.stderr.trim()
        );
    }

    let id = serde_json::from_str::<GhCommentId>(&output.stdout)
        .with_context(|| {
            format!(
                "phase=stack-comments object=PR #{} change:{} parse {action} stack comment response",
                pr_number, change_id
            )
        })?
        .id;
    Ok(id.to_string())
}
