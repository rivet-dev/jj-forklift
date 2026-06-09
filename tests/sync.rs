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
        stderr.contains("Finished sync — 0 roots rebased"),
        "stderr:\n{stderr}"
    );
    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-11")?,
        imported.commit_id
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
