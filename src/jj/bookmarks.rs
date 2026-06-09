use super::super::*;
use super::*;

pub(crate) fn update_get_frozen_bookmarks(
    runner: &impl CommandRunner,
    prs: &[GhPr],
    diagnostics: Diagnostics,
) -> Result<()> {
    if diagnostics.dry_run {
        for pr in prs {
            let bookmark = frozen_bookmark_name(pr.number);
            diagnostics.plan_line(&format!(
                "- set frozen bookmark {bookmark} -> {}",
                pr.head_ref_oid
            ));
        }
        return Ok(());
    }

    let existing = frozen_bookmarks(runner)?
        .into_iter()
        .map(|bookmark| (bookmark.pr_number, bookmark))
        .collect::<BTreeMap<_, _>>();
    for pr in prs {
        if let Some(bookmark) = existing.get(&pr.number)
            && bookmark.commit_id != pr.head_ref_oid
            && !git_commit_is_ancestor(runner, &bookmark.commit_id, &pr.head_ref_oid)?
        {
            bail!(
                CliError::new(format!(
                    "frozen bookmark `{}` points at {}, not an ancestor of fetched PR #{} head {}",
                    bookmark.name,
                    short_commit_id(&bookmark.commit_id),
                    pr.number,
                    short_commit_id(&pr.head_ref_oid)
                ))
                .resolution(
                    "delete or move the frozen bookmark manually after inspecting the rewrite"
                )
            );
        }
    }

    for pr in prs {
        let bookmark = frozen_bookmark_name(pr.number);
        let args = [
            "bookmark",
            "set",
            bookmark.as_str(),
            "-r",
            pr.head_ref_oid.as_str(),
        ];
        diagnostics.command("jj", &args);
        let output = runner.run("jj", &args)?;
        if !output.success {
            bail!(
                "failed-command=`{}` error={}",
                display_command("jj", &args),
                output.stderr.trim()
            );
        }
    }

    Ok(())
}

#[tracing::instrument(level = "trace", skip_all, fields(pr = pr_number))]
pub(crate) fn frozen_bookmark_name(pr_number: u64) -> String {
    format!("{FROZEN_BOOKMARK_PREFIX}{pr_number}")
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn git_commit_is_ancestor(
    runner: &impl CommandRunner,
    ancestor: &str,
    descendant: &str,
) -> Result<bool> {
    let args = ["merge-base", "--is-ancestor", ancestor, descendant];
    let output = git_run(runner, &args)?;
    Ok(output.success)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteBookmarkStatus {
    pub(crate) tracked: bool,
    pub(crate) conflicted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrozenBookmark {
    pub(crate) name: String,
    pub(crate) pr_number: u64,
    pub(crate) commit_id: String,
}

pub(crate) fn frozen_bookmarks(runner: &impl CommandRunner) -> Result<Vec<FrozenBookmark>> {
    let args = [
        "bookmark",
        "list",
        "forklift/frozen/*",
        "-T",
        FROZEN_BOOKMARK_TEMPLATE,
    ];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let mut bookmarks = Vec::new();
    for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 3 {
            bail!("parse frozen bookmark row `{line}`: expected 3 tab-separated fields");
        }
        let name = fields[0];
        if !name.starts_with(FROZEN_BOOKMARK_PREFIX) {
            continue;
        }
        let pr_number = name[FROZEN_BOOKMARK_PREFIX.len()..]
            .parse::<u64>()
            .with_context(|| format!("parse frozen bookmark `{name}` PR number"))?;
        if fields[1] == "conflicted" {
            bail!(
                CliError::new(format!("frozen bookmark `{name}` is conflicted"))
                    .resolution("resolve the bookmark conflict before continuing")
            );
        }
        let commit_id = fields[2].trim();
        if commit_id.is_empty() {
            bail!("frozen bookmark `{name}` has no target commit");
        }
        bookmarks.push(FrozenBookmark {
            name: name.to_owned(),
            pr_number,
            commit_id: commit_id.to_owned(),
        });
    }
    bookmarks.sort_by(|left, right| left.pr_number.cmp(&right.pr_number));
    Ok(bookmarks)
}

pub(crate) fn resolve_stack_resolution(
    runner: &impl CommandRunner,
    owned: Vec<ResolvedChange>,
    frozen_bookmarks: Vec<FrozenBookmark>,
) -> Result<StackResolution> {
    if frozen_bookmarks.is_empty() {
        return Ok(StackResolution {
            owned,
            frozen_dependencies: Vec::new(),
        });
    }

    let frozen_changes = resolve_frozen_changes(runner, frozen_bookmarks)?;
    let frozen_dependencies = frozen_dependencies_below_owned(&owned, frozen_changes)?;
    Ok(StackResolution {
        owned,
        frozen_dependencies,
    })
}

pub(crate) fn resolve_purely_frozen_stack(
    runner: &impl CommandRunner,
    frozen_bookmarks: Vec<FrozenBookmark>,
) -> Result<StackResolution> {
    if frozen_bookmarks.is_empty() {
        bail!(
            CliError::new("empty owned stack and no frozen bookmarks in scope")
                .resolution("run `forklift get <pr>` first, or move to a mutable stack")
        );
    }
    let at_commit =
        resolve_single_rev(runner, "@").context("resolve current revision for frozen sync")?;
    let frozen_changes = resolve_frozen_changes(runner, frozen_bookmarks)?;
    let Some(frozen_dependencies) = frozen_dependency_chain_ending_at(&frozen_changes, &at_commit)?
    else {
        bail!(CliError::new(format!(
            "empty owned stack and current revision {} is not a `forklift/frozen/pr-*` bookmark target",
            short_commit_id(&at_commit)
        ))
        .resolution("run `forklift get <pr>` first, or move to a mutable stack"));
    };

    Ok(StackResolution {
        owned: Vec::new(),
        frozen_dependencies,
    })
}

pub(crate) fn resolve_frozen_changes(
    runner: &impl CommandRunner,
    bookmarks: Vec<FrozenBookmark>,
) -> Result<Vec<FrozenDependency>> {
    let mut dependencies = Vec::new();
    for bookmark in bookmarks {
        let changes = resolve_stack(runner, &bookmark.commit_id)
            .with_context(|| format!("resolve frozen bookmark `{}`", bookmark.name))?;
        let [change] = changes.as_slice() else {
            bail!(CliError::new(format!(
                "frozen bookmark `{}` resolved to {} changes, expected one",
                bookmark.name,
                changes.len()
            )));
        };
        if change.commit_id != bookmark.commit_id {
            bail!(CliError::new(format!(
                "frozen bookmark `{}` points at {}, but jj resolved {}",
                bookmark.name,
                short_commit_id(&bookmark.commit_id),
                short_commit_id(&change.commit_id)
            )));
        }
        if change.conflict {
            bail!(CliError::new(format!(
                "frozen bookmark `{}` points at conflicted change {} ({})",
                bookmark.name,
                short_change_id(&change.change_id),
                short_commit_id(&change.commit_id)
            )));
        }
        dependencies.push(FrozenDependency {
            bookmark,
            change: change.clone(),
        });
    }
    Ok(dependencies)
}

#[tracing::instrument(skip_all, fields(top_commit = %top_commit))]
pub(crate) fn frozen_dependency_chain_ending_at(
    frozen: &[FrozenDependency],
    top_commit: &str,
) -> Result<Option<Vec<FrozenDependency>>> {
    let mut by_commit: HashMap<&str, &FrozenDependency> = HashMap::new();
    for dependency in frozen {
        if let Some(existing) = by_commit.insert(&dependency.change.commit_id, dependency) {
            bail!(CliError::new(format!(
                "multiple frozen bookmarks point at commit {}: `{}` and `{}`",
                short_commit_id(&dependency.change.commit_id),
                existing.bookmark.name,
                dependency.bookmark.name
            )));
        }
    }

    let Some(mut current) = by_commit.get(top_commit).copied() else {
        return Ok(None);
    };
    let mut seen = HashSet::new();
    let mut top_down = Vec::new();
    loop {
        if !seen.insert(current.change.commit_id.as_str()) {
            bail!(CliError::new(format!(
                "frozen dependency graph has a cycle at bookmark `{}`",
                current.bookmark.name
            )));
        }
        top_down.push(current);
        let frozen_parents = current
            .change
            .parent_ids
            .iter()
            .filter_map(|parent_id| by_commit.get(parent_id.as_str()).copied())
            .collect::<Vec<_>>();
        match frozen_parents.as_slice() {
            [] => break,
            [parent] => current = *parent,
            parents => bail!(CliError::new(format!(
                "frozen bookmark `{}` has {} frozen parents, expected a linear chain",
                current.bookmark.name,
                parents.len()
            ))),
        }
    }

    Ok(Some(top_down.into_iter().rev().cloned().collect()))
}

pub(crate) fn frozen_dependencies_below_owned(
    owned: &[ResolvedChange],
    frozen: Vec<FrozenDependency>,
) -> Result<Vec<FrozenDependency>> {
    if owned.is_empty() || frozen.is_empty() {
        return Ok(Vec::new());
    }

    let mut by_commit: HashMap<&str, &FrozenDependency> = HashMap::new();
    for dependency in &frozen {
        if let Some(existing) = by_commit.insert(&dependency.change.commit_id, dependency) {
            bail!(CliError::new(format!(
                "multiple frozen bookmarks point at commit {}: `{}` and `{}`",
                short_commit_id(&dependency.change.commit_id),
                existing.bookmark.name,
                dependency.bookmark.name
            )));
        }
    }

    let selected_commits = owned
        .iter()
        .map(|change| change.commit_id.as_str())
        .collect::<HashSet<_>>();
    let roots = owned
        .iter()
        .filter(|change| selected_parent(change, &selected_commits).is_none())
        .collect::<Vec<_>>();
    let [root] = roots.as_slice() else {
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
    };

    let nearest = root
        .parent_ids
        .iter()
        .filter_map(|parent_id| by_commit.get(parent_id.as_str()).copied())
        .collect::<Vec<_>>();
    let [nearest] = nearest.as_slice() else {
        if nearest.is_empty() {
            return Ok(Vec::new());
        }
        let boundary_labels = nearest
            .iter()
            .map(|dependency| {
                format!(
                    "`{}` at {}",
                    dependency.bookmark.name,
                    change_label(&dependency.change)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            CliError::new(format!(
                "multiple frozen boundaries below owned root {} ({})",
                short_change_id(&root.change_id),
                nearest.len()
            ))
            .reason(boundary_labels.clone())
            .resolution("run `forklift sync` from a single linear stack")
        );
    };

    let mut seen = HashSet::new();
    let mut top_down = Vec::new();
    let mut current = *nearest;
    loop {
        if !seen.insert(current.change.commit_id.as_str()) {
            bail!(CliError::new(format!(
                "frozen dependency graph has a cycle at bookmark `{}`",
                current.bookmark.name
            )));
        }
        top_down.push(current);

        let frozen_parents = current
            .change
            .parent_ids
            .iter()
            .filter_map(|parent_id| by_commit.get(parent_id.as_str()).copied())
            .collect::<Vec<_>>();
        match frozen_parents.as_slice() {
            [] => break,
            [parent] => current = *parent,
            parents => {
                let parent_labels = parents
                    .iter()
                    .map(|dependency| {
                        format!(
                            "`{}` at {}",
                            dependency.bookmark.name,
                            change_label(&dependency.change)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    CliError::new(format!(
                        "frozen bookmark `{}` has {} frozen parents, expected a linear chain",
                        current.bookmark.name,
                        parents.len()
                    ))
                    .reason(parent_labels.clone())
                )
            }
        }
    }

    Ok(top_down.into_iter().rev().cloned().collect())
}

#[tracing::instrument(skip_all, fields(branch = %branch))]
pub(crate) fn track_remote_bookmark(
    runner: &impl CommandRunner,
    config: &AppConfig,
    branch: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = [
        "bookmark",
        "track",
        "--remote",
        config.remote.as_str(),
        branch,
    ];
    if diagnostics.dry_run {
        diagnostics.plan_line(&format!("- {}", display_command("jj", &args)));
        return Ok(());
    }
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(bookmark = %bookmark))]
pub(crate) fn delete_bookmark(
    runner: &impl CommandRunner,
    bookmark: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let args = ["bookmark", "delete", bookmark];
    if diagnostics.dry_run {
        diagnostics.plan_line(&format!("- {}", display_command("jj", &args)));
        return Ok(());
    }
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteBookmark {
    pub(crate) name: String,
    pub(crate) remote: String,
    pub(crate) tracked: bool,
    pub(crate) conflicted: bool,
    pub(crate) commit_id: String,
}

pub(crate) fn remote_bookmarks(runner: &impl CommandRunner) -> Result<Vec<RemoteBookmark>> {
    let args = [
        "bookmark",
        "list",
        "--all-remotes",
        "-T",
        REMOTE_BOOKMARK_TEMPLATE,
    ];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let mut bookmarks = Vec::new();
    for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 5 {
            bail!("parse remote bookmark row `{line}`: expected 5 tab-separated fields");
        }
        let remote = fields[1].trim();
        if remote.is_empty() {
            continue;
        }
        bookmarks.push(RemoteBookmark {
            name: fields[0].to_owned(),
            remote: remote.to_owned(),
            tracked: fields[2] == "tracked",
            conflicted: fields[3] == "conflicted",
            commit_id: fields[4].trim().to_owned(),
        });
    }
    Ok(bookmarks)
}

#[tracing::instrument(skip_all, fields(rev = %rev))]
pub(crate) fn untracked_remote_bookmark_blockers(
    runner: &impl CommandRunner,
    config: &AppConfig,
    rev: &str,
    already_tracked_branch: &str,
) -> Result<Vec<RemoteBookmark>> {
    let mut blockers = Vec::new();
    for bookmark in remote_bookmarks(runner)? {
        if bookmark.remote != config.remote
            || bookmark.tracked
            || bookmark.conflicted
            || bookmark.name == already_tracked_branch
            || bookmark.commit_id.is_empty()
        {
            continue;
        }
        if git_commit_is_ancestor(runner, rev, &bookmark.commit_id)? {
            blockers.push(bookmark);
        }
    }
    blockers.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(blockers)
}

#[tracing::instrument(skip_all, fields(rev = %rev))]
pub(crate) fn track_untracked_remote_bookmark_blockers(
    runner: &impl CommandRunner,
    config: &AppConfig,
    rev: &str,
    already_tracked_branch: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let blockers = untracked_remote_bookmark_blockers(runner, config, rev, already_tracked_branch)?;
    if blockers.is_empty() {
        return Ok(());
    }

    for blocker in blockers {
        ui_warn!(
            "remote bookmark `{}@{}` keeps the target immutable; tracking it before adoption",
            blocker.name,
            blocker.remote
        );
        track_remote_bookmark(runner, config, &blocker.name, diagnostics)?;
    }

    Ok(())
}

#[tracing::instrument(skip_all, fields(rev = %rev))]
#[tracing::instrument(level = "trace", skip_all, fields(rev = %rev))]
pub(crate) fn jj_ref_commit_id(runner: &impl CommandRunner, rev: &str) -> Result<String> {
    run_required(
        runner,
        "jj",
        &["log", "--no-graph", "-r", rev, "-T", "commit_id"],
    )
    .with_context(|| format!("resolve jj revision `{rev}`"))
}

#[tracing::instrument(level = "trace", skip_all, fields(rev = %rev))]
pub(crate) fn jj_ref_change_id(runner: &impl CommandRunner, rev: &str) -> Result<String> {
    run_required(
        runner,
        "jj",
        &["log", "--no-graph", "-r", rev, "-T", "change_id"],
    )
    .with_context(|| format!("resolve change id for jj revision `{rev}`"))
}

#[tracing::instrument(skip_all, fields(branch = %branch))]
pub(crate) fn remote_bookmark_status(
    runner: &impl CommandRunner,
    config: &AppConfig,
    branch: &str,
) -> Result<RemoteBookmarkStatus> {
    let args = [
        "bookmark",
        "list",
        "--all-remotes",
        branch,
        "-T",
        BOOKMARK_STATUS_TEMPLATE,
    ];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let mut status = None;
    for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 3 || fields[0] != config.remote {
            continue;
        }
        status = Some(RemoteBookmarkStatus {
            tracked: fields[1] == "tracked",
            conflicted: fields[2] == "conflicted",
        });
        break;
    }

    status.with_context(|| {
        format!(
            "remote bookmark `{}@{}` is missing from jj; run `jj git fetch --remote {}` before submitting",
            branch, config.remote, config.remote
        )
    })
}

pub(crate) fn forget_bookmark_tracking(
    runner: &impl CommandRunner,
    branch: &str,
    diagnostics: Diagnostics,
) {
    if diagnostics.dry_run {
        return;
    }
    let args = ["bookmark", "forget", "--include-remotes", branch];
    diagnostics.command("jj", &args);
    let _ = runner.run("jj", &args);
}

/// Push the deletion of one or more bookmarks to the remote in a single
/// `jj git push` invocation.
pub(crate) fn push_bookmark_deletions(
    runner: &impl CommandRunner,
    config: &AppConfig,
    branches: &[&str],
    diagnostics: Diagnostics,
) -> Result<()> {
    let mut args: Vec<&str> = vec!["git", "push", "--remote", config.remote.as_str()];
    for branch in branches {
        args.push("--bookmark");
        args.push(branch);
    }
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

/// List local stack bookmarks (those under the configured branch prefix that
/// have no remote-only counterpart), regardless of which revision they point at.
pub(crate) fn local_stack_bookmarks(
    runner: &impl CommandRunner,
    config: &AppConfig,
) -> Result<Vec<String>> {
    let args = ["bookmark", "list", "-T", LOCAL_BOOKMARK_TEMPLATE];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let prefix = format!("{}/", config.branch_prefix.trim_end_matches('/'));
    let mut bookmarks = output
        .stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let remote = fields.next().unwrap_or_default().trim();
            if !remote.is_empty() || !name.starts_with(&prefix) {
                return None;
            }
            Some(name.to_owned())
        })
        .collect::<Vec<_>>();
    bookmarks.sort();
    bookmarks.dedup();
    Ok(bookmarks)
}
