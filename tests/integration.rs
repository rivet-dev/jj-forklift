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

/// The stack revset the original suite used; `@` sits at the top of the stack.
const REVSET: &str = "main..@ & ~empty()";

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

    let output = repo.run(&["submit", "--revset", REVSET])?;
    assert_success("submit", &output);
    assert!(
        stderr_of(&output).contains(
            "Submitted PR #9 https://github.com/owner/repo/pull/9 - action: submit - change title"
        ),
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
fn submit_dry_run_skips_cache_writes() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-dry-run")?;
    repo.init_main()?;
    repo.create_change("change", "change title", "change body")?;

    let output = repo.run(&["submit", "--dry-run", "--revset", REVSET])?;
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
fn two_change_submit_uses_parent_head_branch_base() -> anyhow::Result<()> {
    let repo = TestRepo::new("two-submit")?;
    repo.init_main()?;
    let stack = repo.create_linear_stack(2)?;
    let bottom = branch_for("change-1-title", &stack[0].change_id);
    let top = branch_for("change-2-title", &stack[1].change_id);
    repo.seed_pr_number(&bottom, 11)?;
    repo.seed_pr_number(&top, 12)?;

    let output = repo.run(&["submit", "--revset", REVSET])?;
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
    assert_success(
        "initial submit",
        &repo.run(&["submit", "--revset", REVSET])?,
    );

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

    let output = repo.run(&["submit", "--revset", REVSET])?;
    assert_success("update submit", &output);
    assert!(
        stderr_of(&output).contains(
            "Updated PR #11 https://github.com/owner/repo/pull/11 - action: update - change 1 title edited"
        ),
        "stderr:\n{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains(
            "Updated PR #12 https://github.com/owner/repo/pull/12 - action: update - change 2 title"
        ),
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
    assert_success(
        "initial submit",
        &repo.run(&["submit", "--revset", REVSET])?,
    );
    let pushed = repo.git_remote_branch_target(&branch)?;

    repo.clear_gh_requests()?;
    let output = repo.run(&["submit", "--revset", REVSET])?;
    assert_success("noop submit", &output);
    assert!(
        stderr_of(&output).contains(
            "Nothing PR #9 https://github.com/owner/repo/pull/9 - action: nothing - change title"
        ),
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
    assert_success(
        "initial submit",
        &repo.run(&["submit", "--revset", REVSET])?,
    );

    // Drop the cache; submit must rediscover the PR from the tracked bookmark.
    repo.delete_cache()?;
    repo.write_file("change.txt", "change\nchange title\nedited\n")?;
    repo.jj(&["describe", "-m", "change title", "-m", "edited body"])?;
    let edited = repo.change_at("@")?;

    repo.clear_gh_requests()?;
    assert_success("update submit", &repo.run(&["submit", "--revset", REVSET])?);

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

    let output = repo.run(&["submit", "--revset", REVSET])?;
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

    let output = repo.run(&["submit", "--revset", REVSET])?;
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
    assert_success("submit", &repo.run(&["submit", "--revset", REVSET])?);

    repo.clear_gh_requests()?;
    let output = repo.run(&["merge", "--dry-run", "--revset", REVSET])?;
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
    assert_success("submit", &repo.run(&["submit", "--revset", REVSET])?);

    repo.delete_cache()?;
    repo.clear_gh_requests()?;
    let output = repo.run(&["merge", "--dry-run", "--revset", REVSET])?;
    assert_success("merge --dry-run", &output);

    assert!(repo.gh_request_matches(&["pr", "view", "9"])?);
    assert!(!repo.gh_request_matches(&["pr", "merge", "9"])?);
    assert_eq!(repo.stored_pr(9)?["state"], json!("OPEN"));
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
    assert_success("submit", &repo.run(&["submit", "--revset", REVSET])?);
    let top_commit = repo.change_at(&stack[1].change_id)?.commit_id;

    repo.clear_gh_requests()?;
    let output = repo.run(&["merge", "--revset", REVSET])?;
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
fn sync_rebases_then_submits() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-rebase-submit")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let advanced = repo.advance_remote_trunk("remote work", &change.change_id)?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;

    let output = repo.run(&["sync", "--submit", "--revset", REVSET])?;
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
fn sync_divergence_stops_before_rebase() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-divergence")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let divergent = repo.diverge_remote_trunk("divergent trunk", &change.change_id)?;
    let before = repo.change_at(&change.change_id)?.commit_id;

    let output = repo.run(&["sync", "--revset", REVSET])?;
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
    assert!(
        stdout_of(&output).contains("next: `jj new forklift/frozen/pr-11`"),
        "stdout:\n{}",
        stdout_of(&output)
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

    let output = repo.run(&["get", "12"])?;
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
        stdout_of(&output).contains("next: `jj new forklift/frozen/pr-12`"),
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

// NOTE: the original suite had `non_default_workspace_writes_cache_to_backing_repo`,
// which ran a full `submit` from a jj *secondary* workspace and asserted the
// cache landed in the backing repo's `.jj/repo/stack`. That only worked because
// `git` was faked: a real jj secondary workspace is NOT colocated, so the real
// `git` commands `forklift` relies on (e.g. resolving a commit's tree) cannot
// run there. The behavior it actually exercised — `resolve_jj_repo_dir`
// following the `.jj/repo` pointer file to the backing repo — is covered as a
// pure unit test (see the lib-extraction phase). Mocking `git`/`jj` to resurrect
// the old form here is exactly what this migration removes.
