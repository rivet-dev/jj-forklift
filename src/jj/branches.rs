use super::super::*;
use super::*;

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn slugify_title(title: &str) -> String {
    title
        .trim()
        .nfd()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .scan(None, |last_char, ch| {
            if ch == '-' && last_char == &Some('-') {
                Some(None)
            } else {
                *last_char = Some(ch);
                Some(Some(ch))
            }
        })
        .flatten()
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

#[tracing::instrument(skip_all, fields(change = %change.change_id))]
pub(crate) fn deterministic_head_branch(
    config: &AppConfig,
    change: &ResolvedChange,
    used_branches: &HashSet<String>,
) -> String {
    let slug = match slugify_title(&change.title) {
        slug if slug.is_empty() => "change".to_owned(),
        slug => slug,
    };
    let change_id_prefix = change_id_branch_prefix(&change.change_id);
    let base = format!("{}/{}-{}", config.branch_prefix, slug, change_id_prefix);
    find_unused_head_branch(&base, used_branches)
}

#[tracing::instrument(level = "trace", skip_all)]
pub(crate) fn change_id_branch_prefix(change_id: &str) -> &str {
    change_id
        .char_indices()
        .nth(8)
        .map_or(change_id, |(index, _)| &change_id[..index])
}

#[tracing::instrument(level = "trace", skip_all, fields(base = base))]
pub(crate) fn find_unused_head_branch(base: &str, used_branches: &HashSet<String>) -> String {
    if !used_branches.contains(base) {
        return base.to_owned();
    }

    for index in 1.. {
        let candidate = format!("{base}-{index}");
        if !used_branches.contains(&candidate) {
            return candidate;
        }
    }

    unreachable!("unbounded branch suffix search should find a candidate")
}

pub(crate) fn fetch_get_branches(
    runner: &impl CommandRunner,
    config: &AppConfig,
    prs: &[GhPr],
    diagnostics: Diagnostics,
) -> Result<()> {
    let mut args = vec![
        "git".to_owned(),
        "fetch".to_owned(),
        "--remote".to_owned(),
        config.remote.clone(),
    ];
    for pr in prs {
        args.push("--branch".to_owned());
        args.push(pr.head_ref_name.clone());
    }
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    if diagnostics.dry_run {
        diagnostics.plan_line(&format!("- {}", display_command("jj", &arg_refs)));
        return Ok(());
    }

    diagnostics.command("jj", &arg_refs);
    let output = runner.run("jj", &arg_refs)?;
    if !output.success {
        let branches = prs
            .iter()
            .map(|pr| format!("#{} `{}`", pr.number, pr.head_ref_name))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "failed-command=`{}` error=failed to fetch PR head branches {branches}; an open PR branch may have been deleted or renamed: {}",
            display_command("jj", &arg_refs),
            output.stderr.trim()
        );
    }
    Ok(())
}
