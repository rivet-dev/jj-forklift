// Tests for `forklift ui` (the `jjui` wrapper) and the `jj` passthrough.

mod common;

use common::*;

#[test]
fn tracked_stacks_revset_covers_working_copy_submitted_and_frozen() {
    let revset = forklift::tracked_stacks_revset("stack");
    assert_eq!(
        revset,
        "trunk() | trunk()..(@ | bookmarks(glob:'stack/*') | bookmarks(glob:'forklift/frozen/*'))"
    );
}

#[test]
fn tracked_stacks_revset_trims_trailing_slash_on_prefix() {
    assert_eq!(
        forklift::tracked_stacks_revset("feature/"),
        "trunk() | trunk()..(@ | bookmarks(glob:'feature/*') | bookmarks(glob:'forklift/frozen/*'))"
    );
}

#[test]
fn ui_dry_run_uses_tracked_stacks_revset_by_default() -> anyhow::Result<()> {
    let repo = TestRepo::new("ui-dry-run-default")?;
    repo.init_main()?;

    let output = repo.run(&["ui", "--dry-run"])?;
    assert!(
        output.status.success(),
        "ui --dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        rendered.contains(
            "jjui -r trunk() | trunk()..(@ | bookmarks(glob:'stack/*') | bookmarks(glob:'forklift/frozen/*'))"
        ),
        "expected tracked-stacks revset in dry-run output, got: {rendered}"
    );
    Ok(())
}

#[test]
fn ui_dry_run_all_defers_to_jjui_default_revset() -> anyhow::Result<()> {
    let repo = TestRepo::new("ui-dry-run-all")?;
    repo.init_main()?;

    let output = repo.run(&["ui", "--all", "--dry-run"])?;
    assert!(output.status.success());
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        rendered.contains("would run `jjui`") && !rendered.contains("jjui -r"),
        "expected bare `jjui` invocation for --all, got: {rendered}"
    );
    Ok(())
}

#[test]
fn jj_passthrough_matches_direct_jj_invocation() -> anyhow::Result<()> {
    let repo = TestRepo::new("jj-passthrough")?;
    repo.init_main()?;

    // An unknown subcommand is forwarded verbatim to `jj`, so `forklift bookmark
    // list` must produce the same output as `jj bookmark list`.
    let via_forklift = repo.run(&["bookmark", "list"])?;
    assert!(
        via_forklift.status.success(),
        "passthrough failed: {}",
        String::from_utf8_lossy(&via_forklift.stderr)
    );
    let direct = repo.jj_stdout(&["bookmark", "list"])?;
    assert_eq!(String::from_utf8_lossy(&via_forklift.stdout), direct);
    Ok(())
}

#[test]
fn jj_passthrough_propagates_failure_exit_code() -> anyhow::Result<()> {
    let repo = TestRepo::new("jj-passthrough-failure")?;
    repo.init_main()?;

    // A bogus jj subcommand should fail through forklift just like jj would.
    let output = repo.run(&["definitely-not-a-jj-command"])?;
    assert!(
        !output.status.success(),
        "expected non-zero exit for invalid passthrough"
    );
    Ok(())
}
