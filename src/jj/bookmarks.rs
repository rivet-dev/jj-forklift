use super::super::*;
use super::*;

pub(crate) async fn update_get_frozen_bookmarks(
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

    let existing = frozen_bookmarks(runner)
        .await?
        .into_iter()
        .map(|bookmark| (bookmark.pr_number, bookmark))
        .collect::<BTreeMap<_, _>>();
    for pr in prs {
        if let Some(bookmark) = existing.get(&pr.number)
            && bookmark.commit_id != pr.head_ref_oid
            && !git_commit_is_ancestor(runner, &bookmark.commit_id, &pr.head_ref_oid).await?
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
        let output = runner.run("jj", &args).await?;
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
pub(crate) async fn git_commit_is_ancestor(
    runner: &impl CommandRunner,
    ancestor: &str,
    descendant: &str,
) -> Result<bool> {
    let args = ["merge-base", "--is-ancestor", ancestor, descendant];
    let output = git_run(runner, &args).await?;
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

pub(crate) async fn frozen_bookmarks(runner: &impl CommandRunner) -> Result<Vec<FrozenBookmark>> {
    let args = [
        "bookmark",
        "list",
        "forklift/frozen/*",
        "-T",
        FROZEN_BOOKMARK_TEMPLATE,
    ];
    let output = runner.run("jj", &args).await?;
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

pub(crate) async fn resolve_stack_resolution(
    runner: &impl CommandRunner,
    owned: Vec<ResolvedChange>,
    frozen_bookmarks: Vec<FrozenBookmark>,
) -> Result<StackResolution> {
    let frozen_dependencies = if frozen_bookmarks.is_empty() {
        Vec::new()
    } else {
        let frozen_changes = resolve_frozen_changes(runner, frozen_bookmarks).await?;
        frozen_dependencies_below_owned(runner, &owned, frozen_changes).await?
    };
    let resolution = StackResolution {
        owned,
        frozen_dependencies,
    };
    validate_owned_base(runner, &resolution).await?;
    Ok(resolution)
}

pub(crate) async fn resolve_purely_frozen_stack(
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
        resolve_single_rev(runner, "@").await.context("resolve current revision for frozen sync")?;
    let frozen_changes = resolve_frozen_changes(runner, frozen_bookmarks).await?;
    if let Some(frozen_dependencies) =
        frozen_dependency_chain_ending_at(runner, &frozen_changes, &at_commit).await?
    {
        return Ok(StackResolution {
            owned: Vec::new(),
            frozen_dependencies,
        });
    }

    let current = resolve_stack(runner, "@").await.context("resolve current revision for frozen sync")?;
    if let [current] = current.as_slice() {
        if current.empty {
            if let [parent] = current.parent_ids.as_slice() {
                if let Some(frozen_dependencies) =
                    frozen_dependency_chain_ending_at(runner, &frozen_changes, parent).await?
                {
                    return Ok(StackResolution {
                        owned: Vec::new(),
                        frozen_dependencies,
                    });
                }
            }
        }
    };

    bail!(CliError::new(format!(
        "empty owned stack and current revision {} is not a `forklift/frozen/pr-*` bookmark target or empty child of one",
        short_commit_id(&at_commit)
    ))
    .resolution("run `forklift get <pr>` first, or move to a mutable stack"));
}

pub(crate) async fn resolve_frozen_changes(
    runner: &impl CommandRunner,
    bookmarks: Vec<FrozenBookmark>,
) -> Result<Vec<FrozenDependency>> {
    let mut dependencies = Vec::new();
    for bookmark in bookmarks {
        let changes = resolve_stack(runner, &bookmark.commit_id)
            .await
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
pub(crate) async fn frozen_dependency_chain_ending_at(
    runner: &impl CommandRunner,
    frozen: &[FrozenDependency],
    top_commit: &str,
) -> Result<Option<Vec<FrozenDependency>>> {
    let by_commit = frozen_by_commit(frozen)?;

    let Some(mut current) = by_commit.get(top_commit).copied() else {
        return Ok(None);
    };
    let commit_ids = frozen
        .iter()
        .map(|dependency| dependency.change.commit_id.as_str())
        .collect::<Vec<_>>();
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
        // The next dependency below is the nearest *frozen ancestor*, not the
        // immediate parent: a multi-commit PR puts its own intra-PR commits
        // between its frozen head and the next PR's head.
        let frozen_parents = nearest_frozen_ancestors(runner, &commit_ids, &current.change.commit_id).await?;
        match frozen_parents.as_slice() {
            [] => break,
            [parent] => {
                current = by_commit.get(parent.as_str()).copied().with_context(|| {
                    format!("frozen ancestor {} missing from dependency set", short_commit_id(parent))
                })?;
            }
            parents => {
                let labels = parents
                    .iter()
                    .filter_map(|commit| by_commit.get(commit.as_str()).copied())
                    .map(|dependency| {
                        format!("`{}` at {}", dependency.bookmark.name, change_label(&dependency.change))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    CliError::new(format!(
                        "frozen bookmark `{}` has {} frozen ancestors, expected a linear chain",
                        current.bookmark.name,
                        parents.len()
                    ))
                    .reason(labels)
                );
            }
        }
    }

    Ok(Some(top_down.into_iter().rev().cloned().collect()))
}

/// Index frozen dependencies by their commit id, rejecting two bookmarks on the
/// same commit.
fn frozen_by_commit(frozen: &[FrozenDependency]) -> Result<HashMap<&str, &FrozenDependency>> {
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
    Ok(by_commit)
}

/// Commit ids of the nearest frozen commit(s) that are strict ancestors of
/// `head`. More than one means the frozen graph branches below `head` (not a
/// linear chain); none means `head` is the bottom of the frozen stack. Walking
/// by ancestry — rather than the immediate parent — bridges the intra-PR commits
/// of a multi-commit frozen PR.
async fn nearest_frozen_ancestors(
    runner: &impl CommandRunner,
    frozen_commit_ids: &[&str],
    head: &str,
) -> Result<Vec<String>> {
    let set = frozen_commit_ids
        .iter()
        .map(|commit| format!("present({commit})"))
        .collect::<Vec<_>>()
        .join(" | ");
    let revset = format!("heads(({set}) & ::{head} & ~{head})");
    let args = ["log", "--no-graph", "-r", &revset, "-T", "commit_id ++ \"\\n\""];
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

pub(crate) async fn frozen_dependencies_below_owned(
    runner: &impl CommandRunner,
    owned: &[ResolvedChange],
    frozen: Vec<FrozenDependency>,
) -> Result<Vec<FrozenDependency>> {
    if owned.is_empty() || frozen.is_empty() {
        return Ok(Vec::new());
    }

    // Validate up front: no two frozen bookmarks on one commit.
    frozen_by_commit(&frozen)?;

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

    // The frozen boundary below the owned root is its nearest frozen ancestor —
    // found by ancestry so a multi-commit frozen PR (whose head is several
    // commits above its own base) is still recognized as the boundary.
    let commit_ids = frozen
        .iter()
        .map(|dependency| dependency.change.commit_id.as_str())
        .collect::<Vec<_>>();
    let nearest = nearest_frozen_ancestors(runner, &commit_ids, &root.commit_id).await?;
    let boundary = match nearest.as_slice() {
        [] => return Ok(Vec::new()),
        [boundary] => boundary.clone(),
        boundaries => {
            let by_commit = frozen_by_commit(&frozen)?;
            let boundary_labels = boundaries
                .iter()
                .filter_map(|commit| by_commit.get(commit.as_str()).copied())
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
                    boundaries.len()
                ))
                .reason(boundary_labels)
                .resolution("run `forklift sync` from a single linear stack")
            );
        }
    };

    frozen_dependency_chain_ending_at(runner, &frozen, &boundary).await?.with_context(|| {
        format!(
            "frozen boundary {} resolved no dependency chain",
            short_commit_id(&boundary)
        )
    })
}

#[tracing::instrument(skip_all, fields(branch = %branch))]
pub(crate) async fn track_remote_bookmark(
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
    let output = runner.run("jj", &args).await?;
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
pub(crate) async fn delete_bookmark(
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
    let output = runner.run("jj", &args).await?;
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

pub(crate) async fn remote_bookmarks(runner: &impl CommandRunner) -> Result<Vec<RemoteBookmark>> {
    let args = [
        "bookmark",
        "list",
        "--all-remotes",
        "-T",
        REMOTE_BOOKMARK_TEMPLATE,
    ];
    let output = runner.run("jj", &args).await?;
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
pub(crate) async fn untracked_remote_bookmark_blockers(
    runner: &impl CommandRunner,
    config: &AppConfig,
    rev: &str,
    already_tracked_branch: &str,
) -> Result<Vec<RemoteBookmark>> {
    let mut blockers = Vec::new();
    for bookmark in remote_bookmarks(runner).await? {
        if bookmark.remote != config.remote
            || bookmark.tracked
            || bookmark.conflicted
            || bookmark.name == already_tracked_branch
            || bookmark.commit_id.is_empty()
        {
            continue;
        }
        if git_commit_is_ancestor(runner, rev, &bookmark.commit_id).await? {
            blockers.push(bookmark);
        }
    }
    blockers.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(blockers)
}

#[tracing::instrument(skip_all, fields(rev = %rev))]
pub(crate) async fn track_untracked_remote_bookmark_blockers(
    runner: &impl CommandRunner,
    config: &AppConfig,
    rev: &str,
    already_tracked_branch: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let blockers = untracked_remote_bookmark_blockers(runner, config, rev, already_tracked_branch).await?;
    if blockers.is_empty() {
        return Ok(());
    }

    for blocker in blockers {
        ui_warn!(
            "remote bookmark `{}@{}` keeps the target immutable; tracking it before adoption",
            blocker.name,
            blocker.remote
        );
        track_remote_bookmark(runner, config, &blocker.name, diagnostics).await?;
    }

    Ok(())
}

#[tracing::instrument(skip_all, fields(rev = %rev))]
#[tracing::instrument(level = "trace", skip_all, fields(rev = %rev))]
pub(crate) async fn jj_ref_commit_id(runner: &impl CommandRunner, rev: &str) -> Result<String> {
    run_required(
        runner,
        "jj",
        &["log", "--no-graph", "-r", rev, "-T", "commit_id"],
    )
    .await
    .with_context(|| format!("resolve jj revision `{rev}`"))
}

#[tracing::instrument(level = "trace", skip_all, fields(rev = %rev))]
pub(crate) async fn jj_ref_change_id(runner: &impl CommandRunner, rev: &str) -> Result<String> {
    run_required(
        runner,
        "jj",
        &["log", "--no-graph", "-r", rev, "-T", "change_id"],
    )
    .await
    .with_context(|| format!("resolve change id for jj revision `{rev}`"))
}

#[tracing::instrument(skip_all, fields(branch = %branch))]
pub(crate) async fn remote_bookmark_status(
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
    let output = runner.run("jj", &args).await?;
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

pub(crate) async fn forget_bookmark_tracking(
    runner: &impl CommandRunner,
    branch: &str,
    diagnostics: Diagnostics,
) {
    if diagnostics.dry_run {
        return;
    }
    let args = ["bookmark", "forget", "--include-remotes", branch];
    diagnostics.command("jj", &args);
    let _ = runner.run("jj", &args).await;
}

/// Push the deletion of one or more bookmarks to the remote in a single
/// `jj git push` invocation.
pub(crate) async fn push_bookmark_deletions(
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
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    Ok(())
}

/// Local `<prefix>/*` bookmarks that are in a conflicted state. Unlike
/// [`local_stack_bookmarks`], which deliberately skips conflicted bookmarks,
/// this returns exactly those so cleanup can prune the ones whose change has
/// already landed (a common residue of a squash/abandon collapsing several
/// bookmarks onto one surviving commit).
pub(crate) async fn conflicted_local_stack_bookmarks(
    runner: &impl CommandRunner,
    config: &AppConfig,
) -> Result<Vec<String>> {
    let args = ["bookmark", "list", "-T", LOCAL_BOOKMARK_TEMPLATE];
    let output = runner.run("jj", &args).await?;
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
            let status = fields.next().unwrap_or_default().trim();
            if !remote.is_empty() || !name.starts_with(&prefix) || status != "conflicted" {
                return None;
            }
            Some(name.to_owned())
        })
        .collect::<Vec<_>>();
    bookmarks.sort();
    bookmarks.dedup();
    Ok(bookmarks)
}

/// List every resolvable local bookmark name, regardless of prefix or target.
pub(crate) async fn local_bookmark_names(runner: &impl CommandRunner) -> Result<Vec<String>> {
    let args = ["bookmark", "list", "-T", LOCAL_BOOKMARK_TEMPLATE];
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    let mut bookmarks = output
        .stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let remote = fields.next().unwrap_or_default().trim();
            let status = fields.next().unwrap_or_default().trim();
            let target = fields.next().unwrap_or_default().trim();
            if !remote.is_empty()
                || status == "conflicted"
                || !is_resolvable_bookmark_target(target)
            {
                return None;
            }
            Some(name.to_owned())
        })
        .collect::<Vec<_>>();
    bookmarks.sort();
    bookmarks.dedup();
    Ok(bookmarks)
}

/// List local stack bookmarks (those under the configured branch prefix that
/// have no remote-only counterpart), regardless of which revision they point at.
pub(crate) async fn local_stack_bookmarks(
    runner: &impl CommandRunner,
    config: &AppConfig,
) -> Result<Vec<String>> {
    let args = ["bookmark", "list", "-T", LOCAL_BOOKMARK_TEMPLATE];
    let output = runner.run("jj", &args).await?;
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
            let status = fields.next().unwrap_or_default().trim();
            let target = fields.next().unwrap_or_default().trim();
            if !remote.is_empty()
                || !name.starts_with(&prefix)
                || status == "conflicted"
                || !is_resolvable_bookmark_target(target)
            {
                return None;
            }
            Some(name.to_owned())
        })
        .collect::<Vec<_>>();
    bookmarks.sort();
    bookmarks.dedup();
    Ok(bookmarks)
}

pub(crate) fn is_resolvable_bookmark_target(target: &str) -> bool {
    !target.is_empty() && !target.starts_with("<Error:")
}
