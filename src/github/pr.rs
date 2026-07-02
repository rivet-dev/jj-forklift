use super::super::*;
use super::*;

#[tracing::instrument(skip_all, fields(target = target_pr_number))]
pub(crate) fn validate_get_pr_stack(
    config: &AppConfig,
    github: &GitHubContext,
    target_pr_number: u64,
    prs: &[GhPr],
) -> Result<()> {
    if prs.is_empty() {
        bail!("get resolved an empty PR stack");
    }
    if !prs.iter().any(|pr| pr.number == target_pr_number) {
        bail!("stack comment for PR #{target_pr_number} did not include the target PR");
    }

    let mut seen = HashSet::new();
    for (index, pr) in prs.iter().enumerate() {
        if !seen.insert(pr.number) {
            bail!("stack comment listed PR #{} more than once", pr.number);
        }
        validate_get_pr_metadata(github, pr)?;
        if !pr.state.eq_ignore_ascii_case("OPEN") {
            bail!(
                CliError::new(format!("PR #{} is {}, expected open", pr.number, pr.state))
                    .resolution("get only supports open PRs")
            );
        }

        let head_repo = get_pr_repo(pr, "head")?;
        let base_repo = get_pr_repo(pr, "base")?;
        if head_repo.name_with_owner != github.repo {
            bail!(CliError::new(format!(
                "fork-backed PR #{} is unsupported for get: head repo is `{}`, expected `{}`",
                pr.number, head_repo.name_with_owner, github.repo
            )));
        }
        if base_repo.name_with_owner != github.repo {
            bail!(CliError::new(format!(
                "fork-backed PR #{} is unsupported for get: base repo is `{}`, expected `{}`",
                pr.number, base_repo.name_with_owner, github.repo
            )));
        }

        if index == 0 {
            if pr.base_ref_name != config.trunk {
                bail!(CliError::new(format!(
                    "stack topology mismatch for bottom PR #{}: base branch is `{}`, expected trunk `{}`",
                    pr.number, pr.base_ref_name, config.trunk
                )));
            }
        } else {
            let previous = &prs[index - 1];
            let previous_head_repo = get_pr_repo(previous, "head")?;
            if base_repo.name_with_owner != previous_head_repo.name_with_owner
                || pr.base_ref_name != previous.head_ref_name
            {
                bail!(CliError::new(format!(
                    "stack topology mismatch for PR #{}: base is `{}/{}`, expected previous PR #{} head `{}/{}`",
                    pr.number,
                    base_repo.name_with_owner,
                    pr.base_ref_name,
                    previous.number,
                    previous_head_repo.name_with_owner,
                    previous.head_ref_name
                )));
            }
            if pr.base_ref_oid != previous.head_ref_oid {
                bail!(CliError::new(format!(
                    "stack topology mismatch for PR #{}: base SHA is {}, expected previous PR #{} head SHA {}",
                    pr.number,
                    short_commit_id(&pr.base_ref_oid),
                    previous.number,
                    short_commit_id(&previous.head_ref_oid)
                )));
            }
        }
    }

    Ok(())
}

#[tracing::instrument(skip_all, fields(pr = pr.number))]
pub(crate) fn validate_get_pr_metadata(github: &GitHubContext, pr: &GhPr) -> Result<()> {
    if pr.number == 0 {
        bail!(
            "get PR metadata from {} has invalid PR number 0",
            github.repo
        );
    }
    for (field, value) in [
        ("id", pr.id.as_str()),
        ("state", pr.state.as_str()),
        ("headRefName", pr.head_ref_name.as_str()),
        ("baseRefName", pr.base_ref_name.as_str()),
        ("headRefOid", pr.head_ref_oid.as_str()),
        ("baseRefOid", pr.base_ref_oid.as_str()),
        ("title", pr.title.as_str()),
        ("createdAt", pr.created_at.as_str()),
    ] {
        if value.trim().is_empty() {
            bail!(
                "get PR metadata for {}/{} is missing required field `{}`",
                github.repo,
                pr.number,
                field
            );
        }
    }
    let author = pr.author.as_ref().with_context(|| {
        format!(
            "get PR metadata for {}/{} is missing required field `author`",
            github.repo, pr.number
        )
    })?;
    if author.login.trim().is_empty() {
        bail!(
            "get PR metadata for {}/{} is missing required field `author.login`",
            github.repo,
            pr.number
        );
    }
    for role in ["head", "base"] {
        let repo = get_pr_repo(pr, role)?;
        for (field, value) in [
            ("id", repo.id.as_str()),
            ("node_id", repo.node_id.as_str()),
            ("nameWithOwner", repo.name_with_owner.as_str()),
        ] {
            if value.trim().is_empty() {
                bail!(
                    "get PR metadata for {}/{} is missing required field `{}Repository.{}`",
                    github.repo,
                    pr.number,
                    role,
                    field
                );
            }
        }
    }
    Ok(())
}

#[tracing::instrument(level = "trace", skip_all, fields(pr = pr.number, role = %role))]
pub(crate) fn get_pr_repo<'a>(pr: &'a GhPr, role: &str) -> Result<&'a GhRepository> {
    match role {
        "head" => pr
            .head_repository
            .as_ref()
            .with_context(|| format!("PR #{} is missing head repository metadata", pr.number)),
        "base" => pr
            .base_repository
            .as_ref()
            .with_context(|| format!("PR #{} is missing base repository metadata", pr.number)),
        _ => unreachable!("invalid PR repository role"),
    }
}

pub(crate) async fn resolve_get_pr_changes(
    runner: &impl CommandRunner,
    prs: &[GhPr],
) -> Result<BTreeMap<u64, ResolvedChange>> {
    // Resolve each fetched PR head into a jj change concurrently. These are
    // independent read-only `jj log` lookups; the per-PR validation stays ordered.
    let stacks = stream::iter(prs.iter().map(|pr| resolve_stack(runner, &pr.head_ref_oid)))
        .buffered(NETWORK_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let mut changes = BTreeMap::new();
    for (pr, stack) in prs.iter().zip(stacks) {
        let stack = stack.with_context(|| {
            format!("resolve fetched PR #{} head {}", pr.number, pr.head_ref_oid)
        })?;
        let [change] = stack.as_slice() else {
            bail!(CliError::new(format!(
                "fetched PR #{} head {} resolved to {} changes, expected one",
                pr.number,
                short_commit_id(&pr.head_ref_oid),
                stack.len()
            )));
        };
        if change.commit_id != pr.head_ref_oid {
            bail!(CliError::new(format!(
                "fetched PR #{} head resolved to {}, expected {}",
                pr.number,
                short_commit_id(&change.commit_id),
                short_commit_id(&pr.head_ref_oid)
            )));
        }
        if change.conflict {
            bail!(CliError::new(format!(
                "fetched PR #{} head is conflicted at {} ({})",
                pr.number,
                short_change_id(&change.change_id),
                short_commit_id(&change.commit_id)
            )));
        }
        changes.insert(pr.number, change.clone());
    }
    Ok(changes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GetTarget {
    PullRequest { repo: String, number: u64 },
    BranchOrChange { repo: String, value: String },
}

impl GetTarget {
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn repo(&self) -> &str {
        match self {
            Self::PullRequest { repo, .. } | Self::BranchOrChange { repo, .. } => repo,
        }
    }
}

#[tracing::instrument(skip_all, fields(target = %target))]
pub(crate) fn parse_get_target(target: &str, default_repo: &str) -> Result<GetTarget> {
    let target = target.trim();
    if target.is_empty() {
        bail!(CliError::new("missing get target").resolution(
            "pass a PR number, GitHub pull request URL, branch name, or change id prefix"
        ));
    }

    if let Ok(number) = target.parse::<u64>() {
        return Ok(GetTarget::PullRequest {
            repo: default_repo.to_owned(),
            number,
        });
    }

    let Some(after_host) = target.split("github.com/").nth(1) else {
        return Ok(GetTarget::BranchOrChange {
            repo: default_repo.to_owned(),
            value: normalize_get_branch_target(target),
        });
    };
    let parts = after_host.split('/').collect::<Vec<_>>();
    if parts.len() < 4 || parts[2] != "pull" {
        bail!(
            CliError::new(format!("invalid get target `{target}`")).resolution(
                "pass a PR number, GitHub pull request URL, branch name, or change id prefix"
            )
        );
    }
    let number = parts[3]
        .split(|ch: char| !ch.is_ascii_digit())
        .next()
        .unwrap_or_default()
        .parse::<u64>()
        .with_context(|| format!("parse PR number from `{target}`"))?;
    Ok(GetTarget::PullRequest {
        repo: format!("{}/{}", parts[0], parts[1]),
        number,
    })
}

#[tracing::instrument(level = "trace", skip_all, fields(target = %target))]
pub(crate) fn normalize_get_branch_target(target: &str) -> String {
    target
        .strip_prefix("refs/heads/")
        .unwrap_or(target)
        .to_owned()
}

pub(crate) async fn resolve_get_target_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    target: GetTarget,
) -> Result<GhPr> {
    resolve_target_pr(runner, github, target, "get").await
}

/// Resolve a `pr` command target into `(pr_number, browser_url)`.
///
/// A bare PR number or PR URL needs no network round-trip; branch/change-id
/// targets (and the default current-change lookup) are resolved through `gh`.
pub(crate) async fn resolve_pr_url(
    runner: &impl CommandRunner,
    config: &AppConfig,
    github: &GitHubContext,
    target: Option<&str>,
) -> Result<(u64, String)> {
    match target {
        Some(target) => match parse_get_target(target, &github.repo)? {
            GetTarget::PullRequest { repo, number } => Ok((number, github_pr_url(&repo, number))),
            branch @ GetTarget::BranchOrChange { .. } => {
                let pr = resolve_target_pr(runner, github, branch, "pr").await?;
                Ok((pr.number, github_pr_url(&github.repo, pr.number)))
            }
        },
        None => {
            ensure_default_pr_target_is_stack_change(runner, config).await?;
            let change_id = current_change_id(runner).await?;
            let pr = lookup_get_target_pr(runner, github, &change_id, "pr").await.map_err(|_| {
                CliError::new("current revision is not submitted")
                    .reason(format!(
                        "current change `{}` has no open PR yet",
                        change_id_branch_prefix(&change_id)
                    ))
                    .resolution("run `forklift submit --yes`, then `forklift pr`")
            })?;
            Ok((pr.number, github_pr_url(&github.repo, pr.number)))
        }
    }
}

pub(crate) async fn ensure_default_pr_target_is_stack_change(
    runner: &impl CommandRunner,
    config: &AppConfig,
) -> Result<()> {
    let stack = resolve_stack(runner, DEFAULT_STACK_REVSET).await?;
    if !stack.is_empty() {
        return Ok(());
    }

    Err(CliError::new("no current PR")
        .reason(format!("current checkout is on trunk `{}`", config.trunk))
        .resolution("check out a stack change or pass a PR target")
        .into())
}

/// Change id of the current working-copy commit (`@`).
#[tracing::instrument(skip_all, fields(url = %url))]
pub(crate) async fn open_url(runner: &impl CommandRunner, url: &str) -> Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let output = runner.run(program, &args).await?;
    if !output.success {
        bail!(
            "failed to open browser: failed-command=`{}` error={}",
            display_command(program, &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

pub(crate) async fn resolve_target_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    target: GetTarget,
    purpose: &str,
) -> Result<GhPr> {
    match target {
        GetTarget::PullRequest { number, .. } => {
            fetch_pr_by_number(runner, github, purpose, number).await
        }
        GetTarget::BranchOrChange { value, .. } => {
            lookup_get_target_pr(runner, github, &value, purpose).await
        }
    }
}

#[tracing::instrument(skip_all, fields(target = %target))]
pub(crate) async fn lookup_get_target_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    target: &str,
    purpose: &str,
) -> Result<GhPr> {
    let prs = list_open_prs(runner, github, purpose).await?;
    let branch_matches = prs
        .iter()
        .filter(|pr| pr.head_ref_name == target)
        .cloned()
        .collect::<Vec<_>>();
    if !branch_matches.is_empty() {
        return one_get_target_match(runner, github, target, "branch", branch_matches, purpose)
            .await;
    }

    // Prefer jj's own prefix resolution: it expands a short, locally
    // unambiguous change id (e.g. `lt`) to a full id and errors on
    // ambiguity. Fall back to the literal >=8 char branch-prefix match so
    // PRs whose change isn't checked out locally still resolve.
    let prefix = match expand_local_change_id(runner, target).await {
        Some(change_id) => change_id_branch_prefix(&change_id).to_owned(),
        None => {
            let Some(prefix) = change_prefix_get_target(target) else {
                bail!(
                    CliError::new(format!(
                        "{purpose} target `{target}` did not match an open PR branch"
                    ))
                    .resolution(
                        "pass a PR number, PR URL, exact branch name, or a jj change id prefix \
                     (short prefixes work for locally checked-out changes; otherwise use at \
                     least 8 chars)"
                    )
                );
            };
            prefix
        }
    };
    let change_matches = prs
        .into_iter()
        .filter(|pr| head_branch_matches_change_prefix(&pr.head_ref_name, &prefix))
        .collect::<Vec<_>>();
    one_get_target_match(
        runner,
        github,
        target,
        "change id prefix",
        change_matches,
        purpose,
    )
    .await
}

#[tracing::instrument(skip_all, fields(purpose = %purpose))]
pub(crate) async fn list_open_prs(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    purpose: &str,
) -> Result<Vec<GhPr>> {
    let args = [
        "pr",
        "list",
        "--repo",
        github.repo.as_str(),
        "--state",
        "open",
        "--json",
        PR_JSON_FIELDS,
        "--limit",
        "200",
    ];
    let output = gh_run(runner, &args).await?;
    if !output.success {
        bail!(
            "`{}` failed while listing open PRs for {}: {}",
            display_command("gh", &args),
            purpose,
            output.stderr.trim()
        );
    }

    serde_json::from_str::<Vec<GhPr>>(&output.stdout)
        .with_context(|| format!("parse open PR list while resolving {purpose} target"))
}

#[tracing::instrument(level = "trace", skip_all, fields(target = %target, kind = %kind))]
pub(crate) async fn one_get_target_match(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    target: &str,
    kind: &str,
    matches: Vec<GhPr>,
    purpose: &str,
) -> Result<GhPr> {
    match matches.as_slice() {
        [] => bail!("{purpose} target `{target}` did not match an open PR {kind}"),
        [pr] => fetch_pr_by_number(runner, github, purpose, pr.number).await,
        _ => {
            let refs = matches
                .iter()
                .map(|pr| format!("#{} `{}`", pr.number, pr.head_ref_name))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("{purpose} target `{target}` matched multiple open PRs by {kind}: {refs}")
        }
    }
}

/// Expand a partial jj change id to its full change id using jj's native
/// prefix resolution.
///
/// Returns `None` when the target isn't a plausible change id, isn't present
/// in the local repo, or is ambiguous (jj resolves to zero or many commits).
/// In every `None` case the caller falls back to literal branch matching.
#[tracing::instrument(level = "trace", skip_all, fields(target = %target))]
pub(crate) async fn expand_local_change_id(
    runner: &impl CommandRunner,
    target: &str,
) -> Option<String> {
    if target.is_empty() || !target.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    let template = "change_id ++ \"\\n\"";
    let args = ["log", "--no-graph", "-r", target, "-T", template];
    let output = runner.run("jj", &args).await.ok()?;
    if !output.success {
        return None;
    }
    let mut ids = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = ids.next()?;
    if ids.next().is_some() {
        return None;
    }
    Some(first.to_owned())
}

#[tracing::instrument(level = "trace", skip_all, fields(target = %target))]
pub(crate) fn change_prefix_get_target(target: &str) -> Option<String> {
    if target.chars().count() < 8 || !target.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some(change_id_branch_prefix(target).to_owned())
}

#[tracing::instrument(level = "trace", skip_all, fields(branch = %branch, prefix = %prefix))]
pub(crate) fn head_branch_matches_change_prefix(branch: &str, prefix: &str) -> bool {
    let exact_suffix = format!("-{prefix}");
    if branch.ends_with(&exact_suffix) {
        return true;
    }

    let numbered_suffix = format!("-{prefix}-");
    branch
        .rsplit_once(&numbered_suffix)
        .is_some_and(|(_, suffix)| {
            !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
        })
}

#[tracing::instrument(skip_all, fields(change = %change_id, head_branch = %head_branch))]
pub(crate) async fn lookup_cached_pr(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    change_id: &str,
    head_branch: &str,
    entry: &PrCacheEntry,
) -> Result<PrCacheEntry> {
    if entry.head_branch != head_branch {
        bail!(CliError::new(format!(
            "conflicting cached PR for {}/{}: cache records head branch `{}`, deterministic head branch is `{}`",
            github.repo,
            short_change_id(change_id),
            entry.head_branch,
            head_branch
        )));
    }

    let pr = fetch_pr_by_number(runner, github, change_id, entry.pr_number).await?;
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        bail!(CliError::new(format!(
            "closed cached PR for {}/{}: cache points to {} PR #{} on `{}`",
            github.repo,
            short_change_id(change_id),
            pr.state,
            entry.pr_number,
            head_branch
        )));
    }

    if pr.head_ref_name != entry.head_branch {
        bail!(CliError::new(format!(
            "conflicting cached PR for {}/{}: cache points to PR #{} but GitHub reports head branch `{}`, expected `{}`",
            github.repo,
            short_change_id(change_id),
            entry.pr_number,
            pr.head_ref_name,
            entry.head_branch
        )));
    }
    validate_cached_pr_metadata(github, change_id, entry, &pr)?;

    Ok(pr.into_cache_entry(entry.stack_comment_id.clone()))
}

#[tracing::instrument(skip_all, fields(change = %change_id, pr = pr.number))]
pub(crate) fn validate_cached_pr_metadata(
    github: &GitHubContext,
    change_id: &str,
    entry: &PrCacheEntry,
    pr: &GhPr,
) -> Result<()> {
    let live = pr.clone().into_cache_entry(entry.stack_comment_id.clone());
    let required = [
        ("pr_node_id", entry.pr_node_id.as_str()),
        ("head_repo_id", entry.head_repo_id.as_str()),
        ("head_repo_node_id", entry.head_repo_node_id.as_str()),
        ("head_repo_name", entry.head_repo_name.as_str()),
        ("base_repo_id", entry.base_repo_id.as_str()),
        ("base_repo_node_id", entry.base_repo_node_id.as_str()),
        ("base_repo_name", entry.base_repo_name.as_str()),
        ("author_login", entry.author_login.as_str()),
    ];
    if let Some((field, _)) = required.iter().find(|(_, value)| value.trim().is_empty()) {
        bail!(
            CliError::new(format!(
                "PR for {}/{} is missing required metadata field `{}`",
                github.repo,
                short_change_id(change_id),
                field
            ))
            .resolution(format!(
                "run `forklift get {}`, or recreate the PR branch with forklift",
                github_pr_url(&github.repo, entry.pr_number)
            ))
        );
    }

    for (field, stored, live_value) in [
        (
            "pr_node_id",
            entry.pr_node_id.as_str(),
            live.pr_node_id.as_str(),
        ),
        (
            "head_repo_id",
            entry.head_repo_id.as_str(),
            live.head_repo_id.as_str(),
        ),
        (
            "head_repo_node_id",
            entry.head_repo_node_id.as_str(),
            live.head_repo_node_id.as_str(),
        ),
        (
            "head_repo_name",
            entry.head_repo_name.as_str(),
            live.head_repo_name.as_str(),
        ),
        (
            "base_repo_id",
            entry.base_repo_id.as_str(),
            live.base_repo_id.as_str(),
        ),
        (
            "base_repo_node_id",
            entry.base_repo_node_id.as_str(),
            live.base_repo_node_id.as_str(),
        ),
        (
            "base_repo_name",
            entry.base_repo_name.as_str(),
            live.base_repo_name.as_str(),
        ),
        (
            "author_login",
            entry.author_login.as_str(),
            live.author_login.as_str(),
        ),
        ("head_sha", entry.head_sha.as_str(), live.head_sha.as_str()),
        ("base_sha", entry.base_sha.as_str(), live.base_sha.as_str()),
        (
            "base_branch",
            entry.base_branch.as_str(),
            live.base_branch.as_str(),
        ),
    ] {
        if stored != live_value {
            bail!(CliError::new(format!(
                "GitHub PR metadata mismatch for {}/{} PR #{} field `{}`: cache has `{}`, GitHub has `{}`",
                github.repo,
                short_change_id(change_id),
                entry.pr_number,
                field,
                stored,
                live_value
            )));
        }
    }

    Ok(())
}

#[tracing::instrument(skip_all, fields(change = %change_id, pr = pr_number))]
pub(crate) async fn fetch_pr_by_number(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    change_id: &str,
    pr_number: u64,
) -> Result<GhPr> {
    let pr_number_string = pr_number.to_string();
    let endpoint = format!("repos/{}/pulls/{}", github.repo, pr_number);
    let args = ["api", endpoint.as_str(), "--jq", PR_API_JQ];
    let output = gh_run(runner, &args).await?;
    if !output.success {
        bail!(
            CliError::new(format!(
                "missing cached PR for {}/{}: cache points to PR #{} but gh could not load it",
                github.repo,
                short_change_id(change_id),
                pr_number_string
            ))
            .reason(output.stderr.trim().to_owned())
        );
    }

    serde_json::from_str(&output.stdout)
        .with_context(|| format!("parse GitHub PR #{} metadata", pr_number_string))
}

#[tracing::instrument(skip_all, fields(change = %change_id, head_branch = %head_branch))]
pub(crate) async fn lookup_open_pr_by_head_branch(
    runner: &impl CommandRunner,
    github: &GitHubContext,
    change_id: &str,
    head_branch: &str,
) -> Result<Option<PrCacheEntry>> {
    let args = [
        "pr",
        "list",
        "--repo",
        github.repo.as_str(),
        "--head",
        head_branch,
        "--state",
        "open",
        "--json",
        PR_JSON_FIELDS,
    ];
    let output = gh_run(runner, &args).await?;
    if !output.success {
        bail!(
            "`{}` failed while looking up open PR for {}/{}: {}",
            display_command("gh", &args),
            github.repo,
            change_id,
            output.stderr.trim()
        );
    }

    let prs = serde_json::from_str::<Vec<GhPr>>(&output.stdout)
        .with_context(|| format!("parse open PR lookup for branch `{head_branch}`"))?;
    match prs.as_slice() {
        [] => Ok(None),
        [pr] => {
            if !pr.state.eq_ignore_ascii_case("OPEN") {
                bail!(CliError::new(format!(
                    "conflicting PR lookup for {}/{}: branch `{}` returned {} PR #{} despite open-state lookup",
                    github.repo,
                    short_change_id(change_id),
                    head_branch,
                    pr.state,
                    pr.number
                )));
            }
            let pr = fetch_pr_by_number(runner, github, change_id, pr.number).await?;
            let comment_id = find_stack_comment_id(runner, github, pr.number, change_id).await;
            Ok(Some(pr.into_cache_entry(comment_id)))
        }
        _ => bail!(CliError::new(format!(
            "conflicting PR lookup for {}/{}: branch `{}` matched {} open PRs",
            github.repo,
            short_change_id(change_id),
            head_branch,
            prs.len()
        ))),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GhPr {
    pub(crate) number: u64,
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) merged: bool,
    #[serde(default)]
    pub(crate) id: String,
    #[serde(rename = "headRefName")]
    pub(crate) head_ref_name: String,
    #[serde(rename = "baseRefName")]
    pub(crate) base_ref_name: String,
    #[serde(rename = "headRefOid")]
    pub(crate) head_ref_oid: String,
    #[serde(rename = "baseRefOid")]
    pub(crate) base_ref_oid: String,
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default, deserialize_with = "null_to_default_string")]
    pub(crate) body: String,
    #[serde(default, rename = "createdAt")]
    pub(crate) created_at: String,
    #[serde(default, rename = "headRepository")]
    pub(crate) head_repository: Option<GhRepository>,
    #[serde(default, rename = "baseRepository")]
    pub(crate) base_repository: Option<GhRepository>,
    #[serde(default)]
    pub(crate) author: Option<GhAuthor>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GhRepository {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default, rename = "node_id")]
    pub(crate) node_id: String,
    #[serde(default, rename = "nameWithOwner")]
    pub(crate) name_with_owner: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GhAuthor {
    #[serde(default)]
    pub(crate) login: String,
}

impl GhPr {
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn into_cache_entry(self, stack_comment_id: Option<String>) -> PrCacheEntry {
        let head_repository = self.head_repository.unwrap_or_default();
        let base_repository = self.base_repository.unwrap_or_default();
        let author = self.author.unwrap_or_default();
        PrCacheEntry {
            pr_number: self.number,
            pr_node_id: self.id,
            head_branch: self.head_ref_name,
            base_branch: self.base_ref_name.clone(),
            base_ref: self.base_ref_name,
            head_repo_id: head_repository.id,
            head_repo_node_id: head_repository.node_id,
            head_repo_name: head_repository.name_with_owner,
            base_repo_id: base_repository.id,
            base_repo_node_id: base_repository.node_id,
            base_repo_name: base_repository.name_with_owner,
            head_sha: self.head_ref_oid,
            base_sha: self.base_ref_oid,
            author_login: author.login,
            title: self.title,
            body: self.body,
            created_at: self.created_at,
            stack_comment_id,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GhMergePr {
    pub(crate) number: u64,
    pub(crate) state: String,
    #[serde(rename = "headRefName")]
    pub(crate) head_ref_name: String,
    #[serde(rename = "baseRefName")]
    pub(crate) base_ref_name: String,
    #[serde(rename = "headRefOid")]
    pub(crate) head_ref_oid: String,
    #[serde(rename = "baseRefOid")]
    pub(crate) base_ref_oid: String,
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default, deserialize_with = "null_to_default_string")]
    pub(crate) body: String,
    #[serde(default, rename = "createdAt")]
    pub(crate) created_at: String,
    #[serde(default, rename = "isDraft")]
    pub(crate) is_draft: bool,
    #[serde(
        default,
        rename = "reviewDecision",
        deserialize_with = "empty_string_to_none"
    )]
    pub(crate) review_decision: Option<String>,
    #[serde(default)]
    pub(crate) mergeable: Option<String>,
    #[serde(default, rename = "mergeStateStatus")]
    pub(crate) merge_state_status: Option<String>,
    #[serde(default, rename = "statusCheckRollup")]
    pub(crate) status_check_rollup: Vec<serde_json::Value>,
    #[serde(default, rename = "autoMergeRequest")]
    pub(crate) auto_merge_request: Option<serde_json::Value>,
}

pub(crate) fn null_to_default_string<'de, D>(
    deserializer: D,
) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

impl GhMergePr {
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn into_cache_entry(self, stack_comment_id: Option<String>) -> PrCacheEntry {
        PrCacheEntry {
            pr_number: self.number,
            pr_node_id: String::new(),
            head_branch: self.head_ref_name,
            base_branch: self.base_ref_name.clone(),
            base_ref: self.base_ref_name,
            head_repo_id: String::new(),
            head_repo_node_id: String::new(),
            head_repo_name: String::new(),
            base_repo_id: String::new(),
            base_repo_node_id: String::new(),
            base_repo_name: String::new(),
            head_sha: self.head_ref_oid,
            base_sha: self.base_ref_oid,
            author_login: String::new(),
            title: self.title,
            body: self.body,
            created_at: self.created_at,
            stack_comment_id,
        }
    }
}
