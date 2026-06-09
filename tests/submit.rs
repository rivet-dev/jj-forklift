// End-to-end `submit` tests driving the real `forklift` binary against a real colocated jj repo.

mod common;

use common::*;
use serde_json::json;

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

    let output = repo.run_in(&secondary, &["submit", "--yes"])?;
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

#[test]
fn submit_repoints_pr_bookmark_stranded_on_divergent_copy() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-divergent")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 7)?;

    // Force the change ID to diverge into two visible commits by rewriting it at
    // two different operations. `--at-operation @-` rewrites the change as it was
    // before the previous describe, creating a sibling copy rather than obsoleting
    // the first. jj then marks every copy of the change ID as divergent.
    repo.jj(&["describe", "-m", "change title", "-m", "rewrite one"])?;
    repo.jj(&[
        "describe",
        "--at-operation",
        "@-",
        "-m",
        "change title",
        "-m",
        "rewrite two",
    ])?;

    // The copy reachable from @ is the one forklift submits; the other is stale.
    let selected = repo.rev_commit_id(&format!("change_id({}) & ::@", change.change_id))?;
    let stale = repo.rev_commit_id(&format!("change_id({}) ~ ::@", change.change_id))?;

    // Reproduce the real bug: the PR branch is stranded on the *stale* copy, not
    // the one under @. Submit must still re-point it onto the selected copy rather
    // than bailing because the bookmark "points elsewhere".
    repo.set_bookmark(&branch, &stale)?;

    let output = repo.run(&["submit", "--yes"])?;
    assert_success("submit", &output);
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("is divergent") && stderr.contains("other copies left untouched"),
        "submit should print the one-line divergence notice\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("jj abandon"),
        "submit must not prescribe abandoning the stale copy\nstderr:\n{stderr}"
    );

    // The PR was opened against the @-copy, the local bookmark was re-pointed off
    // the stale copy, and the @-copy is the commit pushed.
    let pr = repo.stored_pr(7)?;
    assert_eq!(pr["headRefName"], json!(branch));
    assert_eq!(repo.bookmark_target(&branch)?, selected);
    assert_eq!(repo.git_remote_branch_target(&branch)?, selected);
    Ok(())
}
