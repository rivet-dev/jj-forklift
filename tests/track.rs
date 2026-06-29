// End-to-end `track` tests: adopting an existing branch + open PR into
// forklift's tracked set, driving the real `forklift` binary against a real
// colocated jj repo and the shared fake `gh`.

mod common;

use common::*;
use serde_json::json;

#[test]
fn track_binds_existing_branch_and_pr_into_cache() -> anyhow::Result<()> {
    let repo = TestRepo::new("track-binds")?;
    repo.init_main()?;
    let change = repo.create_change("feature", "feature title", "feature body")?;

    // A branch + PR created outside forklift: arbitrary name, pushed to the
    // remote, open PR #7, but no forklift cache row yet.
    let branch = "my-feature";
    repo.set_bookmark(branch, &change.commit_id)?;
    repo.push_bookmark(branch)?;
    repo.seed_pr(7, branch, "main", "feature title", "feature body")?;
    repo.delete_cache()?;

    let output = repo.run(&["track", &7.to_string()])?;
    assert_success("track", &output);

    // forklift now has a cache row binding the change to PR #7 on its real head
    // branch, so a later submit updates that PR instead of opening a new one.
    let entry = repo.cache_entry(&change.change_id)?;
    assert_eq!(entry["pr_number"], json!(7));
    assert_eq!(entry["head_branch"], json!(branch));
    assert_eq!(entry["base_branch"], json!("main"));

    // The local bookmark sits on the change and tracks the remote.
    assert_eq!(repo.bookmark_target(branch)?, change.commit_id);
    assert_eq!(repo.tracked_remote_target(branch)?, change.commit_id);
    Ok(())
}

#[test]
fn track_refuses_merged_pr() -> anyhow::Result<()> {
    let repo = TestRepo::new("track-merged")?;
    repo.init_main()?;
    let change = repo.create_change("feature", "feature title", "feature body")?;
    let branch = "merged-feature";
    repo.set_bookmark(branch, &change.commit_id)?;
    repo.push_bookmark(branch)?;
    repo.seed_pr(8, branch, "main", "feature title", "feature body")?;
    repo.set_pr_merged(8, true)?;
    repo.delete_cache()?;

    let output = repo.run(&["track", &8.to_string()])?;
    assert!(
        !output.status.success(),
        "track should refuse a merged PR; stderr:\n{}",
        stderr_of(&output)
    );
    assert!(
        !repo.cache_path().exists(),
        "no cache row should be written for a refused track"
    );
    Ok(())
}

#[test]
fn track_dry_run_writes_no_cache() -> anyhow::Result<()> {
    let repo = TestRepo::new("track-dry-run")?;
    repo.init_main()?;
    let change = repo.create_change("feature", "feature title", "feature body")?;
    let branch = "dry-feature";
    repo.set_bookmark(branch, &change.commit_id)?;
    repo.push_bookmark(branch)?;
    repo.seed_pr(9, branch, "main", "feature title", "feature body")?;
    repo.delete_cache()?;

    let output = repo.run(&["--dry-run", "track", &9.to_string()])?;
    assert_success("track --dry-run", &output);
    assert!(
        !repo.cache_path().exists(),
        "dry-run track must not persist a cache row"
    );
    Ok(())
}
