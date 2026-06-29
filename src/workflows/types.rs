use super::super::*;
use super::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SubmitSummary {
    pub(crate) pushed_refs: usize,
    pub(crate) created_prs: usize,
    pub(crate) updated_prs: usize,
    pub(crate) unchanged_prs: usize,
    pub(crate) created_comments: usize,
    pub(crate) updated_comments: usize,
    pub(crate) unchanged_comments: usize,
    pub(crate) duplicate_comment_warnings: usize,
    pub(crate) closed_orphans: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct SubmitPlan {
    pub(crate) change: ResolvedChange,
    pub(crate) head_branch: String,
    pub(crate) base_branch: String,
    pub(crate) existing_pr: Option<PrCacheEntry>,
    pub(crate) expected_remote_head: Option<String>,
    pub(crate) push_needed: bool,
    pub(crate) pr_update_needed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitPrAction {
    Submit,
    Update,
    Nothing,
}

impl SubmitPrAction {
    pub(crate) fn progress_verb(self) -> &'static str {
        match self {
            Self::Submit => "Submitted",
            Self::Update => "Updated",
            Self::Nothing => "Nothing",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SyncSummary {
    pub(crate) rebased_roots: usize,
    pub(crate) submit_ran: bool,
    pub(crate) cleaned_branches: usize,
    pub(crate) pruned_duplicates: usize,
    pub(crate) conflicts: usize,
}

/// Aggregate of syncing every tracked stack in one `forklift sync` invocation.
/// `stacks` is the number of distinct tracked stacks that were synced; the
/// remaining counters sum the per-stack [`SyncSummary`] values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SyncAllSummary {
    pub(crate) stacks: usize,
    /// Tracked stacks whose sync errored and were skipped (best-effort).
    pub(crate) failed: usize,
    pub(crate) rebased_roots: usize,
    pub(crate) submit_ran: bool,
    pub(crate) cleaned_branches: usize,
    pub(crate) pruned_duplicates: usize,
    pub(crate) conflicts: usize,
}

impl SyncAllSummary {
    pub(crate) fn add(&mut self, summary: &SyncSummary) {
        self.rebased_roots += summary.rebased_roots;
        self.submit_ran |= summary.submit_ran;
        self.cleaned_branches += summary.cleaned_branches;
        self.pruned_duplicates += summary.pruned_duplicates;
        self.conflicts += summary.conflicts;
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MergeSummary {
    pub(crate) checked_prs: usize,
    pub(crate) merged_prs: usize,
    pub(crate) local_updates: usize,
    pub(crate) submit_runs: usize,
    pub(crate) cleaned_branches: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GetSummary {
    pub(crate) prs: usize,
    pub(crate) fetched_branches: usize,
    pub(crate) cache_entries: usize,
    pub(crate) edited: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RepairSummary {
    pub(crate) open_prs: usize,
    pub(crate) pruned_merged_prs: usize,
    pub(crate) comments_changed: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct RepairPlan {
    pub(crate) open_prs: Vec<GhPr>,
    pub(crate) pruned_merged_prs: Vec<u64>,
}

#[derive(Debug, Clone)]
pub(crate) enum RepairAction {
    UpsertStackComment {
        pr_number: u64,
        removed_prs: Vec<u64>,
        body: String,
    },
}

impl RepairAction {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::UpsertStackComment {
                pr_number,
                removed_prs,
                ..
            } => {
                let removed = repair_pr_list(removed_prs);
                format!("update stack comment on PR #{pr_number} to remove {removed}")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusReport {
    pub(crate) repo: String,
    pub(crate) username: String,
    pub(crate) remote: String,
    pub(crate) trunk: String,
    pub(crate) branch_prefix: String,
    pub(crate) require_approval: bool,
    pub(crate) startup_aliases: StatusAliasState,
    pub(crate) stack_log_revset: String,
    pub(crate) stack_entries: Vec<StatusStackEntry>,
    pub(crate) owned_prs: Vec<StatusOwnedPr>,
    pub(crate) frozen_dependencies: Vec<StatusFrozenDependency>,
    pub(crate) first_owned_base_branch: Option<String>,
    pub(crate) merge_blockers: Vec<String>,
    pub(crate) bookmark_problems: Vec<String>,
    pub(crate) problems: Vec<String>,
    pub(crate) suggested_next_command: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusAliasState {
    pub(crate) frozen_heads: Option<String>,
    pub(crate) immutable_heads: Option<String>,
    pub(crate) base_immutable_heads: Option<String>,
    pub(crate) actions_needed: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusStackEntry {
    pub(crate) change_id: String,
    pub(crate) commit_id: String,
    pub(crate) title: String,
    pub(crate) pr_number: Option<u64>,
    pub(crate) head_branch: Option<String>,
    pub(crate) current_stack: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusOwnedPr {
    pub(crate) change_id: String,
    pub(crate) commit_id: String,
    pub(crate) title: String,
    pub(crate) head_branch: String,
    pub(crate) base_branch: String,
    pub(crate) pr_number: Option<u64>,
    pub(crate) action: String,
    pub(crate) bookmark_problem: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusFrozenDependency {
    pub(crate) bookmark: String,
    pub(crate) pr_number: u64,
    pub(crate) change_id: String,
    pub(crate) commit_id: String,
    pub(crate) title: String,
    pub(crate) head_branch: Option<String>,
    pub(crate) state: String,
    pub(crate) problem: Option<String>,
}
