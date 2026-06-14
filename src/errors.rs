use super::*;

#[tracing::instrument(skip_all)]
pub(super) fn phase_error(
    phase: &str,
    object: impl Display,
    error: anyhow::Error,
) -> anyhow::Error {
    let object = object.to_string();
    let inner = diagnostic_from_error(&error);
    if phase == "resolve-pr"
        && matches!(
            inner.message.as_str(),
            "no current PR" | "current revision is not submitted"
        )
    {
        return anyhow::Error::new(inner.detail("phase", phase).detail("object", object));
    }
    let mut cli_error = CliError::new(phase_summary(phase, &object))
        .reason(reason_from_error(&error, &inner))
        .resolution(inner.resolution.unwrap_or_else(|| {
            "run `forklift submit --dry-run` to preview the stack state".to_owned()
        }))
        .detail("phase", phase)
        .detail("object", object);
    cli_error.details.extend(inner.details);
    anyhow::Error::new(cli_error)
}

pub(super) fn reason_from_error(error: &anyhow::Error, diagnostic: &CliError) -> String {
    if error.chain().count() > 1 {
        return format!("{error:#}");
    }
    diagnostic
        .reason
        .clone()
        .unwrap_or_else(|| diagnostic.message.clone())
}

#[derive(Debug, Clone)]
pub(super) struct CliError {
    pub(super) message: String,
    pub(super) reason: Option<String>,
    pub(super) resolution: Option<String>,
    pub(super) details: Vec<(&'static str, String)>,
}

impl CliError {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            reason: None,
            resolution: None,
            details: Vec::new(),
        }
    }

    pub(super) fn reason(mut self, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        if !reason.trim().is_empty() {
            self.reason = Some(reason);
        }
        self
    }

    pub(super) fn resolution(mut self, resolution: impl Into<String>) -> Self {
        let resolution = resolution.into();
        if !resolution.trim().is_empty() {
            self.resolution = Some(resolution);
        }
        self
    }

    pub(super) fn detail(mut self, key: &'static str, value: impl Display) -> Self {
        let value = value.to_string();
        if !value.trim().is_empty() {
            self.details.push((key, value));
        }
        self
    }
}

impl Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CliError {}

#[derive(Debug, Clone)]
pub(super) struct MergeSubmitRequired {
    pub(super) reason: String,
    pub(super) resolution: String,
    pub(super) phase: Option<&'static str>,
    pub(super) object: Option<String>,
}

impl MergeSubmitRequired {
    pub(super) fn new(reason: impl Into<String>, resolution: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            resolution: resolution.into(),
            phase: None,
            object: None,
        }
    }

    pub(super) fn with_phase(mut self, phase: &'static str, object: impl Into<String>) -> Self {
        self.phase = Some(phase);
        self.object = Some(object.into());
        self
    }

    pub(super) fn cli_error(&self) -> CliError {
        let mut error = CliError::new(
            self.phase
                .map(phase_summary_for_error)
                .unwrap_or("stack must be submitted before merge"),
        )
        .reason(self.reason.clone())
        .resolution(self.resolution.clone());
        if let Some(phase) = self.phase {
            error = error.detail("phase", phase);
        }
        if let Some(object) = &self.object {
            error = error.detail("object", object);
        }
        error
    }
}

impl Display for MergeSubmitRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.cli_error().fmt(formatter)
    }
}

impl Error for MergeSubmitRequired {}

#[derive(Debug, Clone)]
pub(super) struct MergeSyncRequired {
    pub(super) reason: String,
    pub(super) resolution: String,
    pub(super) phase: Option<&'static str>,
    pub(super) object: Option<String>,
}

impl MergeSyncRequired {
    pub(super) fn new(reason: impl Into<String>, resolution: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            resolution: resolution.into(),
            phase: None,
            object: None,
        }
    }

    pub(super) fn with_phase(mut self, phase: &'static str, object: impl Into<String>) -> Self {
        self.phase = Some(phase);
        self.object = Some(object.into());
        self
    }

    pub(super) fn cli_error(&self) -> CliError {
        let mut error = CliError::new(
            self.phase
                .map(phase_summary_for_error)
                .unwrap_or("stack must be synced before merge"),
        )
        .reason(self.reason.clone())
        .resolution(self.resolution.clone());
        if let Some(phase) = self.phase {
            error = error.detail("phase", phase);
        }
        if let Some(object) = &self.object {
            error = error.detail("object", object);
        }
        error
    }
}

impl Display for MergeSyncRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.cli_error().fmt(formatter)
    }
}

impl Error for MergeSyncRequired {}

#[derive(Debug, Clone)]
pub(super) struct MergeUnfreezeRequired {
    pub(super) message: String,
    /// The merge target the user passed, or `None` for a whole-stack merge.
    pub(super) target: Option<String>,
    pub(super) unfreeze_targets: Vec<String>,
    pub(super) reason: String,
    pub(super) resolution: String,
}

impl MergeUnfreezeRequired {
    pub(super) fn new(
        message: impl Into<String>,
        target: Option<String>,
        unfreeze_targets: Vec<String>,
        reason: impl Into<String>,
        resolution: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            target,
            unfreeze_targets,
            reason: reason.into(),
            resolution: resolution.into(),
        }
    }

    pub(super) fn cli_error(&self) -> CliError {
        CliError::new(self.message.clone())
            .reason(self.reason.clone())
            .resolution(self.resolution.clone())
    }
}

impl Display for MergeUnfreezeRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.cli_error().fmt(formatter)
    }
}

impl Error for MergeUnfreezeRequired {}

pub(super) fn phase_summary_for_error(phase: &str) -> &'static str {
    match phase {
        "merge-pr-lookup" => "failed during merge-pr-lookup",
        "merge-pr-check" => "failed during merge-pr-check",
        "merge-push" => "failed during merge-push",
        _ => "command failed",
    }
}

pub(super) fn phase_summary(phase: &str, object: &str) -> String {
    match phase {
        "resolve-config" => "could not resolve configuration".to_owned(),
        "startup-config" => "could not prepare jj config".to_owned(),
        "resolve-stack" => format!("could not resolve stack `{object}`"),
        "resolve-merge-target" => format!("could not resolve merge target `{object}`"),
        "resolve-sync-target" => format!("could not resolve sync target `{object}`"),
        "merge-refresh-above" => format!("could not refresh stack above `{object}`"),
        "resolve-github" => format!("could not resolve GitHub context for `{object}`"),
        "resolve-pr" => format!("could not resolve PR for `{object}`"),
        "open-pr" => format!("could not open PR URL `{object}`"),
        "status" => format!("could not build status for `{object}`"),
        _ => format!("failed during {phase}"),
    }
}

pub(super) fn render_cli_error(error: &anyhow::Error, debug_log: Option<&Path>) {
    let mut diagnostic = diagnostic_from_error(error);
    if let Some(path) = debug_log {
        diagnostic
            .details
            .retain(|(key, _)| *key != "debug log" && *key != "log");
        diagnostic
            .details
            .push(("debug log", path.display().to_string()));
    }

    print_error_line(&diagnostic.message);
    print_section("reason", diagnostic.reason.as_deref());
    print_section("resolution", diagnostic.resolution.as_deref());
    print_details(&diagnostic.details);
}

pub(super) fn diagnostic_from_error(error: &anyhow::Error) -> CliError {
    for cause in error.chain() {
        if let Some(submit_required) = cause.downcast_ref::<MergeSubmitRequired>() {
            return submit_required.cli_error();
        }
        if let Some(sync_required) = cause.downcast_ref::<MergeSyncRequired>() {
            return sync_required.cli_error();
        }
        if let Some(unfreeze_required) = cause.downcast_ref::<MergeUnfreezeRequired>() {
            return unfreeze_required.cli_error();
        }
        if let Some(cli_error) = cause.downcast_ref::<CliError>() {
            return cli_error.clone();
        }
    }

    let mut chain = error.chain();
    let message = chain
        .next()
        .map(ToString::to_string)
        .unwrap_or_else(|| "command failed".to_owned());
    let mut diagnostic = diagnostic_from_message(&message);
    let causes = chain.map(ToString::to_string).collect::<Vec<_>>();
    if diagnostic.reason.is_none() && !causes.is_empty() {
        diagnostic.reason = Some(causes.join(": "));
    }
    diagnostic
}

pub(super) fn find_merge_submit_required(error: &anyhow::Error) -> Option<MergeSubmitRequired> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<MergeSubmitRequired>().cloned())
}

pub(super) fn find_merge_sync_required(error: &anyhow::Error) -> Option<MergeSyncRequired> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<MergeSyncRequired>().cloned())
}

pub(super) fn find_merge_unfreeze_required(error: &anyhow::Error) -> Option<MergeUnfreezeRequired> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<MergeUnfreezeRequired>().cloned())
}

pub(super) fn diagnostic_from_message(message: &str) -> CliError {
    let mut diagnostic = if let Some(phase) = structured_value(message, "phase=") {
        let object = structured_value(message, "object=").unwrap_or_default();
        CliError::new(phase_summary(&phase, &object)).detail("phase", phase)
    } else if message.contains("failed-command=`") {
        CliError::new("command failed")
    } else if message.contains("failed-api=`") {
        CliError::new("GitHub API request failed")
    } else {
        CliError::new(message.trim())
    };

    if let Some(object) = structured_value(message, "object=") {
        diagnostic = diagnostic.detail("object", object);
    }
    if let Some(command) = backtick_value(message, "failed-command=`") {
        diagnostic = diagnostic.detail("command", command);
    }
    if let Some(api) = backtick_value(message, "failed-api=`") {
        diagnostic = diagnostic.detail("api", api);
    }
    if let Some(reason) = structured_error_reason(message) {
        diagnostic = diagnostic.reason(reason);
    }
    if let Some(command) = backtick_value(message, "safe-next-command=`") {
        diagnostic = diagnostic.resolution(format!("run `{command}`"));
    }

    diagnostic
}

pub(super) fn structured_error_reason(message: &str) -> Option<String> {
    let start = message.find("error=")? + "error=".len();
    let mut end = message.len();
    for marker in [
        " safe-next-command=",
        " failed-command=",
        " failed-api=",
        " phase=",
        " object=",
    ] {
        if let Some(offset) = message[start..].find(marker) {
            end = end.min(start + offset);
        }
    }
    let reason = message[start..end].trim();
    (!reason.is_empty()).then(|| reason.to_owned())
}

pub(super) fn structured_value(message: &str, key: &str) -> Option<String> {
    let start = message.find(key)? + key.len();
    let value = message[start..]
        .split_once(' ')
        .map(|(value, _)| value)
        .unwrap_or(&message[start..])
        .trim();
    (!value.is_empty()).then(|| value.trim_matches('`').to_owned())
}

pub(super) fn backtick_value(message: &str, key: &str) -> Option<String> {
    let start = message.find(key)? + key.len();
    let value = message[start..].split_once('`')?.0.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

pub(super) fn print_error_line(message: &str) {
    if ui_color_enabled() {
        eprintln!("{} {}", "error:".red().bold(), message);
    } else {
        eprintln!("error: {message}");
    }
}

pub(super) fn print_section(label: &str, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    eprintln!();
    if ui_color_enabled() {
        eprintln!("{}", format!("{label}:").cyan().bold());
    } else {
        eprintln!("{label}:");
    }
    for line in value.lines() {
        eprintln!("  {line}");
    }
}

pub(super) fn print_details(details: &[(&'static str, String)]) {
    if details.is_empty() {
        return;
    }
    eprintln!();
    if ui_color_enabled() {
        eprintln!("{}", "details:".cyan().bold());
    } else {
        eprintln!("details:");
    }
    let width = details
        .iter()
        .map(|(key, _)| key.len() + 1)
        .max()
        .unwrap_or(0);
    for (key, value) in details {
        let label = format!("{key}:");
        eprintln!("  {label:width$} {value}");
    }
}

/// Turns an internal structured error string (the `phase=… object=… error=…
/// safe-next-command=…` breadcrumb form used throughout this binary) into a
/// human-readable headline plus an optional "try this next" hint. The full
/// structured string is still written to the debug log for support.
#[allow(dead_code)]
pub(super) fn humanize_error(raw: &str) -> (String, Option<String>) {
    let mut message = raw.trim().to_owned();

    // Peel off the trailing `safe-next-command=` hint, if any.
    let hint = message.rfind("safe-next-command=").map(|idx| {
        let value = message[idx + "safe-next-command=".len()..]
            .trim()
            .trim_matches('`')
            .trim()
            .to_owned();
        message.truncate(idx);
        value
    });

    // Unwrap the `phase=/object=/failed-command=/failed-api=` breadcrumb prefixes
    // down to the innermost human message.
    let mut headline = message.trim().to_owned();
    let breadcrumb_keys = ["phase=", "failed-command=", "failed-api="];
    while breadcrumb_keys.iter().any(|key| headline.starts_with(key)) {
        match headline.find(" error=") {
            Some(idx) => headline = headline[idx + " error=".len()..].trim().to_owned(),
            None => break,
        }
    }

    (headline, hint.filter(|value| !value.is_empty()))
}
