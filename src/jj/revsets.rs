use super::super::*;
use super::*;

pub(crate) fn effective_merge_revset(
    runner: &impl CommandRunner,
    target: Option<&str>,
) -> Result<MergeRevset> {
    let Some(target) = target else {
        return Ok(MergeRevset {
            revset: DEFAULT_STACK_REVSET.to_owned(),
            target: None,
        });
    };
    let target = resolve_merge_target(runner, target)?;
    Ok(MergeRevset {
        revset: merge_revset_for_target(&target.commit_id),
        target: Some(target),
    })
}

pub(crate) fn merge_revset_for_target(target_commit: &str) -> String {
    format!(
        "trunk()..{} & ~::(immutable_heads() | root()) & ~empty()",
        target_commit
    )
}

pub(crate) fn effective_sync_revset(
    runner: &impl CommandRunner,
    target: Option<&str>,
) -> Result<SyncRevset> {
    let Some(target) = target else {
        return Ok(SyncRevset {
            revset: DEFAULT_STACK_REVSET.to_owned(),
            target: None,
        });
    };
    let target = resolve_merge_target(runner, target)?;
    Ok(SyncRevset {
        revset: merge_revset_for_target(&target.commit_id),
        target: Some(target),
    })
}

pub(crate) fn merge_sync_command(target: Option<&str>) -> String {
    match target {
        Some(target) => format!("forklift merge {target} --sync"),
        None => "forklift merge --sync".to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeRevset {
    pub(crate) revset: String,
    pub(crate) target: Option<MergeTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncRevset {
    pub(crate) revset: String,
    pub(crate) target: Option<MergeTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeTarget {
    pub(crate) input: String,
    pub(crate) commit_id: String,
    pub(crate) pr_number: Option<u64>,
}

impl MergeTarget {
    pub(crate) fn label(&self) -> String {
        self.pr_number
            .map(|number| format!("PR #{number}"))
            .unwrap_or_else(|| format!("merge target `{}`", self.input))
    }
}

pub(crate) fn resolve_merge_target(
    runner: &impl CommandRunner,
    target: &str,
) -> Result<MergeTarget> {
    let mut github = GitHubContext::resolve(runner)
        .context("resolve GitHub repository before resolving merge target")?;
    let parsed = parse_get_target(target, &github.repo)?;
    github.repo = parsed.repo().to_owned();

    match &parsed {
        GetTarget::PullRequest { .. } => {
            let pr = resolve_target_pr(runner, &github, parsed, "merge")?;
            Ok(MergeTarget {
                input: target.to_owned(),
                commit_id: pr.head_ref_oid,
                pr_number: Some(pr.number),
            })
        }
        GetTarget::BranchOrChange { .. } => {
            match resolve_target_pr(runner, &github, parsed, "merge") {
                Ok(pr) => Ok(MergeTarget {
                    input: target.to_owned(),
                    commit_id: pr.head_ref_oid,
                    pr_number: Some(pr.number),
                }),
                Err(pr_error) => {
                    let commit_id = resolve_single_rev(runner, target).with_context(|| {
                        format!(
                            "merge target `{target}` did not resolve as an open PR target; PR lookup failed with: {pr_error:#}"
                        )
                    })?;
                    Ok(MergeTarget {
                        input: target.to_owned(),
                        commit_id,
                        pr_number: None,
                    })
                }
            }
        }
    }
}

pub(crate) fn current_change_id(runner: &impl CommandRunner) -> Result<String> {
    let template = "change_id ++ \"\\n\"";
    let args = ["log", "--no-graph", "-r", "@", "-T", template];
    let output = runner.run("jj", &args)?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={}",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }
    output
        .stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .context("resolve current change id from `@`")
}
