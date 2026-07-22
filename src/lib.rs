pub mod app;

use serde::Deserialize;

/// Revset that scopes a view (e.g. `forklift ui`) to the stacks forklift tracks:
/// trunk plus every commit between trunk and the working copy, any local
/// `<prefix>/*` head bookmark created by submit, any `forklift/frozen/*`
/// dependency bookmark, and any commit stacked on top of the working copy
/// (`@::`). This is the `jjui` analogue of Graphite's tracked-branches view.
pub fn tracked_stacks_revset(branch_prefix: &str) -> String {
    let prefix = branch_prefix.trim_end_matches('/');
    format!(
        "trunk() | @:: | trunk()..(@ | bookmarks(glob:'{prefix}/*') | bookmarks(glob:'forklift/frozen/*'))"
    )
}

pub fn effective_status_checks(checks: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    let mut effective = Vec::<&serde_json::Value>::new();
    for check in checks {
        let identity = check_identity(check);
        let timestamp = check_timestamp(check);
        if let Some(existing_index) = effective
            .iter()
            .position(|existing| check_identity(existing) == identity)
        {
            let existing_timestamp = check_timestamp(effective[existing_index]);
            let replace = match (timestamp, existing_timestamp) {
                (Some(new), Some(old)) => new >= old,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => true,
            };
            if replace {
                effective[existing_index] = check;
            }
        } else {
            effective.push(check);
        }
    }
    effective
}

fn check_identity(check: &serde_json::Value) -> String {
    let workflow = check
        .get("workflowName")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let name = check
        .get("name")
        .or_else(|| check.get("context"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown>");
    format!("{workflow}\t{name}")
}

fn check_timestamp(check: &serde_json::Value) -> Option<&str> {
    check
        .get("startedAt")
        .and_then(serde_json::Value::as_str)
        .or_else(|| check.get("completedAt").and_then(serde_json::Value::as_str))
}

pub fn empty_string_to_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }))
}
