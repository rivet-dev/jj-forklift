// End-to-end `repair` tests driving the real `forklift` binary against a real colocated jj repo.

mod common;

use common::*;

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
