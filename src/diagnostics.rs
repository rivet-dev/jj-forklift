use super::*;

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Diagnostics {
    pub(super) verbose: bool,
    pub(super) dry_run: bool,
}

impl Diagnostics {
    #[tracing::instrument(level = "trace", skip_all, fields(phase = phase))]
    pub(super) fn phase(self, phase: &str) {
        tracing::debug!(phase, "recovery phase");
        // Dry runs narrate via the `- would ...` plan lines instead, so we
        // don't double-report with progress output there.
        if !self.dry_run && !matches!(phase, "save-cache" | "write-cache") {
            let (verb, message) = phase_label(phase);
            ui_progress(verb, message);
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(super) fn repo_details(self, store: &CacheStore) {
        tracing::debug!(cache = %store.path.display(), "resolved repo details");
    }

    #[tracing::instrument(level = "trace", skip_all, fields(program = program))]
    pub(super) fn command(self, program: &str, args: &[&str]) {
        tracing::debug!(command = %display_command(program, args), "command");
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(super) fn progress_bar(
        self,
        verb: &str,
        message: &str,
        total: usize,
    ) -> Option<ProgressBar> {
        if self.dry_run || total == 0 {
            return None;
        }
        let progress = ui_progress_bar(verb, message, total);
        if progress.is_none() {
            ui_progress(verb, message);
        }
        progress
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(super) fn submit_pr_action(
        self,
        repo: &str,
        change: &ResolvedChange,
        action: SubmitPrAction,
        entry: &PrCacheEntry,
        progress: Option<&ProgressBar>,
    ) {
        if self.dry_run {
            return;
        }
        let line = || {
            ui_progress(
                action.progress_verb(),
                &format!(
                    "PR #{} {} - {}",
                    entry.pr_number,
                    github_pr_url(repo, entry.pr_number),
                    change.title
                ),
            );
        };
        if let Some(progress) = progress {
            progress.suspend(line);
        } else {
            line();
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(super) fn warn(self, message: impl Display) {
        tracing::warn!(message = %message, "warning");
    }

    #[tracing::instrument(skip_all)]
    pub(super) fn print_submit_plan(
        self,
        config: &AppConfig,
        context: &AppContext,
        plans: &[SubmitPlan],
    ) {
        if !self.verbose && !self.dry_run {
            return;
        }

        self.plan_line("planned mutations:");
        for plan in plans {
            if plan.push_needed {
                self.plan_line(&format!(
                    "- set bookmark {} to {} and push to {}",
                    plan.head_branch, plan.change.commit_id, config.remote
                ));
            } else {
                self.plan_line(&format!(
                    "- leave bookmark {} at {}",
                    plan.head_branch, plan.change.commit_id
                ));
            }

            if plan.push_needed {
                self.plan_line(&format!(
                    "- push bookmark {} to {}/{}",
                    plan.head_branch, config.remote, plan.head_branch
                ));
            } else {
                self.plan_line(&format!(
                    "- leave remote branch {}/{} unchanged at {}",
                    config.remote, plan.head_branch, plan.change.commit_id
                ));
            }

            if plan.push_needed {
                self.plan_line(&format!(
                    "- verify remote lease for {}: expected {}",
                    plan.head_branch,
                    plan.expected_remote_head.as_deref().unwrap_or("<absent>")
                ));
            }

            match &plan.existing_pr {
                None => self.plan_line(&format!(
                    "- create PR for {}: head={} base={}",
                    plan.change.change_id, plan.head_branch, plan.base_branch
                )),
                Some(existing) if plan.pr_update_needed => self.plan_line(&format!(
                    "- update PR #{} for {}: head={} base={}",
                    existing.pr_number, plan.change.change_id, plan.head_branch, plan.base_branch
                )),
                Some(existing) => self.plan_line(&format!(
                    "- leave PR #{} unchanged for {}",
                    existing.pr_number, plan.change.change_id
                )),
            }

            match &plan.existing_pr {
                Some(existing) => self.plan_line(&format!(
                    "- upsert stack comment on PR #{} for {}",
                    existing.pr_number, plan.change.change_id
                )),
                None => self.plan_line(&format!(
                    "- upsert stack comment after creating PR for {}",
                    plan.change.change_id
                )),
            }
        }

        if plans.iter().all(|plan| !plan.push_needed)
            && plans.iter().all(|plan| !plan.pr_update_needed)
            && plans.iter().all(|plan| plan.existing_pr.is_some())
        {
            self.plan_line("- no branch or PR metadata changes");
        }

        if self.verbose {
            tracing::debug!(
                repo = %context.github.repo,
                remote = %config.remote,
                trunk = %config.trunk,
                stack_size = context.stack.len(),
                "resolved repo details"
            );
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(super) fn plan_line(self, line: &str) {
        if self.dry_run {
            ui_info!("{line}");
        } else if self.verbose {
            tracing::debug!("{line}");
        }
    }
}
