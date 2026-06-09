// End-to-end `unfreeze` tests driving the real `forklift` binary against a real colocated jj repo.

mod common;

use common::*;

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
