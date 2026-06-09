// End-to-end `status` tests driving the real `forklift` binary against a real colocated jj repo.

mod common;

use common::*;

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
