// End-to-end `get` tests driving the real `forklift` binary against a real colocated jj repo.

mod common;

use common::*;
use serde_json::json;

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
fn get_warns_when_local_trunk_is_behind_remote() -> anyhow::Result<()> {
    let repo = TestRepo::new("get-stale-trunk")?;
    let main = repo.init_main()?;
    let imported = repo.create_change("imported", "imported title", "imported body")?;
    let branch = branch_for("imported-title", &imported.change_id);
    repo.set_bookmark(&branch, &imported.commit_id)?;
    repo.push_bookmark(&branch)?;
    repo.seed_pr(11, &branch, "main", "imported title", "imported body")?;
    let advanced = repo.advance_remote_trunk("remote work", &imported.change_id)?;
    repo.jj(&[
        "bookmark",
        "set",
        "--allow-backwards",
        "main",
        "-r",
        &main.commit_id,
    ])?;

    let output = repo.run(&["get", "11", "--no-edit"])?;
    assert_success("get 11 --no-edit", &output);
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(
            "local trunk `main` is behind `main@origin`; run `forklift sync` before editing or submitting this stack"
        ),
        "stderr:\n{stderr}"
    );
    assert_eq!(
        repo.bookmark_target("main")?,
        main.commit_id,
        "get should warn but leave local trunk unmoved"
    );
    assert_eq!(repo.git_remote_branch_target("main")?, advanced.commit_id);
    Ok(())
}

#[test]
fn get_resolves_short_local_change_id_prefix() -> anyhow::Result<()> {
    let repo = TestRepo::new("get-short-prefix")?;
    repo.init_main()?;
    let imported = repo.create_change("imported", "imported title", "imported body")?;
    let branch = branch_for("imported-title", &imported.change_id);
    repo.set_bookmark(&branch, "@")?;
    repo.push_bookmark(&branch)?;
    repo.seed_pr(11, &branch, "main", "imported title", "imported body")?;

    // A 4-char prefix is shorter than the 8 chars encoded in the branch name,
    // so this can only resolve via jj's native prefix expansion of the
    // locally checked-out change.
    let short_prefix: String = imported.change_id.chars().take(4).collect();
    let output = repo.run(&["get", &short_prefix])?;
    assert_success(&format!("get {short_prefix}"), &output);

    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-11")?,
        imported.commit_id,
        "short change-id prefix should resolve to the imported PR"
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
            "skip editing: run `jj new forklift/frozen/pr-12` to start editing above the targeted PR"
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
fn get_lands_working_copy_on_targeted_mid_stack_pr() -> anyhow::Result<()> {
    let repo = TestRepo::new("get-mid-stack")?;
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
    // The stack comment lives on the targeted (bottom) PR so `get 11` can resolve
    // the whole stack from it.
    repo.seed_comment(
        11,
        201,
        &common::stack_comment_body(&rows, &stack[0].change_id),
    )?;

    let output = repo.run(&["get", "11"])?;
    assert_success("get 11", &output);

    // The entire stack is fetched and frozen...
    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-11")?,
        stack[0].commit_id
    );
    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-12")?,
        stack[1].commit_id
    );
    // ...but the working copy lands on the targeted bottom PR, not the stack tip.
    assert_eq!(
        repo.rev_commit_id("@-")?,
        stack[0].commit_id,
        "get should new @ on top of the targeted PR's frozen rev, not the tip"
    );
    Ok(())
}

#[test]
fn get_force_overwrites_diverged_frozen_bookmark() -> anyhow::Result<()> {
    let repo = TestRepo::new("get-force-diverged")?;
    repo.init_main()?;
    let imported = repo.create_change("imported", "imported title", "imported body")?;
    let branch = branch_for("imported-title", &imported.change_id);
    repo.set_bookmark(&branch, "@")?;
    repo.push_bookmark(&branch)?;
    repo.seed_pr(11, &branch, "main", "imported title", "imported body")?;

    // First get freezes pr-11 at the original head.
    let output = repo.run(&["get", "11", "--no-edit"])?;
    assert_success("get 11", &output);
    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-11")?,
        imported.commit_id
    );

    // Rewrite the PR upstream: point its branch at a sibling of the frozen head
    // (so the frozen commit is no longer an ancestor) and force-push it.
    repo.jj(&["new", "main"])?;
    repo.write_file("rewritten.txt", "rewritten\n")?;
    repo.jj(&["describe", "-m", "rewritten title"])?;
    let rewritten = repo.rev_commit_id("@")?;
    repo.jj(&["bookmark", "set", "--allow-backwards", &branch, "-r", "@"])?;
    repo.push_bookmark(&branch)?;

    // Without --force, a non-interactive run aborts rather than move the snapshot.
    let blocked = repo.run(&["get", "11", "--no-edit"])?;
    assert!(
        !blocked.status.success(),
        "diverged frozen bookmark should block get without --force\nstderr:\n{}",
        stderr_of(&blocked)
    );
    let stderr = stderr_of(&blocked);
    assert!(
        stderr.contains("was rewritten upstream"),
        "stderr should explain the divergence:\n{stderr}"
    );
    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-11")?,
        imported.commit_id,
        "the blocked run must leave the frozen bookmark untouched"
    );

    // --force overwrites the frozen bookmark with the fetched head.
    let forced = repo.run(&["get", "11", "--no-edit", "--force"])?;
    assert_success("get 11 --force", &forced);
    assert_eq!(
        repo.bookmark_target("forklift/frozen/pr-11")?,
        rewritten,
        "--force should move the frozen bookmark to the rewritten head"
    );
    Ok(())
}
