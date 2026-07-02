use super::super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitHubContext {
    pub(crate) repo: String,
    pub(crate) username: String,
}

impl GitHubContext {
    #[tracing::instrument(skip_all)]
    pub(crate) async fn resolve(runner: &impl CommandRunner) -> Result<Self> {
        let repo = gh_run_required(
            runner,
            &[
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "--jq",
                ".nameWithOwner",
            ],
        )
        .await
        .context("resolve GitHub repository with gh")?;
        let username = gh_run_required(runner, &["api", "user", "--jq", ".login"])
            .await
            .context("resolve GitHub username with gh")?;

        Ok(Self { repo, username })
    }
}
