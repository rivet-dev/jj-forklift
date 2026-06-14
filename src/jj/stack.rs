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

pub(crate) fn resolve_stack(
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
    )?;
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

    parse_stack_log(runner, &output.stdout).context("parse jj stack log")
}

pub(crate) fn resolve_stack_context(
    runner: &impl CommandRunner,
    revset: &str,
) -> Result<AppContext> {
    resolve_single_rev(runner, "trunk()")?;
    let frozen_bookmarks = frozen_bookmarks(runner)?;
    let stack = resolve_stack(runner, revset)?;
    validate_stack_shape(runner, &stack, revset)?;
    let stack_resolution = resolve_stack_resolution(runner, stack, frozen_bookmarks)?;
    let github = GitHubContext::resolve(runner)?;

    Ok(AppContext::new(github, stack_resolution))
}

pub(crate) fn resolve_single_rev(runner: &impl CommandRunner, rev: &str) -> Result<String> {
    let template = "commit_id ++ \"\\n\"";
    let args = ["log", "--no-graph", "-r", rev, "-T", template];
    let output = runner.run("jj", &args)?;
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

pub(crate) fn parse_stack_log(
    runner: &impl CommandRunner,
    stdout: &str,
) -> Result<Vec<ResolvedChange>> {
    stdout
        .split(STACK_RECORD_SEPARATOR)
        .filter(|record| !record.is_empty())
        .map(|record| parse_stack_record(runner, record))
        .collect()
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn parse_stack_record(
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
    let tree_id = resolve_tree_id(runner, &commit_id)?;

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
pub(crate) fn resolve_tree_id(runner: &impl CommandRunner, commit_id: &str) -> Result<String> {
    git_run_required(runner, &["show", "-s", "--format=%T", commit_id])
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
pub(crate) fn validate_stack_shape(
    runner: &impl CommandRunner,
    stack: &[ResolvedChange],
    revset: &str,
) -> Result<()> {
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

        // The most common cause of phantom roots is an empty change wedged in the
        // middle of the stack: the revset's `~empty()` drops it, severing the
        // parent link so the change above it looks like a second root. Name the
        // empty change instead of the misleading "multiple roots".
        let bridges = empty_changes_bridging_roots(runner, stack, &selected_commits);
        if !bridges.is_empty() {
            let noun = if bridges.len() == 1 {
                "change"
            } else {
                "changes"
            };
            let verb = if bridges.len() == 1 { "is" } else { "are" };
            let bridge_labels = bridges
                .iter()
                .map(change_label)
                .collect::<Vec<_>>()
                .join(", ");
            let abandon_ids = bridges
                .iter()
                .map(|change| short_change_id(&change.change_id))
                .collect::<Vec<_>>()
                .join(" ");
            // The headline is replaced by `phase_error` ("could not resolve
            // stack ..."), so the actionable detail must live in the reason and
            // resolution, which survive: name the empty change and how to fix it.
            bail!(
                CliError::new(format!(
                    "empty {noun} {bridge_labels} wedged in the middle of the stack"
                ))
                .reason(format!(
                    "empty {noun} {bridge_labels} {verb} wedged in the middle of the stack — forklift skips empty changes, which splits the otherwise-linear stack into separate roots ({root_labels})"
                ))
                .resolution(format!(
                    "abandon or amend the empty {noun} (most likely leftover after a rebase or squash), then rerun — e.g. `jj abandon {abandon_ids}`"
                ))
            );
        }

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

/// Find empty changes that sit *between* two selected stack changes.
///
/// The stack revset excludes empty changes, so an empty change wedged mid-stack
/// severs the parent link and makes the change above it look like a second root.
/// For each root, walk from its excluded parent down through contiguous empty
/// changes; keep the empties only when the walk reconnects to another selected
/// change (proving the empty is wedged *between* real changes, not trailing
/// below the stack). Best-effort: any jj lookup failure just falls back to the
/// generic "multiple roots" error.
fn empty_changes_bridging_roots(
    runner: &impl CommandRunner,
    stack: &[ResolvedChange],
    selected_commits: &HashSet<&str>,
) -> Vec<ResolvedChange> {
    let trunk = resolve_single_rev(runner, "trunk()").ok();
    let mut bridges = Vec::new();
    let mut reported = HashSet::new();
    let mut visited = HashSet::new();
    for change in stack {
        if selected_parent(change, selected_commits).is_some() {
            continue;
        }
        for parent_id in &change.parent_ids {
            if selected_commits.contains(parent_id.as_str())
                || trunk.as_deref() == Some(parent_id.as_str())
            {
                continue;
            }
            let mut chain: Vec<ResolvedChange> = Vec::new();
            let mut current = parent_id.clone();
            loop {
                if selected_commits.contains(current.as_str()) {
                    for empty in chain.drain(..) {
                        if reported.insert(empty.commit_id.clone()) {
                            bridges.push(empty);
                        }
                    }
                    break;
                }
                if trunk.as_deref() == Some(current.as_str()) || !visited.insert(current.clone()) {
                    break;
                }
                let Some(resolved) = resolve_stack(runner, &current)
                    .ok()
                    .and_then(|changes| changes.into_iter().next())
                else {
                    break;
                };
                if !resolved.empty {
                    break;
                }
                let next = resolved.parent_ids.first().cloned();
                chain.push(resolved);
                match next {
                    Some(parent) => current = parent,
                    None => break,
                }
            }
        }
    }
    bridges
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
