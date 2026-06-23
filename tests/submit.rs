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
fn submit_refreshes_stale_remote_bookmarks_before_pushing() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-stale-remote")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;

    repo.set_bookmark(&branch, &change.commit_id)?;
    repo.push_bookmark(&branch)?;
    repo.delete_remote_branch_directly(&branch)?;
    assert!(
        !repo.remote_branch_exists(&branch)?,
        "test setup should delete only the real remote branch"
    );

    let output = repo.run(&["submit", "--yes"])?;
    assert_success("submit", &output);

    assert_eq!(repo.git_remote_branch_target(&branch)?, change.commit_id);
    assert!(repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    Ok(())
}

#[test]
fn submit_rejects_undescribed_empty_spacer_below_stack() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-undescribed-spacer")?;
    repo.init_main()?;
    // The classic leftover `jj new`: an empty, undescribed commit between `main`
    // and the stack root. `init_main` already leaves an empty undescribed commit
    // on `main`; stacking one further up turns it into a spacer below the change.
    // The stack revset (`~empty()`) skips it, but jj refuses to push an
    // undescribed commit, so submit must reject it up front — before any bookmark
    // is pushed — with an actionable message, not abort mid-push.
    repo.jj(&["new"])?; // fresh empty working copy above the undescribed spacer
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 7)?;

    let output = repo.run(&["submit", "--yes"])?;
    assert!(
        !output.status.success(),
        "submit must reject an undescribed empty spacer"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("empty commit with no description sits between base `main`"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("jj abandon"),
        "error should suggest abandoning the spacer\nstderr:\n{stderr}"
    );
    // Rejected before any mutation: no PR created, no branch pushed.
    assert!(!repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    assert!(!repo.remote_branch_exists(&branch)?);
    Ok(())
}

#[test]
fn submit_tolerates_described_empty_spacer_below_stack() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-described-spacer")?;
    repo.init_main()?;
    // An empty but *described* commit between `main` and the stack root is also
    // skipped by the stack revset, yet it pushes fine, so submit must tolerate
    // it rather than fail on the merge-base mismatch. Describe the empty commit
    // `init_main` leaves on `main`, then stack the change above it.
    repo.jj(&["describe", "-m", "empty spacer"])?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 7)?;

    let output = repo.run(&["submit", "--yes"])?;
    assert_success("submit", &output);

    // The PR was created against trunk despite the empty spacer between them.
    assert_eq!(repo.git_remote_branch_target(&branch)?, change.commit_id);
    let pr = repo.stored_pr(7)?;
    assert_eq!(pr["headRefName"], json!(branch));
    assert_eq!(pr["baseRefName"], json!("main"));
    Ok(())
}

#[test]
fn submit_names_empty_change_wedged_in_middle_of_stack() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-mid-stack-empty")?;
    repo.init_main()?;
    // A described but empty change wedged between real changes (the leftover of a
    // rebase or squash). The stack revset's `~empty()` drops it, severing the
    // parent link so the change above looks like a second root. Submit must name
    // the empty change, not bail with the misleading "multiple roots".
    repo.create_change("bottom", "bottom title", "bottom body")?;
    repo.create_change("lower", "lower title", "lower body")?;
    let empty = repo.create_empty_change("leftover empty after squash")?;
    repo.create_change("upper", "upper title", "upper body")?;
    repo.create_change("top", "top title", "top body")?;

    let output = repo.run(&["submit", "--yes"])?;
    assert!(
        !output.status.success(),
        "submit must reject a stack fragmented by a mid-stack empty change\nstderr:\n{}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("wedged in the middle of the stack"),
        "error should name the empty change as the culprit\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("jj abandon {}", &empty.change_id[..8])),
        "resolution should suggest abandoning the specific empty change\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("stack has multiple roots"),
        "the misleading multiple-roots message should be replaced\nstderr:\n{stderr}"
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

// A rebase/squash can fold one change into another, leaving jj to strand the
// absorbed change's `stack/` bookmark on the surviving commit. That commit then
// carries two PR branches. Submit must keep the bookmark whose change-id suffix
// matches the commit (the canonical owner), submit it, and offer to close the
// orphaned PR. Reproduced end-to-end with the real `jj squash` + `jj rebase`.
#[test]
fn submit_tolerates_collapsed_bookmarks_and_closes_orphan() -> anyhow::Result<()> {
    let repo = TestRepo::new("collapsed-bookmarks")?;
    repo.init_main()?;
    let stack = repo.create_linear_stack(2)?;
    let canonical = branch_for("change-1-title", &stack[0].change_id);
    let orphan = branch_for("change-2-title", &stack[1].change_id);
    repo.seed_pr_number(&canonical, 11)?;
    repo.seed_pr_number(&orphan, 12)?;

    // First submit opens both PRs and pushes both branches.
    assert_success("initial submit", &repo.run(&["submit", "--yes"])?);
    assert_eq!(repo.stored_pr(11)?["state"], json!("OPEN"));
    assert_eq!(repo.stored_pr(12)?["state"], json!("OPEN"));

    // Fold the top change into the bottom: jj abandons the top and relocates its
    // bookmark onto the bottom commit, so the bottom now carries both branches.
    repo.jj(&[
        "squash",
        "--from",
        &stack[1].change_id,
        "--into",
        &stack[0].change_id,
        "--use-destination-message",
    ])?;
    assert_eq!(
        repo.bookmark_target(&orphan)?,
        repo.bookmark_target(&canonical)?,
        "squash should strand the orphan bookmark onto the canonical commit"
    );

    // Advance trunk and rebase the collapsed change onto it — the real `jj rebase`
    // that surfaces the collision (commit ids change, but both bookmarks follow).
    repo.jj(&["new", "main", "-m", "advance trunk"])?;
    repo.write_file("trunk2.txt", "two\n")?;
    repo.jj(&["describe", "-m", "advance trunk"])?;
    repo.set_bookmark("main", "@")?;
    repo.push_bookmark("main")?;
    repo.jj(&["rebase", "-r", &stack[0].change_id, "-d", "main"])?;
    repo.jj(&["edit", &stack[0].change_id])?;
    let collapsed = repo.change_at(&stack[0].change_id)?;

    // Submit again. Without the fix this bails during planning ("multiple local
    // bookmarks ... have open GitHub PRs"); with it, the canonical PR proceeds
    // and the orphan is closed (--yes consents to the close).
    let output = repo.run(&["submit", "--yes"])?;
    assert_success("submit after collapse", &output);
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("was absorbed into") && stderr.contains(&orphan),
        "submit should warn that the orphan bookmark was absorbed\nstderr:\n{stderr}"
    );

    // The canonical PR is still open and re-pushed to the rebased commit.
    assert_eq!(repo.stored_pr(11)?["state"], json!("OPEN"));
    assert_eq!(
        repo.git_remote_branch_target(&canonical)?,
        collapsed.commit_id
    );
    assert_eq!(repo.bookmark_target(&canonical)?, collapsed.commit_id);

    // The orphaned PR was closed and its branch deleted locally and on the remote.
    assert_eq!(repo.stored_pr(12)?["state"], json!("CLOSED"));
    assert!(
        !repo.remote_branch_exists(&orphan)?,
        "orphan remote branch should be deleted"
    );
    assert!(
        !repo.bookmark_exists(&orphan)?,
        "orphan local bookmark should be forgotten"
    );
    Ok(())
}

#[test]
fn submit_rebases_stack_when_trunk_moved() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-trunk-moved")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;

    // Upstream trunk moves and the local bookmark follows it (as the pre-submit
    // fetch does), stranding the stack root behind the new trunk. Submit used to
    // fail base validation here and demand a manual `forklift sync`.
    let advanced = repo.advance_remote_trunk("remote work", &change.change_id)?;
    repo.set_bookmark("main", &advanced.commit_id)?;

    let output = repo.run(&["submit", "--yes"])?;
    assert_success("submit", &output);

    // The stack was rebased onto the new trunk before pushing.
    let rebased = repo.change_at(&change.change_id)?;
    assert_ne!(
        rebased.commit_id, change.commit_id,
        "submit should rebase the stranded stack"
    );
    let parent = repo.rev_commit_id(&format!("{}-", rebased.commit_id))?;
    assert_eq!(parent, advanced.commit_id, "stack should sit on the new trunk");
    assert_eq!(repo.git_remote_branch_target(&branch)?, rebased.commit_id);
    let pr = repo.stored_pr(9)?;
    assert_eq!(pr["headRefName"], json!(branch));
    assert_eq!(pr["baseRefName"], json!("main"));
    Ok(())
}

#[test]
fn submit_stops_before_pushing_when_trunk_rebase_conflicts() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-trunk-rebase-conflict")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;

    // Advance trunk with a commit that rewrites the same file the stack change
    // touches, so the automatic rebase conflicts. Leave the local bookmark on
    // the new tip, matching the post-fetch state.
    repo.jj(&["new", "main"])?;
    repo.write_file("change.txt", "conflicting remote contents\n")?;
    repo.jj(&["describe", "-m", "remote work"])?;
    repo.jj(&["bookmark", "set", "main", "-r", "@"])?;
    repo.push_bookmark("main")?;
    repo.jj(&["edit", &change.commit_id])?;

    let output = repo.run(&["submit", "--yes"])?;
    assert!(
        !output.status.success(),
        "submit must not push a conflicted rebase\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("conflict"),
        "submit should report the rebase conflict\nstderr:\n{stderr}"
    );

    // Nothing was pushed and no PR was created.
    assert!(
        !repo.remote_branch_exists(&branch)?,
        "conflicted submit must not push the PR branch"
    );
    assert!(!repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    Ok(())
}

#[test]
fn submit_dry_run_plans_rebase_when_trunk_moved() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-trunk-moved-dry-run")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    let advanced = repo.advance_remote_trunk("remote work", &change.change_id)?;
    repo.set_bookmark("main", &advanced.commit_id)?;

    let output = repo.run(&["submit", "--dry-run"])?;
    assert_success("submit --dry-run", &output);
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("rebase stack root"),
        "dry run should plan the trunk rebase\nstdout:\n{stdout}"
    );

    // The dry run changed nothing: the stack still sits on the old trunk and
    // nothing was pushed or created.
    let unchanged = repo.change_at(&change.change_id)?;
    assert_eq!(unchanged.commit_id, change.commit_id);
    assert!(!repo.remote_branch_exists(&branch)?);
    assert!(!repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    Ok(())
}

#[test]
fn submit_prompts_to_sync_when_trunk_moved() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-trunk-moved-prompt-yes")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    let advanced = repo.advance_remote_trunk("remote work", &change.change_id)?;
    repo.set_bookmark("main", &advanced.commit_id)?;

    // First "y" accepts the sync, the second applies the submit plan.
    let output = repo.run_tty_with_stdin(&["submit"], "y\ny\n")?;
    assert_success("submit", &output);
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("sync before submit? [y/N]"),
        "submit should ask before syncing\nstdout:\n{stdout}"
    );

    // Accepting rebased the stack onto the new trunk and pushed the result.
    let rebased = repo.change_at(&change.change_id)?;
    let parent = repo.rev_commit_id(&format!("{}-", rebased.commit_id))?;
    assert_eq!(parent, advanced.commit_id, "stack should sit on the new trunk");
    assert_eq!(repo.git_remote_branch_target(&branch)?, rebased.commit_id);
    let pr = repo.stored_pr(9)?;
    assert_eq!(pr["headRefName"], json!(branch));
    Ok(())
}

#[test]
fn submit_declined_sync_changes_nothing() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-trunk-moved-prompt-no")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    let advanced = repo.advance_remote_trunk("remote work", &change.change_id)?;
    repo.set_bookmark("main", &advanced.commit_id)?;

    let output = repo.run_tty_with_stdin(&["submit"], "n\n")?;
    assert!(
        !output.status.success(),
        "declined sync must cancel the submit\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("submit cancelled"),
        "decline should cancel cleanly\nstdout:\n{stdout}"
    );

    // Declining mutated nothing: no rebase, no push, no PR.
    let unchanged = repo.change_at(&change.change_id)?;
    assert_eq!(unchanged.commit_id, change.commit_id);
    assert!(!repo.remote_branch_exists(&branch)?);
    assert!(!repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    Ok(())
}

#[test]
fn submit_behind_trunk_requires_yes_when_not_a_terminal() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-trunk-moved-non-tty")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    let advanced = repo.advance_remote_trunk("remote work", &change.change_id)?;
    repo.set_bookmark("main", &advanced.commit_id)?;

    let output = repo.run(&["submit"])?;
    assert!(
        !output.status.success(),
        "non-interactive submit of a stale stack must fail\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("submit requires the stack to be synced"),
        "stderr:\n{stderr}"
    );

    let unchanged = repo.change_at(&change.change_id)?;
    assert_eq!(unchanged.commit_id, change.commit_id);
    assert!(!repo.remote_branch_exists(&branch)?);
    assert!(!repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?);
    Ok(())
}

#[test]
fn submit_repins_to_rewritten_commit_instead_of_resurrecting_hidden_id() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-mid-run-rewrite")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;

    // While submit waits at the confirmation prompt, a concurrent tool edits a
    // file in the working copy (@ sits on the stack change). The next jj
    // command's snapshot rewrites the change, hiding the commit id the plan
    // pinned — pushing that hidden id used to resurrect it and leave the
    // change divergent.
    let work_file = repo.work.join("change.txt");
    let output = repo.run_tty_prompt_then_input(
        &["submit"],
        "Apply submit? [y/N]",
        || Ok(std::fs::write(&work_file, "edited while submit waited\n")?),
        "y\n",
    )?;
    assert_success("submit", &output);
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("was rewritten from"),
        "submit should report the re-pin\nstdout:\n{stdout}"
    );

    // The change resolves to a single (non-divergent) commit containing the
    // edit, and that commit — not the stale planned id — is what was pushed.
    let current = repo.change_at(&change.change_id)?;
    assert_ne!(
        current.commit_id, change.commit_id,
        "the snapshot should have rewritten the change"
    );
    assert_eq!(repo.git_remote_branch_target(&branch)?, current.commit_id);
    assert_eq!(repo.stored_pr(9)?["headRefName"], json!(branch));
    Ok(())
}

/// Regression: the owned stack root sits on an un-merged commit that is the head
/// of an open PR (it carries a pushed `stack/*` remote bookmark) but was never
/// frozen via `forklift get`. Submit must refuse rather than silently base the
/// bottom PR on trunk, which would bloat its diff with the un-merged parent.
#[test]
fn submit_refuses_when_owned_root_parent_is_unmerged_open_pr() -> anyhow::Result<()> {
    let repo = TestRepo::new("submit-unmerged-parent")?;
    repo.init_main()?;

    // An open PR's head commit: pushed and made immutable via an untracked
    // remote bookmark (so it falls out of the owned stack), but never frozen.
    let parent = repo.create_change("parent", "parent title", "parent body")?;
    let parent_branch = branch_for("parent-title", &parent.change_id);
    repo.set_bookmark(&parent_branch, &parent.commit_id)?;
    repo.push_bookmark(&parent_branch)?;
    repo.seed_pr(11, &parent_branch, "main", "parent title", "parent body")?;
    repo.jj(&["bookmark", "untrack", &format!("{parent_branch}@origin")])?;
    assert!(
        !repo.is_mutable(&parent.commit_id)?,
        "parent should be immutable so it falls out of the owned stack"
    );

    // The owned stack sits directly on that un-merged, un-frozen parent.
    let child = repo.create_change("child", "child title", "child body")?;
    let child_branch = branch_for("child-title", &child.change_id);
    repo.seed_pr_number(&child_branch, 12)?;

    let output = repo.run(&["submit", "--yes"])?;
    assert!(
        !output.status.success(),
        "submit should refuse a stack based on an un-merged parent\nstderr:\n{}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("un-merged commit") && stderr.contains("not a frozen dependency"),
        "stderr should explain the un-merged base:\n{stderr}"
    );
    // It must not have created any PR for the child (which would be trunk-based).
    assert!(
        !repo.gh_request_matches(&["api", "-X", "POST", "repos/owner/repo/pulls"])?,
        "submit must not create a trunk-based PR\nstderr:\n{stderr}"
    );
    assert!(
        !repo.remote_branch_exists(&child_branch)?,
        "submit must not push the child branch"
    );
    Ok(())
}
