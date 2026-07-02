use super::super::*;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppContext {
    pub(crate) github: GitHubContext,
    pub(crate) stack: Vec<ResolvedChange>,
    pub(crate) frozen_dependencies: Vec<FrozenDependency>,
}

impl AppContext {
    #[tracing::instrument(skip_all)]
    pub(crate) fn new(github: GitHubContext, stack_resolution: StackResolution) -> Self {
        Self {
            github,
            stack: stack_resolution.owned,
            frozen_dependencies: stack_resolution.frozen_dependencies,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedChange {
    pub(crate) change_id: String,
    pub(crate) commit_id: String,
    pub(crate) parent_ids: Vec<String>,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) tree_id: String,
    pub(crate) empty: bool,
    pub(crate) conflict: bool,
    pub(crate) divergent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrozenDependency {
    pub(crate) bookmark: FrozenBookmark,
    pub(crate) change: ResolvedChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StackResolution {
    pub(crate) owned: Vec<ResolvedChange>,
    pub(crate) frozen_dependencies: Vec<FrozenDependency>,
}

pub(crate) async fn resolve_stack(
    runner: &impl CommandRunner,
    revset: &str,
) -> Result<Vec<ResolvedChange>> {
    let output = runner.run(
        "jj",
        &[
            "log",
            "--no-graph",
            "--reversed",
            "-r",
            revset,
            "-T",
            STACK_LOG_TEMPLATE,
        ],
    ).await?;
    if !output.success {
        bail!(
            "`{}` failed: {}",
            display_command(
                "jj",
                &[
                    "log",
                    "--no-graph",
                    "--reversed",
                    "-r",
                    revset,
                    "-T",
                    STACK_LOG_TEMPLATE,
                ],
            ),
            output.stderr.trim()
        );
    }

    parse_stack_log(runner, &output.stdout).await.context("parse jj stack log")
}

pub(crate) async fn resolve_stack_context(
    runner: &impl CommandRunner,
    revset: &str,
) -> Result<AppContext> {
    resolve_single_rev(runner, "trunk()").await?;
    let frozen_bookmarks = frozen_bookmarks(runner).await?;
    let stack = resolve_stack(runner, revset).await?;
    validate_stack_shape(&stack, revset)?;
    let stack_resolution = resolve_stack_resolution(runner, stack, frozen_bookmarks).await?;
    let github = GitHubContext::resolve(runner).await?;

    Ok(AppContext::new(github, stack_resolution))
}

pub(crate) async fn resolve_single_rev(runner: &impl CommandRunner, rev: &str) -> Result<String> {
    let template = "commit_id ++ \"\\n\"";
    let args = ["log", "--no-graph", "-r", rev, "-T", template];
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    let commits = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    match commits.as_slice() {
        [commit] => Ok((*commit).to_owned()),
        [] => bail!("revset `{rev}` resolved to no commits; expected exactly one"),
        _ => bail!(
            "revset `{rev}` resolved to {} commits; expected exactly one",
            commits.len()
        ),
    }
}

pub(crate) async fn parse_stack_log(
    runner: &impl CommandRunner,
    stdout: &str,
) -> Result<Vec<ResolvedChange>> {
    let mut changes = Vec::new();
    for record in stdout
        .split(STACK_RECORD_SEPARATOR)
        .filter(|record| !record.is_empty())
    {
        changes.push(parse_stack_record(runner, record).await?);
    }
    Ok(changes)
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn parse_stack_record(
    runner: &impl CommandRunner,
    record: &str,
) -> Result<ResolvedChange> {
    let fields = record.split(STACK_FIELD_SEPARATOR).collect::<Vec<_>>();
    if fields.len() != 8 {
        bail!("expected 8 stack fields, got {}", fields.len());
    }

    let change_id = parse_json_string(fields[0], "change id")?;
    let commit_id = parse_json_string(fields[1], "commit id")?;
    let parent_ids = serde_json::from_str::<Vec<String>>(fields[2]).context("parse parent ids")?;
    let title = parse_json_string(fields[3], "title")?;
    let description = parse_json_string(fields[4], "description")?;
    let empty = serde_json::from_str::<bool>(fields[5]).context("parse empty status")?;
    let conflict = serde_json::from_str::<bool>(fields[6]).context("parse conflict status")?;
    let divergent = serde_json::from_str::<bool>(fields[7]).context("parse divergent status")?;
    let tree_id = resolve_tree_id(runner, &commit_id).await?;

    Ok(ResolvedChange {
        change_id,
        commit_id,
        parent_ids,
        body: description_body(&description, &title),
        title,
        tree_id,
        empty,
        conflict,
        divergent,
    })
}

#[tracing::instrument(level = "trace", skip_all, fields(field = %field))]
pub(crate) fn parse_json_string(value: &str, field: &str) -> Result<String> {
    serde_json::from_str(value).with_context(|| format!("parse {field}"))
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn description_body(description: &str, title: &str) -> String {
    let mut body = description.strip_prefix(title).unwrap_or(description);
    for _ in 0..2 {
        body = body.strip_prefix('\n').unwrap_or(body);
    }
    body.trim_end_matches('\n').to_owned()
}

#[tracing::instrument(level = "trace", skip_all, fields(commit = %commit_id))]
pub(crate) async fn resolve_tree_id(runner: &impl CommandRunner, commit_id: &str) -> Result<String> {
    git_run_required(runner, &["show", "-s", "--format=%T", commit_id])
        .await
        .with_context(|| format!("resolve tree id for commit {commit_id}"))
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn change_label(change: &ResolvedChange) -> String {
    format!(
        "{} ({})",
        short_change_id(&change.change_id),
        short_commit_id(&change.commit_id)
    )
}

/// Prints one non-fatal line per divergent change: its change ID resolves to
/// more than one visible commit, and forklift submits the copy under `@`. The
/// other copies are left untouched — whether to abandon them is the user's call.
/// The push phase re-points the PR bookmark onto the submitted copy regardless of
/// which copy it was stranded on, so divergence does not block submit.
pub(crate) fn warn_divergent_changes(stack: &[ResolvedChange]) {
    for change in stack.iter().filter(|change| change.divergent) {
        ui_warn_line(&format!(
            "change {} is divergent; submitting the copy under @ ({}), other copies left untouched",
            short_change_id(&change.change_id),
            short_commit_id(&change.commit_id),
        ));
    }
}

#[tracing::instrument(skip_all, fields(revset = %revset))]
pub(crate) fn validate_stack_shape(stack: &[ResolvedChange], revset: &str) -> Result<()> {
    if stack.is_empty() {
        bail!(
            CliError::new(format!("empty stack selected by `{revset}`"))
                .resolution("move to a non-empty owned stack")
        );
    }

    if let Some(change) = stack.iter().find(|change| change.empty) {
        bail!(
            CliError::new(format!("empty change {} selected", change_label(change)))
                .resolution("amend or abandon the empty change")
        );
    }

    if let Some(change) = stack.iter().find(|change| change.conflict) {
        bail!(
            CliError::new(format!(
                "conflicted change {} selected",
                change_label(change)
            ))
            .resolution("resolve conflicts")
        );
    }

    if let Some(change) = stack.iter().find(|change| change.parent_ids.len() > 1) {
        bail!(
            CliError::new(format!(
                "merge commit {} has {} parents",
                change_label(change),
                change.parent_ids.len()
            ))
            .resolution("forklift requires a linear owned stack")
        );
    }

    let selected_commits = stack
        .iter()
        .map(|change| change.commit_id.as_str())
        .collect::<HashSet<_>>();
    let roots = stack
        .iter()
        .filter(|change| selected_parent(change, &selected_commits).is_none())
        .collect::<Vec<_>>();

    if roots.len() != 1 {
        let root_labels = roots
            .iter()
            .map(|change| change_label(change))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            CliError::new(format!("stack has multiple roots ({})", roots.len()))
                .reason(root_labels.clone())
                .resolution("move to a single linear stack")
        );
    }

    let mut children_by_parent = HashMap::<&str, Vec<&ResolvedChange>>::new();
    for change in stack {
        if let Some(parent_id) = selected_parent(change, &selected_commits) {
            children_by_parent
                .entry(parent_id)
                .or_default()
                .push(change);
        }
    }

    if let Some((parent_id, children)) = children_by_parent
        .iter()
        .find(|(_, children)| children.len() > 1)
    {
        let child_labels = children
            .iter()
            .map(|change| change_label(change))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            CliError::new(format!(
                "siblings selected under parent {} ({} children)",
                short_commit_id(parent_id),
                children.len()
            ))
            .reason(child_labels.clone())
            .resolution("move to one linear branch")
        );
    }

    Ok(())
}

/// Refuse to submit/sync when the owned stack root sits on a commit that is
/// neither in trunk nor a frozen dependency. Without this guard forklift would
/// silently treat such a stack as trunk-based: submit would plan the bottom PR
/// with `base=trunk` (a bloated diff carrying the intervening un-merged commits)
/// and sync would plan `jj rebase -s <root> -d trunk`, dropping those un-merged
/// parents from the ancestry. The dangerous parent is typically the head of a
/// separately-submitted but still-open PR that was never `forklift get`-frozen.
///
/// Detection is by ancestry, not parent adjacency, so empty spacer commits
/// (e.g. a leftover `jj new` between trunk and the root) and the frozen-dep
/// chain are correctly ignored: any *non-empty* ancestor of the root that is
/// not in `::trunk()` and not a frozen dependency is the dangerous case.
pub(crate) async fn validate_owned_base(
    runner: &impl CommandRunner,
    resolution: &StackResolution,
) -> Result<()> {
    if resolution.owned.is_empty() {
        return Ok(());
    }
    // When a frozen dependency covers the owned root, submit bases the bottom PR
    // on that dependency's branch and sync rebases the root onto it — never onto
    // trunk — and the frozen-dependency checks own the chain below it. Only the
    // no-frozen-dependency case falls back to trunk, which is the destructive
    // path this guard exists to stop.
    if !resolution.frozen_dependencies.is_empty() {
        return Ok(());
    }
    let root = stack_root(&resolution.owned)?;

    // Any non-empty ancestor of the root that is not already in trunk is an
    // un-merged commit the stack is built on. Empty spacer commits (e.g. a
    // leftover `jj new` between trunk and the root) are excluded by `~empty()`,
    // so detection is by ancestry rather than parent adjacency.
    let revset = format!(
        "(::{root} ~ ::trunk() ~ {root}) & ~empty()",
        root = root.commit_id
    );

    let dangerous = list_commit_ids(runner, &revset)
        .await
        .context("inspect ancestry below owned stack root for un-merged commits")?;
    if let Some(parent) = dangerous.first() {
        // Keep the diagnosis in the message (not `.reason()`): when this bubbles
        // through `phase_error` the message becomes the displayed reason, so a
        // separate `.reason()` would be dropped.
        bail!(
            CliError::new(format!(
                "owned stack root {} is based on un-merged commit {} which is not a frozen dependency; forklift would otherwise silently base the bottom PR on trunk and drop the intervening un-merged commits",
                change_label(root),
                short_commit_id(parent)
            ))
            .resolution(
                "run `forklift get <pr>` to depend on its PR, or rebase onto trunk if it has already merged"
            )
        );
    }

    Ok(())
}

pub(crate) fn selected_parent<'a>(
    change: &'a ResolvedChange,
    selected_commits: &HashSet<&'a str>,
) -> Option<&'a str> {
    change
        .parent_ids
        .iter()
        .map(String::as_str)
        .find(|parent_id| selected_commits.contains(parent_id))
}

pub(crate) fn print_github_context(github: &GitHubContext) {
    eprintln!(
        "resolved github: repo={}, username={}",
        github.repo, github.username
    );
}

pub(crate) fn print_stack(stack: &[ResolvedChange]) {
    eprintln!("resolved stack table: {} changes", stack.len());
    eprintln!("change\tcommit\ttitle");
    for change in stack {
        eprintln!(
            "{}\t{}\t{}{}",
            change.change_id,
            change.commit_id,
            change.title,
            if change.conflict { " [conflict]" } else { "" }
        );
    }
}
