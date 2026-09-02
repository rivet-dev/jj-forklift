pub mod app;

use serde::Deserialize;

/// Revset that scopes a view (e.g. `forklift ui`) to every stack off trunk, the
/// `jjui` analogue of Graphite's view of all stacks plus the current one.
///
/// `mutable()` is what makes sibling stacks visible: it covers every un-merged
/// commit regardless of whether submit has bookmarked it, so a stack branching
/// off the middle of the current one (or a bookmark-less scratch stack) still
/// shows up instead of being hidden because the working copy sits elsewhere.
/// The rest is belt-and-braces for commits `mutable()` cannot reach: trunk
/// itself, the working copy and its descendants (`@::`), and any local
/// `<prefix>/*` submit head or `forklift/frozen/*` dependency bookmark.
pub fn tracked_stacks_revset(branch_prefix: &str) -> String {
    let prefix = branch_prefix.trim_end_matches('/');
    format!(
        "trunk() | @:: | mutable() | trunk()..(@ | bookmarks(glob:'{prefix}/*') | bookmarks(glob:'forklift/frozen/*'))"
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
