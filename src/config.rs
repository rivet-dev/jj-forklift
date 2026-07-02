use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AppConfig {
    pub(super) remote: String,
    pub(super) trunk: String,
    pub(super) require_approval: bool,
    pub(super) branch_prefix: String,
}

impl AppConfig {
    #[tracing::instrument(skip_all)]
    pub(super) async fn resolve(runner: &impl CommandRunner) -> Result<Self> {
        let remote = resolve_string_config(runner, "remote", DEFAULT_REMOTE).await;
        let trunk = resolve_string_config(runner, "trunk", DEFAULT_TRUNK).await;
        let branch_prefix =
            resolve_string_config(runner, "branch-prefix", DEFAULT_BRANCH_PREFIX).await;
        Ok(Self {
            remote: validate_ref_component("remote", remote)?,
            trunk: validate_ref_component("trunk", trunk)?,
            require_approval: resolve_bool_config(
                runner,
                "require-approval",
                DEFAULT_REQUIRE_APPROVAL,
            )
            .await?,
            branch_prefix: validate_ref_component("branch-prefix", branch_prefix)?,
        })
    }
}

#[tracing::instrument(skip_all)]
pub(super) async fn resolve_string_config(
    runner: &impl CommandRunner,
    name: &str,
    default: &str,
) -> String {
    if let Some(value) = config_value(runner, "jj", name).await {
        return value;
    }
    if let Some(value) = config_value(runner, "git", name).await {
        return value;
    }
    default.to_owned()
}

/// Validate a configured ref component (remote/trunk/branch-prefix) before it is
/// ever passed to `jj`/`git` as a positional argument. These values come from
/// `jj config`/`git config`, which a cloned or shared repo can poison. Without a
/// shell there is no metacharacter injection, but a value beginning with `-`
/// would be parsed as a flag by the downstream tool (e.g. `--insert-after` to
/// `jj rebase`). Reject anything that is not a plain ref name so it can only ever
/// be interpreted as data.
#[tracing::instrument(skip_all)]
pub(super) fn validate_ref_component(name: &str, value: String) -> Result<String> {
    let valid = !value.is_empty()
        && !value.starts_with('-')
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'));
    if !valid {
        bail!(CliError::new(format!(
            "invalid {CONFIG_PREFIX}.{name} value `{value}`"
        ))
        .resolution(
            "use a plain ref name (letters, digits, `/`, `-`, `_`, `.`; no leading `-`, whitespace, `:`, glob, or `..`)"
        ));
    }
    Ok(value)
}

#[tracing::instrument(skip_all)]
pub(super) async fn resolve_bool_config(
    runner: &impl CommandRunner,
    name: &str,
    default: bool,
) -> Result<bool> {
    let value = match config_value(runner, "jj", name).await {
        Some(value) => Some(value),
        None => config_value(runner, "git", name).await,
    };
    match value {
        Some(value) => parse_bool_config(name, &value),
        None => Ok(default),
    }
}

#[tracing::instrument(skip_all)]
pub(super) async fn config_value(
    runner: &impl CommandRunner,
    program: &str,
    name: &str,
) -> Option<String> {
    let key = format!("{CONFIG_PREFIX}.{name}");
    let args = match program {
        "jj" => vec!["config", "get", key.as_str()],
        "git" => vec!["config", "--get", key.as_str()],
        _ => return None,
    };

    let output = match program {
        "git" => git_run(runner, &args).await.ok()?,
        _ => runner.run(program, &args).await.ok()?,
    };
    if !output.success {
        return None;
    }

    let value = output.stdout.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

#[tracing::instrument(skip_all)]
pub(super) fn parse_bool_config(name: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => bail!("invalid boolean value for {CONFIG_PREFIX}.{name}: {value}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StartupConfigAction {
    pub(super) key: &'static str,
    pub(super) value: String,
}

#[tracing::instrument(skip_all)]
pub(super) async fn ensure_jj_repo(runner: &impl CommandRunner, cwd: &str) -> Result<()> {
    let output = runner.run("jj", &["root"]).await?;
    if output.success {
        return Ok(());
    }

    bail!(
        CliError::new(format!("not inside a jj repository (cwd={cwd})")).resolution(
            "run from within your repo, or initialize one with `jj git init --colocate`"
        )
    );
}

#[tracing::instrument(skip_all)]
pub(super) async fn ensure_startup_config(
    runner: &impl CommandRunner,
    diagnostics: Diagnostics,
) -> Result<()> {
    let frozen = jj_config_optional(runner, JJ_CONFIG_FROZEN_ALIAS_KEY).await?;
    let immutable = jj_config_required(runner, JJ_CONFIG_IMMUTABLE_ALIAS_KEY).await?;
    let base = jj_config_optional(runner, JJ_CONFIG_BASE_IMMUTABLE_ALIAS_KEY).await?;
    let actions = plan_startup_config(frozen.as_deref(), &immutable, base.as_deref())?;

    if actions.is_empty() {
        return Ok(());
    }

    if diagnostics.dry_run {
        for action in actions {
            diagnostics.plan_line(&format!(
                "- set repo jj config {} = {}",
                action.key, action.value
            ));
        }
        return Ok(());
    }

    for action in actions {
        set_jj_repo_config(runner, action.key, &action.value, diagnostics).await?;
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
pub(super) fn plan_startup_config(
    frozen: Option<&str>,
    immutable: &str,
    base: Option<&str>,
) -> Result<Vec<StartupConfigAction>> {
    let mut actions = Vec::new();

    match frozen {
        Some(value) if value == JJ_FROZEN_ALIAS_VALUE => {}
        Some(value) => bail!(CliError::new(format!(
            "repo config `{JJ_CONFIG_FROZEN_ALIAS_KEY}` is `{value}`, expected `{JJ_FROZEN_ALIAS_VALUE}`"
        ))),
        None => actions.push(StartupConfigAction {
            key: JJ_CONFIG_FROZEN_ALIAS_KEY,
            value: JJ_FROZEN_ALIAS_VALUE.to_owned(),
        }),
    }

    match immutable {
        JJ_DEFAULT_IMMUTABLE_ALIAS_VALUE => {
            actions.push(StartupConfigAction {
                key: JJ_CONFIG_IMMUTABLE_ALIAS_KEY,
                value: JJ_REQUIRED_IMMUTABLE_ALIAS_VALUE.to_owned(),
            });
        }
        JJ_REQUIRED_IMMUTABLE_ALIAS_VALUE => {}
        JJ_WRAPPED_IMMUTABLE_ALIAS_VALUE => {
            if base.is_none() {
                bail!(CliError::new(format!(
                    "repo config `{JJ_CONFIG_IMMUTABLE_ALIAS_KEY}` wraps `{JJ_CONFIG_BASE_IMMUTABLE_ALIAS_KEY}`, but the base alias is missing"
                )));
            }
        }
        value if value.contains("forklift_frozen_heads()") => {}
        custom => match base {
            None => {
                actions.push(StartupConfigAction {
                    key: JJ_CONFIG_BASE_IMMUTABLE_ALIAS_KEY,
                    value: custom.to_owned(),
                });
                actions.push(StartupConfigAction {
                    key: JJ_CONFIG_IMMUTABLE_ALIAS_KEY,
                    value: JJ_WRAPPED_IMMUTABLE_ALIAS_VALUE.to_owned(),
                });
            }
            Some(existing_base) => bail!(CliError::new(format!(
                "repo config `{JJ_CONFIG_IMMUTABLE_ALIAS_KEY}` is custom (`{custom}`), but `{JJ_CONFIG_BASE_IMMUTABLE_ALIAS_KEY}` already exists as `{existing_base}`"
            ))),
        },
    }

    Ok(actions)
}

#[tracing::instrument(skip_all)]
pub(super) async fn jj_config_required(runner: &impl CommandRunner, key: &str) -> Result<String> {
    let args = ["config", "get", key];
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    Ok(output.stdout.trim().to_owned())
}

#[tracing::instrument(skip_all)]
pub(super) async fn jj_config_optional(
    runner: &impl CommandRunner,
    key: &str,
) -> Result<Option<String>> {
    let args = ["config", "get", key];
    let output = runner.run("jj", &args).await?;
    if !output.success {
        return Ok(None);
    }

    let value = output.stdout.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.to_owned()))
    }
}

#[tracing::instrument(skip_all)]
pub(super) async fn set_jj_repo_config(
    runner: &impl CommandRunner,
    key: &str,
    value: &str,
    diagnostics: Diagnostics,
) -> Result<()> {
    let toml_value = serde_json::to_string(value).context("quote jj config value")?;
    let args = ["config", "set", "--repo", key, toml_value.as_str()];
    diagnostics.command("jj", &args);
    let output = runner.run("jj", &args).await?;
    if !output.success {
        bail!(
            "failed-command=`{}` error={} safe-next-command=`forklift submit --dry-run`",
            display_command("jj", &args),
            output.stderr.trim()
        );
    }

    Ok(())
}
