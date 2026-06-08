// End-to-end tests driving the real `forklift` binary against a REAL colocated
// `jj` repo and a REAL bare `git` remote. Per AGENTS.md, `jj` and `git` are
// never mocked; the only faked process is `gh` (see tests/common/mod.rs).
//
// These tests assert on observable state — remote refs, local bookmarks,
// revision mutability, the SQLite cache, and the fake-`gh` PR store — rather
// than on the argv a command happened to invoke.

mod common;

use common::{TestRepo, assert_success, stderr_of, stdout_of};
use serde_json::json;

fn branch_for(slug: &str, change_id: &str) -> String {
    format!("stack/{slug}-{}", &change_id[..8])
}

#[test]
fn one_change_submit_creates_pr() -> anyhow::Result<()> {
    let repo = TestRepo::new("one-submit")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;

    let output = repo.run(&["submit", "--yes"])?;
    assert_success("submit", &output);
    assert!(
        stderr_of(&output)
            .contains("Submitted PR #9 https://github.com/owner/repo/pull/9 - change title"),
        "stderr:\n{}",
        stderr_of(&output)
    );

    // The branch was pushed through jj to the real remote at the change commit.
    assert_eq!(repo.git_remote_branch_target(&branch)?, change.commit_id);
    // A PR was created with the branch as head and trunk as base.
    let pr = repo.stored_pr(9)?;
    assert_eq!(pr["headRefName"], json!(branch));
    assert_eq!(pr["baseRefName"], json!("main"));
    assert!(repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    assert!(repo.gh_request_has_field(&format!("head={branch}"))?);
    // The repo-private cache was written.
    assert!(
        repo.cache_path().exists(),
        "submit should save SQLite cache"
    );
    Ok(())
}

#[test]
fn submit_requires_confirmation_before_mutation() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-confirm")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);

    let output = repo.run(&["submit"])?;
    assert!(
        !output.status.success(),
        "non-interactive submit should require confirmation"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&format!(
            "actions:\n  1. create new PR `change title`: push origin/{branch} @ {}, base main",
            &change.commit_id[..8]
        )) && stderr.contains("2. sync stack comments for submitted stack"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("------------------------------------------------------------"),
        "submit confirmation should not print a divider before the prompt\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("error: submit requires confirmation"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("resolution:\n  rerun with `forklift submit --yes`"),
        "stderr:\n{stderr}"
    );
    assert!(
        !repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?,
        "submit must not create a PR before confirmation"
    );
    assert!(
        !repo.cache_path().exists(),
        "submit must not write cache before confirmation"
    );
    Ok(())
}

#[test]
fn submit_dry_run_skips_cache_writes() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-dry-run")?;
    repo.init_main()?;
    repo.create_change("change", "change title", "change body")?;

    let output = repo.run(&["submit", "--dry-run"])?;
    assert_success("submit --dry-run", &output);

    assert!(
        stdout_of(&output).contains("SQLite cache writes are skipped"),
        "stdout:\n{}",
        stdout_of(&output)
    );
    assert!(
        !repo.cache_path().exists(),
        "dry-run submit must not create the cache"
    );
    // Discovery still ran, but no PR was created.
    assert!(repo.gh_request_matches(&["pr", "list"])?);
    assert!(!repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    Ok(())
}

#[test]
fn pr_error_is_rendered_as_a_human_diagnostic() -> anyhow::Result<()> {
    let repo = TestRepo::new("pr-error")?;
    repo.init_main()?;
    repo.create_change("change", "change title", "change body")?;

    let output = repo.run(&["pr"])?;
    assert!(!output.status.success(), "pr without a PR should fail");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("error: current revision is not submitted"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("reason:\n  current change `"),
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("has no open PR yet"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("resolution:\n  run `forklift submit --yes`, then `forklift pr`"),
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("details:"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("phase:     resolve-pr"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("phase=resolve-pr object=@"),
        "stderr:\n{stderr}"
    );
    Ok(())
}

#[test]
fn pr_on_trunk_explains_there_is_no_current_pr() -> anyhow::Result<()> {
    let repo = TestRepo::new("pr-on-trunk")?;
    repo.init_main()?;

    let output = repo.run(&["pr"])?;
    assert!(!output.status.success(), "pr on trunk should fail");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("error: no current PR"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("reason:\n  current checkout is on trunk `main`"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("resolution:\n  check out a stack change or pass a PR target"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("forklift submit --dry-run"),
        "trunk diagnostic should not point to submit\nstderr:\n{stderr}"
    );
    Ok(())
}

#[test]
fn two_change_submit_uses_parent_head_branch_base() -> anyhow::Result<()> {
    let repo = TestRepo::new("two-submit")?;
    repo.init_main()?;
    let stack = repo.create_linear_stack(2)?;
    let bottom = branch_for("change-1-title", &stack[0].change_id);
    let top = branch_for("change-2-title", &stack[1].change_id);
    repo.seed_pr_number(&bottom, 11)?;
    repo.seed_pr_number(&top, 12)?;

    let output = repo.run(&["submit", "--yes"])?;
    assert_success("submit", &output);

    let top_pr = repo.stored_pr(12)?;
    assert_eq!(
        top_pr["baseRefName"],
        json!(bottom),
        "top PR should target the bottom PR branch"
    );
    let bottom_pr = repo.stored_pr(11)?;
    assert_eq!(bottom_pr["baseRefName"], json!("main"));
    Ok(())
}

#[test]
fn two_change_update_keeps_top_pr_based_on_bottom_branch() -> anyhow::Result<()> {
    let repo = TestRepo::new("two-update")?;
    repo.init_main()?;
    let stack = repo.create_linear_stack(2)?;
    let bottom = branch_for("change-1-title", &stack[0].change_id);
    let top = branch_for("change-2-title", &stack[1].change_id);
    repo.seed_pr_number(&bottom, 11)?;
    repo.seed_pr_number(&top, 12)?;
    assert_success("initial submit", &repo.run(&["submit", "--yes"])?);

    // Edit the bottom change's title; this rewrites it and rebases the top.
    repo.jj(&[
        "describe",
        "-r",
        &stack[0].change_id,
        "-m",
        "change 1 title edited",
        "-m",
        "edited body",
    ])?;
    let bottom_after = repo.change_at(&stack[0].change_id)?;
    let top_after = repo.change_at(&stack[1].change_id)?;

    let output = repo.run(&["submit", "--yes"])?;
    assert_success("update submit", &output);
    assert!(
        stderr_of(&output).contains(
            "Updated PR #11 https://github.com/owner/repo/pull/11 - change 1 title edited"
        ),
        "stderr:\n{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output)
            .contains("Updated PR #12 https://github.com/owner/repo/pull/12 - change 2 title"),
        "stderr:\n{}",
        stderr_of(&output)
    );

    let bottom_pr = repo.stored_pr(11)?;
    assert_eq!(bottom_pr["baseRefName"], json!("main"));
    assert_eq!(bottom_pr["title"], json!("change 1 title edited"));
    let top_pr = repo.stored_pr(12)?;
    assert_eq!(top_pr["baseRefName"], json!(bottom));

    // Both branches were re-pushed to the rewritten commits.
    assert_eq!(
        repo.git_remote_branch_target(&bottom)?,
        bottom_after.commit_id
    );
    assert_eq!(repo.git_remote_branch_target(&top)?, top_after.commit_id);
    Ok(())
}

#[test]
fn noop_submit_skips_push_and_pr_mutation() -> anyhow::Result<()> {
    let repo = TestRepo::new("noop-submit")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("initial submit", &repo.run(&["submit", "--yes"])?);
    let pushed = repo.git_remote_branch_target(&branch)?;

    repo.clear_gh_requests()?;
    let output = repo.run(&["submit", "--yes"])?;
    assert_success("noop submit", &output);
    assert!(
        stderr_of(&output)
            .contains("Nothing PR #9 https://github.com/owner/repo/pull/9 - change title"),
        "stderr:\n{}",
        stderr_of(&output)
    );

    // No PR create or update on the second, no-op run.
    assert!(!repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    assert!(!repo.gh_request_matches(&["api", "-X", "PATCH", "repos/owner/repo/pulls/9"])?);
    // Remote head is unchanged.
    assert_eq!(repo.git_remote_branch_target(&branch)?, pushed);
    Ok(())
}

#[test]
fn submit_updates_existing_pr_from_tracked_bookmark_without_cache() -> anyhow::Result<()> {
    let repo = TestRepo::new("update-no-cache")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "original body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("initial submit", &repo.run(&["submit", "--yes"])?);

    // Drop the cache; submit must rediscover the PR from the tracked bookmark.
    repo.delete_cache()?;
    repo.write_file("change.txt", "change\nchange title\nedited\n")?;
    repo.jj(&["describe", "-m", "change title", "-m", "edited body"])?;
    let edited = repo.change_at("@")?;

    repo.clear_gh_requests()?;
    assert_success("update submit", &repo.run(&["submit", "--yes"])?);

    let prs = repo.stored_prs()?;
    assert_eq!(
        prs.len(),
        1,
        "submit should update, not duplicate: {prs:#?}"
    );
    assert_eq!(repo.git_remote_branch_target(&branch)?, edited.commit_id);
    assert_eq!(repo.stored_pr(9)?["body"], json!("edited body"));
    assert!(
        repo.cache_path().exists(),
        "submit should rebuild the cache"
    );
    assert!(repo.gh_request_matches(&["api", "-X", "PATCH", "repos/owner/repo/pulls/9"])?);
    assert!(!repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    Ok(())
}

#[test]
fn submit_refuses_open_branch_pr_without_local_bookmark() -> anyhow::Result<()> {
    let repo = TestRepo::new("branch-without-bookmark")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    // A PR exists for the branch, but there is no local tracked bookmark/cache.
    repo.seed_pr(9, &branch, "main", "change title", "old body")?;

    let output = repo.run(&["submit", "--yes"])?;
    assert!(
        !output.status.success(),
        "submit without a local bookmark should fail"
    );
    assert!(
        stderr_of(&output).contains(&format!(
            "local head bookmark `{branch}` is missing or conflicted"
        )),
        "stderr:\n{}",
        stderr_of(&output)
    );
    assert!(!repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    assert!(!repo.cache_path().exists());
    Ok(())
}

#[test]
fn submit_refuses_undescribed_commit_before_pushing() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-undescribed")?;
    repo.init_main()?;
    // A proper described change at the bottom of the stack...
    let described = repo.create_change("described", "described title", "body")?;
    let described_branch = branch_for("described-title", &described.change_id);
    // ...with a non-empty but UNdescribed change stacked on top of it.
    repo.jj(&["new"])?;
    repo.write_file("undescribed.txt", "no description here\n")?;

    let output = repo.run(&["submit", "--yes"])?;
    assert!(
        !output.status.success(),
        "submit must refuse a stack containing an undescribed commit"
    );
    assert!(
        stderr_of(&output).contains("no description"),
        "stderr should explain the missing description, got:\n{}",
        stderr_of(&output)
    );
    // It failed during pre-flight validation, before push-refs: the described
    // branch must NOT have been pushed, and no PR was opened.
    assert!(
        !repo.remote_branch_exists(&described_branch)?,
        "no branch should reach the remote when validation fails pre-flight"
    );
    assert!(
        !repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?,
        "no PR should be created when validation fails pre-flight"
    );
    Ok(())
}

#[test]
fn merge_dry_run_checks_without_mutating() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-dry-run")?;
    repo.init_main()?;
    let main = repo.bookmark_target("main")?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);

    repo.clear_gh_requests()?;
    let output = repo.run(&["merge", "--dry-run"])?;
    assert_success("merge --dry-run", &output);

    assert!(!repo.gh_request_matches(&["pr", "merge", "9"])?);
    assert_eq!(
        repo.git_remote_branch_target("main")?,
        main,
        "dry-run merge must not advance trunk"
    );
    assert_eq!(repo.stored_pr(9)?["state"], json!("OPEN"));
    Ok(())
}

#[test]
fn merge_dry_run_discovers_pr_without_cache() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-no-cache")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);

    repo.delete_cache()?;
    repo.clear_gh_requests()?;
    let output = repo.run(&["merge", "--dry-run"])?;
    assert_success("merge --dry-run", &output);

    assert!(repo.gh_request_matches(&["pr", "view", "9"])?);
    assert!(!repo.gh_request_matches(&["pr", "merge", "9"])?);
    assert_eq!(repo.stored_pr(9)?["state"], json!("OPEN"));
    Ok(())
}

#[test]
fn merge_dry_run_refuses_stale_remote_trunk() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-dry-run-stale-trunk")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);
    let submitted = repo.change_at(&change.change_id)?;
    let advanced = repo.advance_remote_trunk("remote work", &change.change_id)?;

    let output = repo.run(&["merge", "9", "--dry-run", "--admin"])?;
    assert!(
        !output.status.success(),
        "dry-run merge must reject stale trunk instead of printing a fast-forward plan"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("error: failed during merge-push"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "trunk `main` cannot fast-forward to {}: remote {} is not an ancestor; run `forklift merge 9 --sync` first",
            submitted.commit_id, advanced.commit_id
        )),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("fast-forward trunk `main`"),
        "stale dry-run must not print a fast-forward plan\nstderr:\n{stderr}"
    );
    assert_eq!(repo.git_remote_branch_target("main")?, advanced.commit_id);
    Ok(())
}

#[test]
fn merge_prompts_to_sync_submit_when_trunk_is_stale() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-prompt-sync-submit")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);
    let submitted = repo.change_at(&change.change_id)?;
    let advanced = repo.advance_remote_trunk("remote work", &change.change_id)?;

    let output = repo.run(&["merge", "9", "--admin"])?;
    assert!(
        !output.status.success(),
        "non-interactive merge should explain how to sync and submit before merging"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("error: failed during merge-push"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "trunk `main` cannot fast-forward to {}: remote {} is not an ancestor; run `forklift merge 9 --sync` first",
            submitted.commit_id, advanced.commit_id
        )),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("resolution:\n  run `forklift merge 9 --sync`"),
        "stderr:\n{stderr}"
    );
    assert_eq!(repo.git_remote_branch_target("main")?, advanced.commit_id);
    assert_eq!(repo.stored_pr(9)?["state"], json!("OPEN"));
    Ok(())
}

#[test]
fn merge_sync_rebases_submits_then_merges_target() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-sync-target")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);
    let submitted = repo.change_at(&change.change_id)?;
    let advanced = repo.advance_remote_trunk("remote work", &change.change_id)?;

    let output = repo.run(&["merge", "9", "--sync", "--admin"])?;
    assert_success("merge 9 --sync --admin", &output);

    let remote_main = repo.git_remote_branch_target("main")?;
    assert_ne!(
        remote_main, submitted.commit_id,
        "merge --sync should not merge the stale pre-sync PR head"
    );
    assert_ne!(
        remote_main, advanced.commit_id,
        "merge --sync should fast-forward trunk beyond the fetched remote tip"
    );
    assert_eq!(repo.stored_pr(9)?["state"], json!("MERGED"));
    Ok(())
}

#[test]
fn clean_two_pr_merge_fast_forwards_trunk_and_merges_by_reachability() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-two-clean")?;
    repo.init_main()?;
    let stack = repo.create_linear_stack(2)?;
    let bottom = branch_for("change-1-title", &stack[0].change_id);
    let top = branch_for("change-2-title", &stack[1].change_id);
    repo.seed_pr_number(&bottom, 11)?;
    repo.seed_pr_number(&top, 12)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);
    let top_commit = repo.change_at(&stack[1].change_id)?.commit_id;

    repo.clear_gh_requests()?;
    let output = repo.run(&["merge"])?;
    assert_success("merge", &output);

    // The top PR was retargeted onto trunk; the old squash path is gone.
    assert_eq!(repo.stored_pr(12)?["baseRefName"], json!("main"));
    assert!(
        !repo.gh_request_matches(&["pr", "merge", "11"])?
            && !repo.gh_request_matches(&["pr", "merge", "12"])?,
        "merge must not squash via gh pr merge"
    );
    // Trunk was fast-forwarded over the whole stack on the real remote.
    assert_eq!(repo.git_remote_branch_target("main")?, top_commit);
    // Both PRs are merged by reachability.
    for number in [11, 12] {
        assert_eq!(
            repo.stored_pr(number)?["state"],
            json!("MERGED"),
            "PR #{number} should be merged"
        );
    }
    Ok(())
}

#[test]
fn merge_refuses_open_frozen_dependency_below_owned_pr() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-open-frozen-dependency")?;
    repo.init_main()?;
    let main = repo.bookmark_target("main")?;
    let stack = repo.create_linear_stack(2)?;
    let bottom = branch_for("change-1-title", &stack[0].change_id);
    let top = branch_for("change-2-title", &stack[1].change_id);
    repo.set_bookmark(&bottom, &stack[0].commit_id)?;
    repo.set_bookmark(&top, &stack[1].commit_id)?;
    repo.push_bookmark(&bottom)?;
    repo.push_bookmark(&top)?;
    repo.seed_pr(11, &bottom, "main", "change 1 title", "change 1 body")?;
    repo.seed_pr(12, &top, &bottom, "change 2 title", "change 2 body")?;
    repo.set_bookmark("forklift/frozen/pr-11", &stack[0].commit_id)?;

    let output = repo.run(&["merge"])?;
    assert!(
        !output.status.success(),
        "merge should fail while lower dependency PR is open\nstdout:\n{}\nstderr:\n{}\nprs:\n{:#?}",
        stdout_of(&output),
        stderr_of(&output),
        repo.stored_prs()?
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("frozen dependency")
            && stderr.contains("PR #11")
            && stderr.contains("still open"),
        "stderr:\n{stderr}"
    );
    assert_eq!(
        repo.git_remote_branch_target("main")?,
        main,
        "merge must not advance trunk"
    );
    assert_eq!(repo.stored_pr(11)?["state"], json!("OPEN"));
    assert_eq!(repo.stored_pr(12)?["state"], json!("OPEN"));
    Ok(())
}

#[test]
fn targeted_merge_errors_when_target_is_frozen() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-target-frozen")?;
    repo.init_main()?;
    let main = repo.bookmark_target("main")?;
    let imported = repo.create_change("imported", "imported title", "imported body")?;
    let branch = branch_for("imported-title", &imported.change_id);
    repo.set_bookmark(&branch, &imported.commit_id)?;
    repo.push_bookmark(&branch)?;
    repo.seed_pr(1, &branch, "main", "imported title", "imported body")?;
    repo.set_bookmark("forklift/frozen/pr-1", &imported.commit_id)?;
    repo.jj(&["new", &imported.commit_id])?;

    let output = repo.run(&["merge", "1"])?;
    assert!(
        !output.status.success(),
        "targeted merge of frozen PR should fail"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("error: PR #1 is frozen"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("covered by `forklift/frozen/pr-1`"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("resolution:\n  unfreeze or get ownership of PR #1"),
        "stderr:\n{stderr}"
    );
    assert_eq!(
        repo.git_remote_branch_target("main")?,
        main,
        "merge must not advance trunk"
    );
    assert_eq!(repo.stored_pr(1)?["state"], json!("OPEN"));
    Ok(())
}

#[test]
fn targeted_merge_errors_when_target_is_already_in_trunk() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-target-in-trunk")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.set_bookmark(&branch, &change.commit_id)?;
    repo.push_bookmark(&branch)?;
    repo.seed_pr(1, &branch, "main", "change title", "change body")?;
    repo.jj(&["bookmark", "set", "main", "-r", &change.commit_id])?;
    repo.push_bookmark("main")?;

    let output = repo.run(&["merge", "1"])?;
    assert!(
        !output.status.success(),
        "targeted merge of trunk-reachable PR should fail"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("error: PR #1 is already on trunk"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("reason:\n  ") && stderr.contains(" is in `main`"),
        "stderr:\n{stderr}"
    );
    Ok(())
}

#[test]
fn merge_rewritten_local_change_points_to_submit() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-rewritten-local")?;
    repo.init_main()?;
    let stack = repo.create_linear_stack(1)?;
    let branch = branch_for("change-1-title", &stack[0].change_id);
    repo.seed_pr_number(&branch, 31)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);

    // Rewrite the local change so its commit moves past what was pushed — the
    // PR head and the cache still agree on the old commit. This is the user's
    // "ran sync, didn't submit" case.
    repo.write_file("change-1.txt", "rewritten contents\n")?;

    let output = repo.run(&["merge"])?;
    assert!(
        !output.status.success(),
        "merge of a rewritten-but-unpushed change must fail"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("your stack was rewritten") && stderr.contains("forklift submit"),
        "expected a submit-pointing message, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("error: failed during merge-pr-check"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("resolution:\n  run `forklift submit --yes`, then `forklift merge`"),
        "expected submit confirmation guidance, stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("resolution:\n  run `forklift submit --dry-run`"),
        "merge readiness should not fall back to the generic submit dry-run hint, stderr:\n{stderr}"
    );
    // It must NOT have merged the PR.
    assert_ne!(repo.stored_pr(31)?["state"], json!("MERGED"));
    Ok(())
}

#[test]
fn merge_auto_tracks_untracked_trunk_before_fast_forward() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-untracked-trunk")?;
    repo.init_main()?;
    let stack = repo.create_linear_stack(1)?;
    let branch = branch_for("change-1-title", &stack[0].change_id);
    repo.seed_pr_number(&branch, 21)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);
    let top_commit = repo.change_at(&stack[0].change_id)?.commit_id;

    // Reproduce the user's broken state: a non-tracking `main@origin`. Without
    // the auto-track fix the fast-forward push aborts with "Non-tracking remote
    // bookmark main@origin exists".
    repo.jj(&["bookmark", "untrack", "main@origin"])?;

    let output = repo.run(&["merge"])?;
    assert_success("merge", &output);

    // It warned that it repaired the tracking...
    assert!(
        stderr_of(&output).contains("was untracked"),
        "expected an auto-track warning, stderr:\n{}",
        stderr_of(&output)
    );
    // ...and the fast-forward push landed on the real remote.
    assert_eq!(repo.git_remote_branch_target("main")?, top_commit);
    assert_eq!(repo.stored_pr(21)?["state"], json!("MERGED"));
    Ok(())
}

#[test]
fn sync_rebases_then_submits() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-rebase-submit")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let advanced = repo.advance_remote_trunk("remote work", &change.change_id)?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;

    let output = repo.run(&["sync", "--submit", "--yes"])?;
    assert_success("sync --submit", &output);

    // The change was rebased onto the advanced remote trunk.
    let rebased = repo.change_at(&change.change_id)?;
    let parent = repo.rev_commit_id(&format!("{}-", rebased.commit_id))?;
    assert_eq!(parent, advanced.commit_id, "change should sit on new trunk");
    // And submitted.
    assert!(repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    Ok(())
}

#[test]
fn sync_reports_rebase_conflicts() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-rebase-conflict")?;
    let main = repo.init_main()?;

    repo.jj(&["new"])?;
    repo.write_file("file.txt", "local\n")?;
    repo.jj(&["describe", "-m", "local edit"])?;
    let local = repo.change_at("@")?;

    repo.jj(&["new", "main"])?;
    repo.write_file("file.txt", "remote\n")?;
    repo.jj(&["describe", "-m", "remote edit"])?;
    let remote = repo.change_at("@")?;
    repo.jj(&["bookmark", "set", "main", "-r", "@"])?;
    repo.push_bookmark("main")?;
    repo.jj(&[
        "bookmark",
        "set",
        "--allow-backwards",
        "main",
        "-r",
        &main.commit_id,
    ])?;
    repo.jj(&["edit", &local.change_id])?;

    let output = repo.run(&["sync"])?;
    assert_success("sync", &output);
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&format!(
            "Conflict {} has unresolved merge conflicts",
            local.change_id
        )),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Finished sync — 1 roots rebased, 1 conflict(s), submit skipped"),
        "stderr:\n{stderr}"
    );
    assert_eq!(repo.git_remote_branch_target("main")?, remote.commit_id);
    Ok(())
}

#[test]
fn targeted_sync_rebases_target_stack_without_current_checkout() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-target-side-stack")?;
    let main = repo.init_main()?;
    let target = repo.create_change("target", "target title", "target body")?;
    let target_branch = branch_for("target-title", &target.change_id);
    repo.set_bookmark(&target_branch, &target.commit_id)?;
    repo.push_bookmark(&target_branch)?;
    repo.seed_pr(1, &target_branch, "main", "target title", "target body")?;

    repo.jj(&["new", "main"])?;
    let unrelated = repo.create_change("unrelated", "unrelated title", "unrelated body")?;
    let unrelated_before = unrelated.commit_id.clone();
    let advanced = repo.advance_remote_trunk("remote work", &unrelated.change_id)?;

    let output = repo.run(&["sync", "1"])?;
    assert_success("sync 1", &output);

    let target_after = repo.change_at(&target.change_id)?;
    let target_parent = repo.rev_commit_id(&format!("{}-", target_after.commit_id))?;
    assert_eq!(
        target_parent, advanced.commit_id,
        "targeted sync should rebase the target stack onto fetched trunk"
    );
    assert_ne!(
        target_after.commit_id, target.commit_id,
        "target commit should be rewritten by sync"
    );
    assert_eq!(
        repo.change_at(&unrelated.change_id)?.commit_id,
        unrelated_before,
        "targeted sync must not rebase the unrelated current checkout stack"
    );
    assert_ne!(
        repo.rev_commit_id(&format!("{}-", unrelated_before))?,
        advanced.commit_id,
        "unrelated stack should remain based on the original trunk"
    );
    assert_eq!(
        repo.git_remote_branch_target("main")?,
        advanced.commit_id,
        "sync should still move trunk to the fetched remote tip"
    );
    assert_eq!(
        main.commit_id,
        repo.rev_commit_id(&format!("{}-", unrelated_before))?
    );
    Ok(())
}

#[test]
fn targeted_sync_submit_updates_only_target_stack() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-target-submit")?;
    repo.init_main()?;
    let target = repo.create_change("target", "target title", "target body")?;
    let target_branch = branch_for("target-title", &target.change_id);
    repo.set_bookmark(&target_branch, &target.commit_id)?;
    repo.push_bookmark(&target_branch)?;
    repo.seed_pr(1, &target_branch, "main", "target title", "target body")?;

    repo.jj(&["new", "main"])?;
    let unrelated = repo.create_change("unrelated", "unrelated title", "unrelated body")?;
    let unrelated_branch = branch_for("unrelated-title", &unrelated.change_id);
    repo.set_bookmark(&unrelated_branch, &unrelated.commit_id)?;
    repo.push_bookmark(&unrelated_branch)?;
    repo.seed_pr(
        2,
        &unrelated_branch,
        "main",
        "unrelated title",
        "unrelated body",
    )?;
    repo.advance_remote_trunk("remote work", &unrelated.change_id)?;
    repo.clear_gh_requests()?;

    let output = repo.run(&[
        "sync",
        "https://github.com/owner/repo/pull/1",
        "--submit",
        "--yes",
    ])?;
    assert_success("sync 1 --submit", &output);

    let target_after = repo.change_at(&target.change_id)?;
    assert_eq!(
        repo.git_remote_branch_target(&target_branch)?,
        target_after.commit_id,
        "targeted sync --submit should push the rebased target branch"
    );
    assert_eq!(
        repo.git_remote_branch_target(&unrelated_branch)?,
        unrelated.commit_id,
        "targeted sync --submit must not push the unrelated branch"
    );
    assert!(repo.gh_request_matches(&["api", "-X", "PATCH", "repos/owner/repo/pulls/1"])?);
    assert!(!repo.gh_request_matches(&["api", "-X", "PATCH", "repos/owner/repo/pulls/2"])?);
    Ok(())
}

#[test]
fn sync_divergence_stops_before_rebase() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-divergence")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let divergent = repo.diverge_remote_trunk("divergent trunk", &change.change_id)?;
    let before = repo.change_at(&change.change_id)?.commit_id;

    let output = repo.run(&["sync"])?;
    assert!(!output.status.success(), "divergent sync should fail");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&divergent.local),
        "stderr should cite local trunk:\n{stderr}"
    );
    assert!(
        stderr.contains(&divergent.remote),
        "stderr should cite divergent remote trunk:\n{stderr}"
    );
    // The change was not rebased.
    assert_eq!(repo.change_at(&change.change_id)?.commit_id, before);
    Ok(())
}

#[test]
fn get_imports_single_pr_without_stack_comment() -> anyhow::Result<()> {
    let repo = TestRepo::new("get-single")?;
    repo.init_main()?;
    let imported = repo.create_change("imported", "imported title", "imported body")?;
    let branch = branch_for("imported-title", &imported.change_id);
    repo.set_bookmark(&branch, "@")?;
    repo.push_bookmark(&branch)?;
    repo.seed_pr(11, &branch, "main", "imported title", "imported body")?;

    let output = repo.run(&["get", "11"])?;
    assert_success("get 11", &output);

    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-11")?,
        imported.commit_id,
        "get should freeze the PR head"
    );
    assert!(
        !repo.bookmark_exists("forklift/frozen/pr-12")?,
        "single-PR import should not infer descendants"
    );
    assert_eq!(
        repo.rev_commit_id("@-")?,
        imported.commit_id,
        "get should leave @ on a new editable change above the imported PR"
    );
    assert_eq!(
        repo.cache_entry(&imported.change_id)?["pr_number"],
        json!(11)
    );
    Ok(())
}

#[test]
fn get_fetches_stack_from_comment_and_writes_cache() -> anyhow::Result<()> {
    let repo = TestRepo::new("get-stack")?;
    repo.init_main()?;
    let stack = repo.create_linear_stack(2)?;
    let bottom = branch_for("change-1-title", &stack[0].change_id);
    let top = branch_for("change-2-title", &stack[1].change_id);
    repo.set_bookmark(&bottom, &stack[0].commit_id)?;
    repo.set_bookmark(&top, &stack[1].commit_id)?;
    repo.push_bookmark(&bottom)?;
    repo.push_bookmark(&top)?;
    repo.seed_pr(11, &bottom, "main", "change 1 title", "change 1 body")?;
    repo.seed_pr(12, &top, &bottom, "change 2 title", "change 2 body")?;
    let rows = [
        (
            stack[0].change_id.as_str(),
            11u64,
            bottom.as_str(),
            "main",
            "change 1 title",
        ),
        (
            stack[1].change_id.as_str(),
            12u64,
            top.as_str(),
            bottom.as_str(),
            "change 2 title",
        ),
    ];
    repo.seed_comment(
        12,
        201,
        &common::stack_comment_body(&rows, &stack[1].change_id),
    )?;

    let output = repo.run(&["get", "12", "--no-edit"])?;
    assert_success("get 12", &output);

    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-11")?,
        stack[0].commit_id
    );
    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-12")?,
        stack[1].commit_id
    );
    assert!(
        stdout_of(&output).contains(
            "skip editing: run `jj new forklift/frozen/pr-12` to start editing above the fetched stack"
        ),
        "stdout:\n{}",
        stdout_of(&output)
    );
    assert_eq!(
        repo.cache_entry(&stack[0].change_id)?["pr_number"],
        json!(11)
    );
    assert_eq!(
        repo.cache_entry(&stack[1].change_id)?["pr_number"],
        json!(12)
    );
    Ok(())
}

#[test]
fn unfreeze_tracks_descendant_untracked_remote_blockers() -> anyhow::Result<()> {
    let repo = TestRepo::new("unfreeze-remote-blocker")?;
    repo.init_main()?;
    let stack = repo.create_linear_stack(2)?;
    let bottom_branch = branch_for("change-1-title", &stack[0].change_id);
    let top_branch = branch_for("change-2-title", &stack[1].change_id);
    repo.set_bookmark(&bottom_branch, &stack[0].commit_id)?;
    repo.set_bookmark(&top_branch, &stack[1].commit_id)?;
    repo.push_bookmark(&bottom_branch)?;
    repo.push_bookmark(&top_branch)?;
    repo.seed_pr(
        11,
        &bottom_branch,
        "main",
        "change 1 title",
        "change 1 body",
    )?;
    repo.set_bookmark("forklift/frozen/pr-11", &stack[0].commit_id)?;
    repo.jj(&["bookmark", "untrack", &format!("{top_branch}@origin")])?;

    assert!(
        !repo.is_mutable(&stack[0].commit_id)?,
        "bottom PR should start immutable because the frozen bookmark and untracked descendant remote cover it"
    );

    let output = repo.run(&["unfreeze", "11"])?;
    assert_success("unfreeze 11", &output);
    assert!(
        stderr_of(&output).contains("keeps the target immutable; tracking it before adoption"),
        "stderr:\n{}",
        stderr_of(&output)
    );
    assert!(
        !repo.bookmark_exists("forklift/frozen/pr-11")?,
        "frozen bookmark should be removed"
    );
    assert!(
        repo.is_mutable(&stack[0].commit_id)?,
        "bottom PR should become mutable after unfreeze tracks the remote blocker"
    );
    Ok(())
}

#[test]
fn unfreeze_recovers_when_previous_attempt_already_removed_frozen_bookmark() -> anyhow::Result<()> {
    let repo = TestRepo::new("unfreeze-missing-frozen")?;
    repo.init_main()?;
    let stack = repo.create_linear_stack(2)?;
    let bottom_branch = branch_for("change-1-title", &stack[0].change_id);
    let top_branch = branch_for("change-2-title", &stack[1].change_id);
    repo.set_bookmark(&bottom_branch, &stack[0].commit_id)?;
    repo.set_bookmark(&top_branch, &stack[1].commit_id)?;
    repo.push_bookmark(&bottom_branch)?;
    repo.push_bookmark(&top_branch)?;
    repo.seed_pr(
        11,
        &bottom_branch,
        "main",
        "change 1 title",
        "change 1 body",
    )?;
    repo.jj(&["bookmark", "untrack", &format!("{top_branch}@origin")])?;

    assert!(
        !repo.bookmark_exists("forklift/frozen/pr-11")?,
        "test starts in the partial old-unfreeze state"
    );
    assert!(
        !repo.is_mutable(&stack[0].commit_id)?,
        "bottom PR should still be immutable because the untracked descendant remote covers it"
    );

    let output = repo.run(&["unfreeze", "11"])?;
    assert_success("unfreeze 11", &output);
    assert!(
        stderr_of(&output).contains("frozen bookmark `forklift/frozen/pr-11` is missing"),
        "stderr:\n{}",
        stderr_of(&output)
    );
    assert!(
        repo.is_mutable(&stack[0].commit_id)?,
        "rerun should finish adoption even after the old command removed the frozen bookmark"
    );
    Ok(())
}

#[test]
fn repair_prunes_merged_pr_from_stale_stack_comment() -> anyhow::Result<()> {
    let repo = TestRepo::new("repair-stale-stack")?;
    repo.init_main()?;
    let open = repo.create_change("open", "open title", "open body")?;
    let open_branch = branch_for("open-title", &open.change_id);
    repo.set_bookmark(&open_branch, &open.commit_id)?;
    repo.push_bookmark(&open_branch)?;

    repo.jj(&["new", "main"])?;
    let merged = repo.create_change("merged", "merged title", "merged body")?;
    let merged_branch = branch_for("merged-title", &merged.change_id);
    repo.set_bookmark(&merged_branch, &merged.commit_id)?;
    repo.push_bookmark(&merged_branch)?;
    repo.jj(&["bookmark", "set", "main", "-r", &merged.commit_id])?;
    repo.push_bookmark("main")?;

    repo.seed_pr(1, &open_branch, "main", "open title", "open body")?;
    repo.seed_pr(5, &merged_branch, "main", "merged title", "merged body")?;
    repo.set_pr_state(5, "CLOSED")?;
    repo.set_pr_merged(5, true)?;
    let rows = [
        (
            open.change_id.as_str(),
            1u64,
            open_branch.as_str(),
            "main",
            "open title",
        ),
        (
            merged.change_id.as_str(),
            5u64,
            merged_branch.as_str(),
            "main",
            "merged title",
        ),
    ];
    repo.seed_comment(1, 201, &common::stack_comment_body(&rows, &open.change_id))?;

    let get_before = repo.run(&["get", "1"])?;
    assert!(
        !get_before.status.success(),
        "stale get should fail before repair"
    );
    assert!(
        stderr_of(&get_before).contains("PR #5 is CLOSED"),
        "stderr:\n{}",
        stderr_of(&get_before)
    );

    let repair_without_confirmation = repo.run(&["repair", "1"])?;
    assert!(
        !repair_without_confirmation.status.success(),
        "non-interactive repair should require confirmation"
    );
    let stderr = stderr_of(&repair_without_confirmation);
    assert!(
        !stderr.contains("plan: repair stack comment"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("problems:\n  merged PRs still listed in stack comment: #5"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("open PRs remaining in stack"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("actions:\n  1. update stack comment on PR #1 to remove #5")
            && stderr.contains("2. revalidate repaired stack comment topology"),
        "stderr:\n{stderr}"
    );
    assert!(!stderr.contains("bytes)"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("  2. revalidate repaired stack comment topology\n\nerror:"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("error: repair requires confirmation"),
        "stderr:\n{stderr}"
    );
    let body_before_confirm = repo
        .stored_comments(1)?
        .first()
        .and_then(|comment| comment["body"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(
        body_before_confirm.contains("/pull/5"),
        "body:\n{body_before_confirm}"
    );

    let repair = repo.run(&["repair", "1", "--yes"])?;
    assert_success("repair 1", &repair);
    assert!(
        stderr_of(&repair).contains("Finished repair"),
        "stderr:\n{}",
        stderr_of(&repair)
    );
    let comments = repo.stored_comments(1)?;
    let body = comments
        .first()
        .and_then(|comment| comment["body"].as_str())
        .unwrap_or_default();
    assert!(body.contains("/pull/1"), "body:\n{body}");
    assert!(!body.contains("/pull/5"), "body:\n{body}");

    let get_after = repo.run(&["get", "1"])?;
    assert_success("get 1 after repair", &get_after);
    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-1")?,
        open.commit_id
    );
    Ok(())
}

#[test]
fn repair_explains_closed_unmerged_pr_in_stack_comment() -> anyhow::Result<()> {
    let repo = TestRepo::new("repair-closed-stack")?;
    repo.init_main()?;
    let open = repo.create_change("open", "open title", "open body")?;
    let open_branch = branch_for("open-title", &open.change_id);
    repo.set_bookmark(&open_branch, &open.commit_id)?;
    repo.push_bookmark(&open_branch)?;

    let closed = repo.create_change("closed", "closed title", "closed body")?;
    let closed_branch = branch_for("closed-title", &closed.change_id);
    repo.set_bookmark(&closed_branch, &closed.commit_id)?;
    repo.push_bookmark(&closed_branch)?;

    repo.seed_pr(1, &open_branch, "main", "open title", "open body")?;
    repo.seed_pr(5, &closed_branch, "main", "closed title", "closed body")?;
    repo.set_pr_state(5, "CLOSED")?;
    let rows = [
        (
            open.change_id.as_str(),
            1u64,
            open_branch.as_str(),
            "main",
            "open title",
        ),
        (
            closed.change_id.as_str(),
            5u64,
            closed_branch.as_str(),
            "main",
            "closed title",
        ),
    ];
    repo.seed_comment(1, 201, &common::stack_comment_body(&rows, &open.change_id))?;

    let repair = repo.run(&["repair", "1"])?;
    assert!(
        !repair.status.success(),
        "repair should fail when a listed PR is closed but unmerged"
    );
    let stderr = stderr_of(&repair);
    assert!(
        stderr.contains("error: cannot repair stack comment automatically"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("reason:\n  PR #5 is CLOSED but not merged"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "resolution:\n  reopen or merge PR #5, or remove it from the stack comment manually, then run `forklift repair 1`"
        ),
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("state:     CLOSED"), "stderr:\n{stderr}");
    Ok(())
}

// Secondary jj workspaces are NOT git worktrees — they have a `.jj/repo`
// pointer to the primary's `.jj/repo` but no `.git`. `forklift` must therefore
// route every `git` invocation to the backing colocated workspace, regardless
// of which jj workspace the user ran it from. These tests pin that down with
// real `jj` and real `git` (no mocks).

#[test]
fn sync_from_secondary_workspace_succeeds() -> anyhow::Result<()> {
    let repo = TestRepo::new("ws-sync")?;
    repo.init_main()?;

    let secondary = repo.root.join("secondary");
    repo.jj(&["workspace", "add", secondary.to_str().unwrap()])?;
    // The secondary has no `.git`; before the workspace-routing fix this
    // failed with "resolve commit id for `main`" because git ran in the
    // workspace cwd and found no repo.
    let output = repo.run_in(&secondary, &["sync"])?;
    assert_success("sync from secondary workspace", &output);
    Ok(())
}

#[test]
fn status_from_secondary_workspace_succeeds() -> anyhow::Result<()> {
    let repo = TestRepo::new("ws-status")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;

    let secondary = repo.root.join("secondary");
    repo.jj(&["workspace", "add", secondary.to_str().unwrap()])?;
    // Reclaim the stack tip in the secondary so `trunk()..@` is non-empty.
    repo.jj(&["new", "main"])?;
    let edit_output = std::process::Command::new("jj")
        .args(["edit", &change.change_id])
        .current_dir(&secondary)
        .output()?;
    assert_success("jj edit on secondary", &edit_output);

    // `status` calls `gh repo view` to identify the GitHub repository.
    // Without the workspace-routing fix, `gh` ran with the secondary's cwd
    // (no `.git`, no remote) and bailed with
    // "resolve GitHub repository with gh".
    let output = repo.run_in(&secondary, &["status"])?;
    assert_success("status from secondary workspace", &output);
    Ok(())
}

#[test]
fn submit_from_secondary_workspace_pushes_branch_and_writes_cache_to_backing_repo()
-> anyhow::Result<()> {
    let repo = TestRepo::new("ws-submit")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 11)?;

    // Add a secondary workspace and point its @ at the stack's only change so
    // `main..@ & ~empty()` on the secondary resolves to that change.
    let secondary = repo.root.join("secondary");
    repo.jj(&["workspace", "add", secondary.to_str().unwrap()])?;
    // Primary released the change when we added the secondary; reclaim it
    // there so the secondary can `jj edit` onto it.
    repo.jj(&["new", "main"])?;
    let edit_output = std::process::Command::new("jj")
        .args(["edit", &change.change_id])
        .current_dir(&secondary)
        .output()?;
    assert_success("jj edit on secondary", &edit_output);

    let output = repo.run_in(&secondary, &["submit", "--revset", REVSET])?;
    assert_success("submit from secondary workspace", &output);

    // Real `jj git push` ran via the backing repo; the branch reached the
    // shared remote.
    assert_eq!(repo.git_remote_branch_target(&branch)?, change.commit_id);
    // Cache is stored at `<backing>/.jj/repo/stack/...`, never inside the
    // secondary workspace's `.jj` directory.
    assert!(
        repo.cache_path().exists(),
        "submit from secondary workspace should write cache to backing repo"
    );
    assert!(
        !secondary.join(".jj/repo/stack").exists(),
        "secondary workspace must not get its own stack cache directory"
    );
    Ok(())
}
