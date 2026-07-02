use super::*;

/// Global toggle controlling whether the user-facing `ui_*` macros emit ANSI
/// color escapes. Set once via [`init_ui`]; defaults to colored output.
static UI_COLOR: OnceLock<bool> = OnceLock::new();

/// Returns whether the user-facing status macros should emit ANSI color.
///
/// Defaults to `true` when [`init_ui`] has not been called so that early output
/// is still styled on terminals.
pub(super) fn ui_color_enabled() -> bool {
    *UI_COLOR.get().unwrap_or(&true)
}

/// Initializes the global color toggle for the `ui_*` status macros.
///
/// Pass the desired color setting; callers typically derive it from
/// `std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()`.
pub(super) fn init_ui(color: bool) {
    let _ = UI_COLOR.set(color);
}

/// Emits an "info" status line to stdout.
macro_rules! ui_info {
    ($($arg:tt)*) => {{
        let __msg = format!($($arg)*);
        ui_info_line(&__msg);
    }};
}

/// Emits a "warning" status line to stderr.
macro_rules! ui_warn {
    ($($arg:tt)*) => {{
        let __msg = format!($($arg)*);
        ui_warn_line(&__msg);
    }};
}

/// Width of the right-aligned status verb column. Matches cargo's gutter so
/// output lines up under a familiar `   Compiling ...` shape.
const PROGRESS_VERB_WIDTH: usize = 12;

/// Emits a cargo-style progress line to stderr: a right-aligned bold-green
/// verb in a fixed-width gutter, followed by a message. Append-only — never
/// rewrites or clears lines. Goes to stderr so stdout stays clean for machine
/// output (`status --json`, follow-up command hints).
pub(super) fn ui_progress(verb: &str, message: &str) {
    // Pad the plain verb first, then color, so ANSI escapes don't throw off the
    // alignment width.
    let padded = format!("{verb:>width$}", width = PROGRESS_VERB_WIDTH);
    if ui_color_enabled() {
        eprintln!("{} {message}", padded.green().bold());
    } else {
        eprintln!("{padded} {message}");
    }
}

/// Wraps `text` in an OSC 8 terminal hyperlink pointing at `url`, so capable
/// terminals render `text` as a clickable link to the full URL without showing
/// it inline. When rich output is disabled (`NO_COLOR`, piped output) it falls
/// back to plain `text`.
pub(super) fn ui_hyperlink(url: &str, text: &str) -> String {
    if ui_color_enabled() {
        format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
    } else {
        text.to_owned()
    }
}

/// Emits a red `error:` line to stderr for a human-readable failure headline.
#[allow(dead_code)]
pub(super) fn ui_error(message: &str) {
    if ui_color_enabled() {
        eprintln!("{} {message}", "error:".red().bold());
    } else {
        eprintln!("error: {message}");
    }
}

/// Emits a dimmed `hint:` line suggesting a safe next command.
#[allow(dead_code)]
pub(super) fn ui_hint(message: &str) {
    if ui_color_enabled() {
        eprintln!("{} {message}", "hint:".cyan().bold());
    } else {
        eprintln!("hint: {message}");
    }
}

pub(super) fn ui_info_line(message: &str) {
    let padded = format!("{:>width$}", "Info", width = PROGRESS_VERB_WIDTH);
    if ui_color_enabled() {
        println!("{} {message}", padded.cyan().bold());
    } else {
        println!("{padded} {message}");
    }
}

pub(super) fn ui_warn_line(message: &str) {
    let padded = format!("{:>width$}", "Warning", width = PROGRESS_VERB_WIDTH);
    if ui_color_enabled() {
        eprintln!("{} {message}", padded.yellow().bold());
    } else {
        eprintln!("{padded} {message}");
    }
}

/// Emits a continuation line aligned under a status message (e.g. the body of a
/// `Warning`), indented to the gutter width + 1 so it lines up beneath the
/// message column. Dimmed so it reads as secondary detail.
pub(super) fn ui_detail_line(message: &str) {
    let indent = " ".repeat(PROGRESS_VERB_WIDTH + 1);
    if ui_color_enabled() {
        eprintln!("{indent}{}", message.dimmed());
    } else {
        eprintln!("{indent}{message}");
    }
}

/// Visual intent for a dry-run plan body line rendered by [`ui_plan_line`].
pub(super) enum PlanLineStyle {
    /// A section header, e.g. `planned mutations:`.
    Header,
    /// A mutation that creates something new.
    Create,
    /// A mutation that updates something that already exists.
    Update,
    /// A no-op the plan lists only for completeness.
    Unchanged,
    /// A nested detail beneath a mutation (the `├─`/`└─` tree lines).
    Detail,
}

/// Renders a dry-run plan body line aligned under the status-message column and
/// colored to convey intent, without the repeated `Info` gutter that the plain
/// `ui_info` lines carry. Changed mutations (`Create`/`Update`) are colored so
/// they pop; no-ops and nested details recede. This is the shared look every
/// command's dry-run plan uses so the output reads the same everywhere.
pub(super) fn ui_plan_line(message: &str, style: PlanLineStyle) {
    let indent = " ".repeat(PROGRESS_VERB_WIDTH + 1);
    if !ui_color_enabled() {
        println!("{indent}{message}");
        return;
    }
    let styled = match style {
        PlanLineStyle::Header => message.cyan().bold().to_string(),
        PlanLineStyle::Create => message.green().to_string(),
        PlanLineStyle::Update => message.yellow().to_string(),
        PlanLineStyle::Unchanged | PlanLineStyle::Detail => message.dimmed().to_string(),
    };
    println!("{indent}{styled}");
}

pub(super) fn ui_conflict(message: &str) {
    let padded = format!("{:>width$}", "Conflict", width = PROGRESS_VERB_WIDTH);
    if ui_color_enabled() {
        eprintln!("{} {message}", padded.red().bold());
    } else {
        eprintln!("{padded} {message}");
    }
}

pub(super) fn ui_progress_bar(verb: &str, message: &str, total: usize) -> Option<ProgressBar> {
    if total == 0 || !std::io::stderr().is_terminal() {
        return None;
    }
    let progress = ProgressBar::new(total as u64);
    progress.set_draw_target(ProgressDrawTarget::stderr_with_hz(10));
    progress.set_prefix(format!("{verb:>width$}", width = PROGRESS_VERB_WIDTH));
    progress.set_message(message.to_owned());

    let template = if ui_color_enabled() {
        "{prefix:.green.bold} {msg} [{bar:18}] {pos}/{len}"
    } else {
        "{prefix} {msg} [{bar:18}] {pos}/{len}"
    };
    let style = ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ");
    progress.set_style(style);
    progress.set_position(0);
    progress.force_draw();
    Some(progress)
}

pub(super) fn ui_finish_progress_bar(progress: ProgressBar) {
    // Leave the completed bar on screen, but terminate its line with a newline.
    // The bar draws to stderr with a bare carriage return and no trailing newline
    // on `finish()`, so without this the next stdout write (plan/summary text)
    // lands on the bar's line and mangles it. The explicit newline moves the
    // cursor past the persisted bar so following output starts clean.
    progress.finish();
    eprintln!();
}

/// Maps an internal recovery-phase id to a cargo-style `(verb, message)` pair
/// for progress output. The verb is a present participle shown in the gutter;
/// the message names what is being acted on. Unknown phases fall back to the
/// raw id so new phases still surface something useful.
pub(super) fn phase_label(phase: &str) -> (&'static str, &str) {
    match phase {
        "resolve-github" => ("Resolving", "GitHub repository"),
        "resolve-stack" => ("Resolving", "stack"),
        "resolve-stack-comment" => ("Resolving", "stack comment"),
        "resolve-prs" => ("Resolving", "pull requests"),
        "resolve-target" => ("Resolving", "target"),
        "resolve-fetched-heads" => ("Resolving", "fetched heads"),
        "plan-submit" => ("Planning", "submit"),
        "validate-submit-bases" => ("Validating", "submit bases"),
        "validate-frozen" => ("Validating", "frozen bookmarks"),
        "verify-mutable" => ("Verifying", "mutable changes"),
        "verify-merge" => ("Verifying", "merge"),
        "merge-pr-check" => ("Checking", "merge readiness"),
        "status-aliases" => ("Checking", "jj aliases"),
        "fetch-branch" => ("Fetching", "branch"),
        "fetch-stack" => ("Fetching", "stack"),
        "submit-fetch" => ("Fetching", "remote"),
        "sync-fetch" => ("Fetching", "trunk"),
        "push-refs" => ("Pushing", "bookmarks"),
        "track-branch" => ("Tracking", "branch"),
        "track-blockers" => ("Tracking", "immutable blockers"),
        "stack-comments" => ("Updating", "stack comments"),
        "rebase-stack" => ("Rebasing", "stack"),
        "move-trunk" => ("Moving", "trunk"),
        "carry-working-copy" => ("Moving", "working copy"),
        "merge-push" => ("Merging", "fast-forward push"),
        "merge-refresh-above" => ("Refreshing", "stack above merge"),
        "merge-submit" => ("Submitting", "stack"),
        "freeze-stack" => ("Freezing", "stack bookmarks"),
        "sync-frozen" => ("Syncing", "frozen bookmarks"),
        "remove-frozen" => ("Removing", "frozen bookmarks"),
        "reset-working-copy" => ("Resetting", "working copy"),
        "sync-submit" => ("Submitting", "stack"),
        "cleanup-branches" => ("Cleaning", "branches"),
        "cleanup-merged" => ("Cleaning", "merged branches"),
        other => ("Running", other),
    }
}
