pub mod app;

use serde::Deserialize;

/// Revset that scopes a view (e.g. `forklift ui`) to the stacks forklift tracks:
/// trunk plus every commit between trunk and the working copy, any local
/// `<prefix>/*` head bookmark created by submit, and any `forklift/frozen/*`
/// dependency bookmark. This is the `jjui` analogue of Graphite's
/// tracked-branches view.
pub fn tracked_stacks_revset(branch_prefix: &str) -> String {
    let prefix = branch_prefix.trim_end_matches('/');
    format!(
        "trunk() | trunk()..(@ | bookmarks(glob:'{prefix}/*') | bookmarks(glob:'forklift/frozen/*'))"
    )
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
