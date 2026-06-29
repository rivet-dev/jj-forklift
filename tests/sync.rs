// End-to-end `sync` tests driving the real `forklift` binary against a real colocated jj repo.

mod common;

use common::*;

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
fn sync_prompts_to_submit_clean_rebase() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-prompt-submit")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);
    let submitted = repo.change_at(&change.change_id)?;
    repo.advance_remote_trunk("remote work", &change.change_id)?;
    repo.clear_gh_requests()?;

    let output = repo.run_tty_with_stdin(&["sync"], "y\n")?;
    assert_success("sync", &output);
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("Submit updated PRs now? [y/N]"),
        "stdout:\n{stdout}"
    );

    let rebased = repo.change_at(&change.change_id)?;
    assert_ne!(
        rebased.commit_id, submitted.commit_id,
        "sync should rebase the change before submitting"
    );
    assert_eq!(
        repo.git_remote_branch_target(&branch)?,
        rebased.commit_id,
        "prompted submit should push the rebased PR branch"
    );
    assert!(repo.gh_request_matches(&["api", "-X", "PATCH", "repos/owner/repo/pulls/9"])?);
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
        stderr.contains("Finished sync — 1 stack(s), 1 roots rebased, 1 conflict(s), submit skipped"),
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
fn sync_from_empty_child_above_frozen_pr_succeeds() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-empty-child-frozen")?;
    repo.init_main()?;
    let imported = repo.create_change("imported", "imported title", "imported body")?;
    let branch = branch_for("imported-title", &imported.change_id);
    repo.set_bookmark(&branch, &imported.commit_id)?;
    repo.push_bookmark(&branch)?;
    repo.seed_pr(11, &branch, "main", "imported title", "imported body")?;

    let get_output = repo.run(&["get", "11"])?;
    assert_success("get 11", &get_output);
    assert_eq!(
        repo.rev_commit_id("@-")?,
        imported.commit_id,
        "get should leave @ on an empty child above the frozen PR"
    );

    let output = repo.run(&["sync"])?;
    assert_success("sync", &output);
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("Finished sync — 0 stack(s), 0 roots rebased"),
        "stderr:\n{stderr}"
    );
    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-11")?,
        imported.commit_id
    );
    Ok(())
}

#[test]
fn sync_frozen_suffix_based_on_unfrozen_parent_succeeds() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-frozen-suffix")?;
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
    repo.set_bookmark("forklift/frozen/pr-12", &stack[1].commit_id)?;
    repo.jj(&["new", &stack[1].commit_id])?;

    let output = repo.run(&["sync"])?;
    assert_success("sync", &output);
    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-12")?,
        stack[1].commit_id
    );
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
        stderr.contains(&divergent.local[..8]),
        "stderr should cite local trunk:\n{stderr}"
    );
    assert!(
        stderr.contains(&divergent.remote[..8]),
        "stderr should cite divergent remote trunk:\n{stderr}"
    );
    // The change was not rebased.
    assert_eq!(repo.change_at(&change.change_id)?.commit_id, before);
    Ok(())
}

#[test]
fn sync_prunes_duplicate_change_already_landed_on_remote_trunk() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-landed-duplicate-change-id")?;
    let main = repo.init_main()?;
    let landed = repo.create_change(
        "cold-storage",
        "refactor(depot): remove sqlite cold storage",
        "remove cold storage",
    )?;
    let child = repo.create_change("followup", "fix(depot): followup", "keep me")?;
    let local_stack_op = repo.current_operation_id()?;

    repo.set_bookmark("main", &landed.commit_id)?;
    repo.push_bookmark("main")?;
    repo.jj(&["op", "restore", &local_stack_op])?;
    repo.jj(&["edit", &landed.change_id])?;
    repo.write_file("cold-storage.txt", "local duplicate rewrite\n")?;
    repo.jj(&[
        "describe",
        "-m",
        "refactor(depot): remove sqlite cold storage",
        "-m",
        "remove cold storage",
    ])?;
    let duplicate = repo.change_at(&landed.change_id)?;
    assert_ne!(
        duplicate.commit_id, landed.commit_id,
        "test setup should create a local divergent copy"
    );
    repo.jj(&["edit", &child.change_id])?;

    let output = repo.run(&["sync"])?;
    assert_success("sync", &output);
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("already exists on `main@origin`; pruning local duplicate"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("1 duplicate(s) pruned"),
        "stderr:\n{stderr}"
    );

    let child_after = repo.change_at(&child.change_id)?;
    let child_parent = repo.rev_commit_id(&format!("{}-", child_after.commit_id))?;
    assert_eq!(
        child_parent, landed.commit_id,
        "sync should rebase the surviving child onto the landed remote trunk copy"
    );
    assert_eq!(
        repo.bookmark_target("main")?,
        landed.commit_id,
        "sync should move local trunk to the landed remote copy"
    );
    assert_eq!(
        repo.rev_commit_id(&format!("change_id({})", landed.change_id))?,
        landed.commit_id,
        "local duplicate should be abandoned; only the landed copy remains"
    );
    assert_ne!(
        repo.bookmark_target("main")?,
        main.commit_id,
        "test setup should advance trunk"
    );
    Ok(())
}

#[test]
fn sync_ignores_deleted_local_stack_bookmark_markers() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-deleted-local-bookmark-marker")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.set_bookmark(&branch, &change.commit_id)?;
    repo.push_bookmark(&branch)?;
    repo.jj(&["bookmark", "delete", &branch])?;
    assert!(
        repo.remote_branch_exists(&branch)?,
        "test setup should leave the remote bookmark intact"
    );

    let output = repo.run(&["sync"])?;
    assert_success("sync", &output);
    assert!(
        repo.remote_branch_exists(&branch)?,
        "sync cleanup should ignore the local deleted marker rather than push a deletion"
    );
    Ok(())
}

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
fn sync_recovers_trunk_stranded_on_stack_by_failed_merge_push() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-stranded-trunk")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    let branch = branch_for("change-title", &change.change_id);
    repo.seed_pr_number(&branch, 9)?;
    assert_success("submit", &repo.run(&["submit", "--yes"])?);

    // Replicate a merge whose push failed after moving the bookmark: local
    // trunk left on the stack top while another clone advanced the remote.
    repo.jj(&["bookmark", "set", "main", "-r", &change.commit_id])?;
    let external = repo.advance_remote_trunk_externally("external work")?;

    let output = repo.run(&["sync"])?;
    assert_success("sync", &output);

    // Trunk adopted the remote tip and the stack was rebased onto it; the
    // stranded commit was covered by its stack bookmark, so nothing was lost.
    assert_eq!(repo.bookmark_target("main")?, external);
    let rebased = repo.change_at(&change.change_id)?;
    assert_ne!(
        rebased.commit_id, change.commit_id,
        "sync should rebase the stack onto the recovered trunk"
    );
    let parent = repo.rev_commit_id(&format!("{}-", rebased.commit_id))?;
    assert_eq!(parent, external, "stack should sit on the external commit");
    Ok(())
}

#[test]
fn sync_carries_empty_working_copy_onto_moved_trunk() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-carry-empty-wc")?;
    repo.init_main()?;
    // A fresh `jj new main` working copy: empty, so the stack revset never
    // includes it and sync used to leave it stranded on the old trunk commit.
    repo.jj(&["new", "main"])?;
    let wc_before = repo.change_at("@")?;
    // Advance trunk from another clone so the local workspace (and its empty
    // working copy, which jj would discard on checkout) is never touched.
    let advanced = repo.advance_remote_trunk_externally("remote work")?;

    let output = repo.run(&["sync"])?;
    assert_success("sync", &output);

    // Trunk moved to the remote tip and the empty working copy moved with it.
    assert_eq!(repo.bookmark_target("main")?, advanced);
    let wc_after = repo.change_at("@")?;
    assert_eq!(
        wc_after.change_id, wc_before.change_id,
        "sync should move the same working-copy change, not create a new one"
    );
    let parent = repo.rev_commit_id(&format!("{}-", wc_after.commit_id))?;
    assert_eq!(parent, advanced, "working copy should sit on the new trunk");
    Ok(())
}

#[test]
fn sync_leaves_empty_working_copy_on_rebased_stack_top() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-empty-wc-on-stack")?;
    repo.init_main()?;
    let change = repo.create_change("change", "change title", "change body")?;
    // Empty working copy on top of the stack: it follows the stack rebase and
    // must not be re-targeted onto trunk away from its stack parent.
    repo.jj(&["new"])?;
    let wc_before = repo.change_at("@")?;
    let advanced = repo.advance_remote_trunk_externally("remote work")?;

    let output = repo.run(&["sync"])?;
    assert_success("sync", &output);

    let rebased = repo.change_at(&change.change_id)?;
    assert_ne!(rebased.commit_id, change.commit_id, "stack should be rebased");
    let stack_parent = repo.rev_commit_id(&format!("{}-", rebased.commit_id))?;
    assert_eq!(stack_parent, advanced);
    let wc_after = repo.change_at("@")?;
    assert_eq!(wc_after.change_id, wc_before.change_id);
    let wc_parent = repo.rev_commit_id(&format!("{}-", wc_after.commit_id))?;
    assert_eq!(
        wc_parent, rebased.commit_id,
        "empty working copy should stay on the rebased stack top, not move to trunk"
    );
    Ok(())
}

/// Regression: with the owned root on an un-merged, un-frozen open-PR commit,
/// sync must refuse rather than plan `jj rebase -s <root> -d trunk`, which would
/// drop the un-merged parent from the ancestry (silent history loss).
#[test]
fn sync_refuses_to_rebase_onto_trunk_past_unmerged_parent() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-unmerged-parent")?;
    repo.init_main()?;

    let parent = repo.create_change("parent", "parent title", "parent body")?;
    let parent_branch = branch_for("parent-title", &parent.change_id);
    repo.set_bookmark(&parent_branch, &parent.commit_id)?;
    repo.push_bookmark(&parent_branch)?;
    repo.seed_pr(11, &parent_branch, "main", "parent title", "parent body")?;
    repo.jj(&["bookmark", "untrack", &format!("{parent_branch}@origin")])?;

    let _child = repo.create_change("child", "child title", "child body")?;

    let output = repo.run(&["sync", "--dry-run"])?;
    assert!(
        !output.status.success(),
        "sync should refuse a stack based on an un-merged parent\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("un-merged commit") && stderr.contains("not a frozen dependency"),
        "stderr should explain the un-merged base:\n{stderr}"
    );
    // The destructive rebase onto trunk must never have been planned.
    let combined = format!("{}{stderr}", stdout_of(&output));
    assert!(
        !combined.contains("rebase stack root") && !combined.contains("-d main"),
        "sync must not plan a rebase onto trunk\noutput:\n{combined}"
    );
    Ok(())
}

#[test]
fn sync_with_no_target_rebases_all_tracked_stacks() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-all-stacks")?;
    repo.init_main()?;

    // Stack A rooted at trunk, submitted so it carries a `stack/*` bookmark.
    let a = repo.create_change("a", "a title", "a body")?;
    let branch_a = branch_for("a-title", &a.change_id);
    repo.seed_pr_number(&branch_a, 11)?;
    assert_success("submit A", &repo.run(&["submit", "--yes"])?);

    // Stack B is a sibling rooted at trunk, also submitted.
    repo.jj(&["new", "main"])?;
    let b = repo.create_change("b", "b title", "b body")?;
    let branch_b = branch_for("b-title", &b.change_id);
    repo.seed_pr_number(&branch_b, 12)?;
    assert_success("submit B", &repo.run(&["submit", "--yes"])?);

    // Remote trunk advances; both stacks are now behind.
    let advanced = repo.advance_remote_trunk("remote work", &b.change_id)?;

    // `forklift sync` with no target rebases BOTH tracked stacks onto new trunk.
    let output = repo.run(&["sync"])?;
    assert_success("sync", &output);

    for change in [&a, &b] {
        let rebased = repo.change_at(&change.change_id)?;
        let parent = repo.rev_commit_id(&format!("{}-", rebased.commit_id))?;
        assert_eq!(
            parent, advanced.commit_id,
            "stack {} should sit on the new trunk",
            change.change_id
        );
    }
    Ok(())
}

#[test]
fn sync_current_only_rebases_the_working_copy_stack() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-current-only")?;
    repo.init_main()?;

    let a = repo.create_change("a", "a title", "a body")?;
    let branch_a = branch_for("a-title", &a.change_id);
    repo.seed_pr_number(&branch_a, 11)?;
    assert_success("submit A", &repo.run(&["submit", "--yes"])?);

    repo.jj(&["new", "main"])?;
    let b = repo.create_change("b", "b title", "b body")?;
    let branch_b = branch_for("b-title", &b.change_id);
    repo.seed_pr_number(&branch_b, 12)?;
    assert_success("submit B", &repo.run(&["submit", "--yes"])?);

    // Remote trunk advances; @ is on stack B after the advance restore.
    let advanced = repo.advance_remote_trunk("remote work", &b.change_id)?;

    // `--current` syncs only the stack containing @ (B), leaving A behind.
    let output = repo.run(&["sync", "--current"])?;
    assert_success("sync --current", &output);

    let rebased_b = repo.change_at(&b.change_id)?;
    let parent_b = repo.rev_commit_id(&format!("{}-", rebased_b.commit_id))?;
    assert_eq!(parent_b, advanced.commit_id, "B should sit on new trunk");

    let untouched_a = repo.change_at(&a.change_id)?;
    let parent_a = repo.rev_commit_id(&format!("{}-", untouched_a.commit_id))?;
    assert_ne!(
        parent_a, advanced.commit_id,
        "A should NOT be rebased when --current is used"
    );
    Ok(())
}

#[test]
fn sync_unfreeze_rebases_frozen_dependency_onto_new_trunk() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-unfreeze-deps")?;
    repo.init_main()?;

    // A frozen upstream dependency (a teammate's imported PR) at the base.
    let dep = repo.create_change("dep", "dep title", "dep body")?;
    let dep_branch = branch_for("dep-title", &dep.change_id);
    repo.set_bookmark(&dep_branch, &dep.commit_id)?;
    repo.push_bookmark(&dep_branch)?;
    repo.seed_pr(11, &dep_branch, "main", "dep title", "dep body")?;
    repo.set_bookmark("forklift/frozen/pr-11", &dep.commit_id)?;

    // Your own change stacked on top of the frozen dependency.
    let mine = repo.create_change("mine", "mine title", "mine body")?;

    // An unrelated stack lands: remote trunk advances. @ is restored onto `mine`.
    let advanced = repo.advance_remote_trunk("unrelated landed", &mine.change_id)?;

    // `sync --unfreeze` adopts the dependency, then rebases the WHOLE chain
    // (dependency + your work) onto the new trunk.
    let output = repo.run(&["sync", "--unfreeze"])?;
    assert_success("sync --unfreeze", &output);

    assert!(
        !repo.bookmark_exists("forklift/frozen/pr-11")?,
        "frozen bookmark should be removed after --unfreeze"
    );

    // The former-frozen dependency now sits directly on the advanced trunk.
    let rebased_dep = repo.change_at(&dep.change_id)?;
    let dep_parent = repo.rev_commit_id(&format!("{}-", rebased_dep.commit_id))?;
    assert_eq!(
        dep_parent, advanced.commit_id,
        "the adopted dependency should be rebased onto the new trunk"
    );

    // And your work still sits on top of the dependency.
    let rebased_mine = repo.change_at(&mine.change_id)?;
    let mine_parent = repo.rev_commit_id(&format!("{}-", rebased_mine.commit_id))?;
    assert_eq!(
        mine_parent, rebased_dep.commit_id,
        "your change should stay stacked on the dependency"
    );
    Ok(())
}

#[test]
fn sync_is_best_effort_across_stacks_when_one_fails() -> anyhow::Result<()> {
    let repo = TestRepo::new("sync-best-effort")?;
    repo.init_main()?;

    // Healthy stack A: submitted, will fall behind trunk and need a rebase.
    let a = repo.create_change("a", "a title", "a body")?;
    let branch_a = branch_for("a-title", &a.change_id);
    repo.seed_pr_number(&branch_a, 9)?;
    assert_success("submit A", &repo.run(&["submit", "--yes"])?);

    // Broken stack B: child rooted on an un-merged, un-frozen open PR, which
    // sync refuses to rebase past.
    repo.jj(&["new", "main"])?;
    let parent = repo.create_change("parent", "parent title", "parent body")?;
    let parent_branch = branch_for("parent-title", &parent.change_id);
    repo.set_bookmark(&parent_branch, &parent.commit_id)?;
    repo.push_bookmark(&parent_branch)?;
    repo.seed_pr(11, &parent_branch, "main", "parent title", "parent body")?;
    repo.jj(&["bookmark", "untrack", &format!("{parent_branch}@origin")])?;
    let child = repo.create_change("child", "child title", "child body")?;

    let advanced = repo.advance_remote_trunk("unrelated landed", &child.change_id)?;

    let output = repo.run(&["sync"])?;
    // The broken stack makes the overall run exit non-zero...
    assert!(
        !output.status.success(),
        "sync should report failure when a tracked stack fails"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("failed to sync") && stderr.contains("tracked stack(s) failed to sync"),
        "stderr should surface the per-stack failure and tally:\n{stderr}"
    );
    // The underlying cause is printed indented under the warning, not swallowed.
    assert!(
        stderr.contains("un-merged commit") && stderr.contains("not a frozen dependency"),
        "the failing stack's detailed reason should be shown under the warning:\n{stderr}"
    );

    // ...but the healthy stack A was still rebased onto the new trunk.
    let rebased_a = repo.change_at(&a.change_id)?;
    let parent_a = repo.rev_commit_id(&format!("{}-", rebased_a.commit_id))?;
    assert_eq!(
        parent_a, advanced.commit_id,
        "healthy stack A should be rebased despite the other stack failing"
    );

    // The broken stack B was left untouched (not rebased onto trunk).
    let child_after = repo.change_at(&child.change_id)?;
    let parent_child = repo.rev_commit_id(&format!("{}-", child_after.commit_id))?;
    assert_ne!(
        parent_child, advanced.commit_id,
        "broken stack B must not be rebased onto trunk"
    );
    Ok(())
}
