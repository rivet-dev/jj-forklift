// End-to-end `pr` tests driving the real `forklift` binary against a real colocated jj repo.

mod common;

use common::*;

#[test]
fn pr_error_is_rendered_as_a_human_diagnostic() -> anyhow::Result<()> {
    let repo = TestRepo::new("pr-error")?;
    repo.init_main()?;
    repo.create_change("change", "change title", "change body")?;

    let output = repo.run(&["pr"])?;
    assert!(!output.status.success(), "pr without a PR should fail");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("error: current revision is not submitted"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("reason:\n  current change `"),
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("has no open PR yet"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("resolution:\n  run `forklift submit --yes`, then `forklift pr`"),
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("details:"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("phase:     resolve-pr"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("phase=resolve-pr object=@"),
        "stderr:\n{stderr}"
    );
    Ok(())
}

#[test]
fn pr_on_trunk_explains_there_is_no_current_pr() -> anyhow::Result<()> {
    let repo = TestRepo::new("pr-on-trunk")?;
    repo.init_main()?;

    let output = repo.run(&["pr"])?;
    assert!(!output.status.success(), "pr on trunk should fail");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("error: no current PR"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("reason:\n  current checkout is on trunk `main`"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("resolution:\n  check out a stack change or pass a PR target"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("forklift submit --dry-run"),
        "trunk diagnostic should not point to submit\nstderr:\n{stderr}"
    );
    Ok(())
}
