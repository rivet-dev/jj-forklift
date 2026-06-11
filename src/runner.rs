use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CommandOutput {
    pub(super) success: bool,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

pub(super) trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput>;

    fn run_interactive(&self, program: &str, args: &[&str]) -> Result<()> {
        let output = self.run(program, args)?;
        print!("{}", output.stdout);
        eprint!("{}", output.stderr);
        if !output.success {
            bail!("`{}` failed", display_command(program, args));
        }
        Ok(())
    }

    /// Like `run`, but executes with `cwd` as the child process's working
    /// directory. Default impl ignores `cwd` and delegates to `run`, which is
    /// fine for test fakes that don't care about which directory they're
    /// invoked from.
    fn run_in_dir(&self, program: &str, args: &[&str], _cwd: &Path) -> Result<CommandOutput> {
        self.run(program, args)
    }
}

pub(super) struct SystemRunner;

impl CommandRunner for SystemRunner {
    #[tracing::instrument(skip_all)]
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("run `{}`", display_command(program, args)))?;

        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    #[tracing::instrument(skip_all)]
    fn run_interactive(&self, program: &str, args: &[&str]) -> Result<()> {
        let status = Command::new(program)
            .args(args)
            .status()
            .with_context(|| format!("run `{}`", display_command(program, args)))?;
        if !status.success() {
            bail!(
                "`{}` failed with status {status}",
                display_command(program, args)
            );
        }
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    fn run_in_dir(&self, program: &str, args: &[&str], cwd: &Path) -> Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("run `{}`", display_command(program, args)))?;

        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[tracing::instrument(skip_all)]
pub(super) fn run_required(
    runner: &impl CommandRunner,
    program: &str,
    args: &[&str],
) -> Result<String> {
    let output = runner.run(program, args)?;
    if !output.success {
        bail!(
            "`{}` failed: {}",
            display_command(program, args),
            output.stderr.trim()
        );
    }

    let value = output.stdout.trim();
    if value.is_empty() {
        bail!("`{}` returned empty output", display_command(program, args));
    }

    Ok(value.to_owned())
}

/// Directory of the colocated git repo backing the current jj workspace.
///
/// When invoked from a secondary jj workspace the cwd has no `.git` — only the
/// primary (colocated) workspace does. The resolved `.jj/repo` path is
/// `<primary>/.jj/repo`, so the primary workspace dir is two `parent()` calls
/// up from there.
#[tracing::instrument(level = "trace", skip_all)]
pub(super) fn git_workspace_root(runner: &impl CommandRunner) -> Result<PathBuf> {
    let repo_dir = resolve_current_jj_repo_dir(runner)?;
    repo_dir
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "derive backing workspace dir from jj repo dir {}",
                repo_dir.display()
            )
        })
}

/// Run `git` against the backing colocated workspace, regardless of which jj
/// workspace the user invoked us from. Secondary jj workspaces are not git
/// worktrees, so there is exactly one `.git` to talk to — the primary's.
pub(super) fn git_run(runner: &impl CommandRunner, args: &[&str]) -> Result<CommandOutput> {
    let root = git_workspace_root(runner)?;
    runner.run_in_dir("git", args, &root)
}

/// `run_required` for git, targeting the backing colocated workspace.
pub(super) fn git_run_required(runner: &impl CommandRunner, args: &[&str]) -> Result<String> {
    let output = git_run(runner, args)?;
    if !output.success {
        bail!(
            "`{}` failed: {}",
            display_command("git", args),
            output.stderr.trim()
        );
    }
    let value = output.stdout.trim();
    if value.is_empty() {
        bail!("`{}` returned empty output", display_command("git", args));
    }
    Ok(value.to_owned())
}

/// Run `gh` against the backing colocated workspace. `gh repo view` and other
/// commands without an explicit `--repo` auto-detect the repo from the git
/// remote in the cwd; in a secondary jj workspace there is no `.git`, so we
/// must point gh at the primary.
pub(super) fn gh_run(runner: &impl CommandRunner, args: &[&str]) -> Result<CommandOutput> {
    let root = git_workspace_root(runner)?;
    runner.run_in_dir("gh", args, &root)
}

/// `run_required` for gh, targeting the backing colocated workspace.
pub(super) fn gh_run_required(runner: &impl CommandRunner, args: &[&str]) -> Result<String> {
    let output = gh_run(runner, args)?;
    if !output.success {
        bail!(
            "`{}` failed: {}",
            display_command("gh", args),
            output.stderr.trim()
        );
    }
    let value = output.stdout.trim();
    if value.is_empty() {
        bail!("`{}` returned empty output", display_command("gh", args));
    }
    Ok(value.to_owned())
}

pub(super) fn display_command(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}
