// End-to-end `merge` tests driving the real `forklift` binary against a real colocated jj repo.

mod common;

use common::*;
use serde_json::json;

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
fn merge_approval_failure_mentions_bypass_flags() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-approval-required")?;
    repo.init_main()?;
    let main = repo.bookmark_target("main")?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 7)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);
    repo.set_pr_review_decision(7, "NONE")?;

    let output = repo.run(&["merge"])?;
    assert!(
        !output.status.success(),
        "merge should fail while approval is required"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("error: failed during merge-pr-check"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("PR #7 requires approval; reviewDecision is `NONE`"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("forklift merge --no-require-approval")
            && stderr.contains("forklift merge --admin"),
        "stderr should mention approval bypass flags:\n{stderr}"
    );
    assert!(
        !stderr.contains("resolution:\n  run `forklift submit --dry-run`"),
        "approval failure should not fall back to generic submit dry-run guidance:\n{stderr}"
    );
    assert_eq!(
        repo.git_remote_branch_target("main")?,
        main,
        "merge must not advance trunk"
    );
    assert_eq!(repo.stored_pr(7)?["state"], json!("OPEN"));
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
        stderr.contains("error: merge target is frozen"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("covered by `forklift/frozen/pr-1`"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "resolution:\n  run `forklift unfreeze 1`, then `forklift sync 1 --submit --yes`, then rerun `forklift merge 1`"
        ),
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
fn targeted_merge_reports_all_frozen_descendants_covering_target() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-target-frozen-descendants")?;
    repo.init_main()?;
    let main = repo.bookmark_target("main")?;
    let stack = repo.create_linear_stack(3)?;
    let bottom = branch_for("change-1-title", &stack[0].change_id);
    let middle = branch_for("change-2-title", &stack[1].change_id);
    let top = branch_for("change-3-title", &stack[2].change_id);
    repo.set_bookmark(&bottom, &stack[0].commit_id)?;
    repo.set_bookmark(&middle, &stack[1].commit_id)?;
    repo.set_bookmark(&top, &stack[2].commit_id)?;
    repo.push_bookmark(&bottom)?;
    repo.push_bookmark(&middle)?;
    repo.push_bookmark(&top)?;
    repo.seed_pr(11, &bottom, "main", "change 1 title", "change 1 body")?;
    repo.seed_pr(12, &middle, &bottom, "change 2 title", "change 2 body")?;
    repo.seed_pr(13, &top, &middle, "change 3 title", "change 3 body")?;
    repo.set_bookmark("forklift/frozen/pr-12", &stack[1].commit_id)?;
    repo.set_bookmark("forklift/frozen/pr-13", &stack[2].commit_id)?;

    let output = repo.run(&["merge", "11"])?;
    assert!(
        !output.status.success(),
        "targeted merge should fail non-interactively while frozen descendants cover the target"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("covered by `forklift/frozen/pr-13`, `forklift/frozen/pr-12`"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "resolution:\n  run `forklift unfreeze 13`, then `forklift unfreeze 12`, then `forklift sync 11 --submit --yes`, then rerun `forklift merge 11`"
        ),
        "stderr:\n{stderr}"
    );
    assert_eq!(
        repo.git_remote_branch_target("main")?,
        main,
        "merge must not advance trunk"
    );
    Ok(())
}

#[test]
fn targeted_merge_unfreezes_then_sync_submits_before_retry() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-unfreeze-sync-submit")?;
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
    let advanced = repo.advance_remote_trunk("remote work", &stack[1].change_id)?;
    repo.set_bookmark("forklift/frozen/pr-12", &stack[1].commit_id)?;
    repo.clear_gh_requests()?;

    let output = repo.run_tty_with_stdin(&["merge", "11", "--admin"], "y\n")?;
    assert_success("merge 11 --admin with unfreeze prompt", &output);
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("sync") || stdout_of(&output).contains("sync"),
        "merge should report the recovery flow\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&output),
        stderr
    );

    let merged_head = repo.git_remote_branch_target("main")?;
    assert_eq!(
        repo.git_remote_branch_target(&bottom)?,
        merged_head,
        "sync+submit should push the rebased target PR before merge"
    );
    assert_ne!(
        merged_head, stack[0].commit_id,
        "merge should not use the stale pre-sync PR head"
    );
    assert_ne!(
        merged_head, advanced.commit_id,
        "merge should fast-forward trunk past the fetched remote tip"
    );
    assert_eq!(repo.stored_pr(11)?["state"], json!("MERGED"));
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
fn merge_unsubmitted_single_change_points_to_submit() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-unsubmitted-single")?;
    repo.init_main()?;
    let local_only = repo.create_change("local-only", "local only title", "local only body")?;

    let output = repo.run(&["merge"])?;
    assert!(
        !output.status.success(),
        "merge should fail when the only stack change is unsubmitted"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&format!(
            "change {} is still local-only",
            &local_only.change_id[..8]
        )),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("resolution:\n  run `forklift submit --yes`, then `forklift merge`"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("resolution:\n  run `forklift submit --dry-run`"),
        "single local-only merge should point to submit, not dry-run, stderr:\n{stderr}"
    );
    Ok(())
}

#[test]
fn merge_unsubmitted_child_explains_local_only_change() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-unsubmitted-child")?;
    repo.init_main()?;
    let submitted = repo.create_change("submitted", "submitted title", "submitted body")?;
    let branch = branch_for("submitted-title", &submitted.change_id);
    repo.seed_pr_number(&branch, 32)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);

    let local_only = repo.create_change("local-only", "local only title", "local only body")?;

    let output = repo.run(&["merge"])?;
    assert!(
        !output.status.success(),
        "merge should fail when the stack has an unsubmitted child"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&format!(
            "change {} is still local-only",
            &local_only.change_id[..8]
        )),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("no tracked stack bookmark or GitHub PR was found for `local only title`"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("merge can only verify submitted changes")
            && stderr.contains("resolution:\n  run `forklift submit --yes`, then `forklift merge`"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("resolution:\n  run `forklift submit --dry-run`"),
        "local-only merge should point to submit, not dry-run, stderr:\n{stderr}"
    );
    assert_ne!(repo.stored_pr(32)?["state"], json!("MERGED"));
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
fn merge_recovers_when_remote_trunk_moved_externally() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-external-trunk-move")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);

    // Another clone lands work on trunk; this workspace's tracking refs are
    // now stale, the state that used to make the merge push fail with jj's
    // unrecoverable "stale info" refusal.
    let external = repo.advance_remote_trunk_externally("external work")?;

    // Merge fetches, sees trunk cannot fast-forward, and offers the
    // sync+submit recovery; accept it and the retried merge lands.
    let output = repo.run_tty_with_stdin(&["merge", "--admin"], "y\n")?;
    assert_success("merge --admin after external trunk move", &output);
    let stdout = stdout_of(&output);
    assert!(
        !stdout.contains("stale info"),
        "merge must fetch instead of tripping jj's stale-info push check\nstdout:\n{stdout}"
    );

    // The stack was rebased onto the external commit and trunk fast-forwarded
    // over it.
    let rebased = repo.change_at(&change.change_id)?;
    assert_ne!(
        rebased.commit_id, change.commit_id,
        "recovery should rebase the stack onto the moved trunk"
    );
    let parent = repo.rev_commit_id(&format!("{}-", rebased.commit_id))?;
    assert_eq!(parent, external, "stack should sit on the external commit");
    assert_eq!(repo.git_remote_branch_target("main")?, rebased.commit_id);
    assert_eq!(repo.stored_pr(9)?["state"], json!("MERGED"));
    Ok(())
}

#[test]
fn merge_surfaces_sync_guidance_when_remote_trunk_moved_externally() -> anyhow::Result<()> {
    let repo = TestRepo::new("merge-external-trunk-move-non-tty")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);
    let external = repo.advance_remote_trunk_externally("external work")?;

    let output = repo.run(&["merge", "--admin"])?;
    assert!(
        !output.status.success(),
        "non-interactive merge cannot run the sync recovery and must fail"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("forklift merge --sync"),
        "failure should point at the sync recovery command\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("stale info"),
        "merge must fail with sync guidance, not jj's stale-info push refusal\nstderr:\n{stderr}"
    );

    // Nothing merged: remote trunk stays on the external commit, PR stays open.
    assert_eq!(repo.git_remote_branch_target("main")?, external);
    assert_eq!(repo.stored_pr(9)?["state"], json!("OPEN"));
    Ok(())
}
