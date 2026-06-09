use super::super::*;

pub(crate) fn resolve_current_jj_repo_dir(runner: &impl CommandRunner) -> Result<PathBuf> {
    let workspace_root =
        run_required(runner, "jj", &["root"]).context("resolve jj workspace root")?;
    resolve_jj_repo_dir(Path::new(&workspace_root))
}

pub(crate) fn resolve_jj_repo_dir(workspace_root: &Path) -> Result<PathBuf> {
    let jj_dir = workspace_root.join(".jj");
    let repo_entry = jj_dir.join("repo");
    let metadata = fs::metadata(&repo_entry)
        .with_context(|| format!("read jj repo entry {}", repo_entry.display()))?;

    let repo_dir = if metadata.is_dir() {
        repo_entry
    } else {
        let pointer = fs::read_to_string(&repo_entry)
            .with_context(|| format!("read jj repo pointer {}", repo_entry.display()))?;
        let pointer = pointer.trim();
        if pointer.is_empty() {
            bail!("jj repo pointer {} is empty", repo_entry.display());
        }

        let target = PathBuf::from(pointer);
        if target.is_absolute() {
            target
        } else {
            jj_dir.join(target)
        }
    };

    fs::canonicalize(&repo_dir)
        .with_context(|| format!("resolve jj repo directory {}", repo_dir.display()))
}
