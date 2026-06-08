// Shared real-jj integration harness.
//
// Policy (see AGENTS.md): tests NEVER mock `jj` or `git`. Every test drives a
// real colocated `jj` repo backed by a real bare `git` remote. The ONLY faked
// process is `gh`, because we cannot create real GitHub PRs in CI. The fake
// `gh` derives PR head/base oids and merged-state dynamically from the real
// remote, so it reflects actual repository state rather than a hand-maintained
// map.
#![allow(dead_code)]

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

pub const CONFIG_PREFIX: &str = "stack";

pub struct TestRepo {
    pub root: PathBuf,
    pub work: PathBuf,
    pub bin: PathBuf,
    pub remote: PathBuf,
}

#[derive(Clone, Debug)]
pub struct TestChange {
    pub commit_id: String,
    pub change_id: String,
}

/// The two divergent trunk tips produced by [`TestRepo::diverge_remote_trunk`].
#[derive(Clone, Debug)]
pub struct DivergentTrunk {
    pub local: String,
    pub remote: String,
}

impl TestRepo {
    pub fn new(name: &str) -> anyhow::Result<Self> {
        let root = unique_dir(name)?;
        let work = root.join("work");
        let bin = root.join("bin");
        let remote = root.join("remote.git");
        fs::create_dir_all(&bin)?;

        run_ok_in(&root, "jj", &["git", "init", "--colocate", "work"])?;
        run_ok("git", &["init", "--bare", remote.to_str().unwrap()])?;
        run_ok_in(&work, "git", &["remote", "add", "origin", "../remote.git"])?;
        run_ok_in(&work, "git", &["config", "user.email", "test@example.com"])?;
        run_ok_in(&work, "git", &["config", "user.name", "Test User"])?;
        run_ok_in(
            &work,
            "jj",
            &["config", "set", "--repo", "user.email", "test@example.com"],
        )?;
        run_ok_in(
            &work,
            "jj",
            &["config", "set", "--repo", "user.name", "Test User"],
        )?;

        let repo = Self {
            root,
            work,
            bin,
            remote,
        };
        repo.install_fake_gh()?;
        repo.write_gh_state(&json!({
            "prs": [],
            "comments": {},
            "pr_numbers": {},
        }))?;
        Ok(repo)
    }

    fn install_fake_gh(&self) -> anyhow::Result<()> {
        write_executable(&self.bin.join("gh"), FAKE_GH)
    }

    pub fn path_with_fake_gh(&self) -> String {
        let old_path = env::var("PATH").unwrap_or_default();
        format!("{}:{old_path}", self.bin.display())
    }

    /// Run the real `forklift` binary in the work tree, with the fake `gh` on
    /// PATH and the gh state directory pointed at the test root.
    pub fn run(&self, args: &[&str]) -> anyhow::Result<Output> {
        self.run_in(&self.work, args)
    }

    /// Run the real `forklift` binary in an arbitrary directory (e.g. a second
    /// jj workspace), still using the fake `gh` and shared gh state.
    pub fn run_in(&self, dir: &Path, args: &[&str]) -> anyhow::Result<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_forklift"))
            .args(args)
            .current_dir(dir)
            .env("PATH", self.path_with_fake_gh())
            .env("FORKLIFT_GH_DIR", &self.root)
            .output()?)
    }

    // ----- real-repo construction helpers -----

    pub fn init_main(&self) -> anyhow::Result<TestChange> {
        fs::write(self.work.join("file.txt"), "one\n")?;
        run_ok_in(&self.work, "jj", &["describe", "-m", "initial"])?;
        run_ok_in(&self.work, "jj", &["bookmark", "set", "main", "-r", "@"])?;
        let main = self.change_at("@")?;
        run_ok_in(
            &self.work,
            "jj",
            &["git", "push", "--remote", "origin", "--bookmark", "main"],
        )?;
        Ok(main)
    }

    pub fn create_change(&self, name: &str, title: &str, body: &str) -> anyhow::Result<TestChange> {
        if !self.current_change_is_empty_undescribed()? {
            run_ok_in(&self.work, "jj", &["new"])?;
        }
        let contents = format!("{name}\n{title}\n{body}\n");
        fs::write(self.work.join(format!("{name}.txt")), contents)?;
        run_ok_in(&self.work, "jj", &["describe", "-m", title, "-m", body])?;
        self.change_at("@")
    }

    pub fn create_linear_stack(&self, count: usize) -> anyhow::Result<Vec<TestChange>> {
        (1..=count)
            .map(|index| {
                self.create_change(
                    &format!("change-{index}"),
                    &format!("change {index} title"),
                    &format!("change {index} body"),
                )
            })
            .collect()
    }

    pub fn create_empty_change(&self, title: &str) -> anyhow::Result<TestChange> {
        if !self.current_change_is_empty_undescribed()? {
            run_ok_in(&self.work, "jj", &["new"])?;
        }
        run_ok_in(&self.work, "jj", &["describe", "-m", title])?;
        self.change_at("@")
    }

    pub fn jj(&self, args: &[&str]) -> anyhow::Result<()> {
        run_ok_in(&self.work, "jj", args)
    }

    pub fn write_file(&self, name: &str, contents: &str) -> anyhow::Result<()> {
        fs::write(self.work.join(name), contents).map_err(Into::into)
    }

    pub fn set_bookmark(&self, name: &str, rev: &str) -> anyhow::Result<()> {
        run_ok_in(&self.work, "jj", &["bookmark", "set", name, "-r", rev])
    }

    pub fn push_bookmark(&self, name: &str) -> anyhow::Result<()> {
        run_ok_in(
            &self.work,
            "jj",
            &["git", "push", "--remote", "origin", "--bookmark", name],
        )
    }

    pub fn fetch_origin(&self) -> anyhow::Result<()> {
        run_ok_in(&self.work, "jj", &["git", "fetch", "--remote", "origin"])
    }

    /// Fast-forward the *remote* trunk by one commit while leaving the local
    /// trunk bookmark and working copy where they were. `restore_rev` is the
    /// revision `@` should point at afterwards (typically the stack being
    /// synced). Returns the new remote trunk commit.
    pub fn advance_remote_trunk(
        &self,
        name: &str,
        restore_rev: &str,
    ) -> anyhow::Result<TestChange> {
        run_ok_in(&self.work, "jj", &["new", "main"])?;
        fs::write(self.work.join("remote-advance.txt"), format!("{name}\n"))?;
        run_ok_in(&self.work, "jj", &["describe", "-m", name])?;
        let advanced = self.change_at("@")?;
        run_ok_in(&self.work, "jj", &["bookmark", "set", "main", "-r", "@"])?;
        run_ok_in(
            &self.work,
            "jj",
            &["git", "push", "--remote", "origin", "--bookmark", "main"],
        )?;
        // Move the local trunk bookmark back to its old tip so the local stack
        // is strictly behind the remote, and restore the working copy.
        run_ok_in(
            &self.work,
            "jj",
            &["bookmark", "set", "--allow-backwards", "main", "-r", "@-"],
        )?;
        run_ok_in(&self.work, "jj", &["edit", restore_rev])?;
        Ok(advanced)
    }

    /// Produce real trunk divergence: advance the *remote* trunk to `remote` and
    /// move the *local* trunk bookmark to a different child `local` of the shared
    /// base, so neither is an ancestor of the other. Restores `@` to
    /// `restore_rev`. (A simple force-push won't do — jj would adopt the new
    /// remote tip as the local bookmark on the next fetch, erasing divergence.)
    pub fn diverge_remote_trunk(
        &self,
        name: &str,
        restore_rev: &str,
    ) -> anyhow::Result<DivergentTrunk> {
        let base = self.rev_commit_id("main")?;
        // Remote side: a new commit on the base, pushed as trunk.
        run_ok_in(&self.work, "jj", &["new", "main"])?;
        fs::write(
            self.work.join("remote-side.txt"),
            format!("{name} remote\n"),
        )?;
        run_ok_in(
            &self.work,
            "jj",
            &["describe", "-m", &format!("{name} remote")],
        )?;
        let remote = self.change_at("@")?.commit_id;
        run_ok_in(&self.work, "jj", &["bookmark", "set", "main", "-r", "@"])?;
        run_ok_in(
            &self.work,
            "jj",
            &["git", "push", "--remote", "origin", "--bookmark", "main"],
        )?;
        // Local side: a different commit on the same base; move trunk back to it.
        run_ok_in(&self.work, "jj", &["new", &base])?;
        fs::write(self.work.join("local-side.txt"), format!("{name} local\n"))?;
        run_ok_in(
            &self.work,
            "jj",
            &["describe", "-m", &format!("{name} local")],
        )?;
        let local = self.change_at("@")?.commit_id;
        run_ok_in(
            &self.work,
            "jj",
            &["bookmark", "set", "--allow-backwards", "main", "-r", "@"],
        )?;
        run_ok_in(&self.work, "jj", &["edit", restore_rev])?;
        Ok(DivergentTrunk { local, remote })
    }

    // ----- real-repo inspection helpers -----

    pub fn bookmark_target(&self, name: &str) -> anyhow::Result<String> {
        self.rev_commit_id(name)
    }

    pub fn tracked_remote_target(&self, name: &str) -> anyhow::Result<String> {
        self.rev_commit_id(&format!("{name}@origin"))
    }

    pub fn git_remote_branch_target(&self, name: &str) -> anyhow::Result<String> {
        let output = run_stdout_in(
            &self.work,
            "git",
            &["ls-remote", "origin", &format!("refs/heads/{name}")],
        )?;
        output
            .split_whitespace()
            .next()
            .map(str::to_owned)
            .with_context(|| format!("missing remote branch `{name}`"))
    }

    pub fn remote_branch_exists(&self, name: &str) -> anyhow::Result<bool> {
        let output = run_stdout_in(
            &self.work,
            "git",
            &["ls-remote", "origin", &format!("refs/heads/{name}")],
        )?;
        Ok(!output.trim().is_empty())
    }

    pub fn bookmark_exists(&self, name: &str) -> anyhow::Result<bool> {
        // `jj log -r <missing>` exits non-zero; treat that as "absent" rather
        // than asserting success.
        let output = Command::new("jj")
            .args(["log", "--no-graph", "-r", name, "-T", "commit_id"])
            .current_dir(&self.work)
            .output()?;
        Ok(output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty())
    }

    pub fn is_mutable(&self, rev: &str) -> anyhow::Result<bool> {
        let output = run_stdout_in(
            &self.work,
            "jj",
            &["log", "--no-graph", "-r", rev, "-T", "immutable"],
        )?;
        Ok(output.trim() == "false")
    }

    pub fn change_at(&self, rev: &str) -> anyhow::Result<TestChange> {
        Ok(TestChange {
            commit_id: self.rev_commit_id(rev)?,
            change_id: run_stdout_in(
                &self.work,
                "jj",
                &["log", "--no-graph", "-r", rev, "-T", "change_id"],
            )?
            .trim()
            .to_owned(),
        })
    }

    pub fn rev_commit_id(&self, rev: &str) -> anyhow::Result<String> {
        run_stdout_in(
            &self.work,
            "jj",
            &["log", "--no-graph", "-r", rev, "-T", "commit_id"],
        )
        .map(|value| value.trim().to_owned())
    }

    fn current_change_is_empty_undescribed(&self) -> anyhow::Result<bool> {
        let output = run_stdout_in(
            &self.work,
            "jj",
            &[
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "empty ++ \"\\n\" ++ description",
            ],
        )?;
        let mut lines = output.lines();
        Ok(lines.next() == Some("true") && lines.all(|line| line.is_empty()))
    }

    /// Branch name `forklift` derives for a change: `stack/<slug>-<change8>`.
    pub fn stack_branch(&self, title_slug: &str, change_id: &str) -> String {
        format!("stack/{title_slug}-{}", &change_id[..8])
    }

    // ----- fake gh state (the ONLY mock) -----

    fn gh_state_path(&self) -> PathBuf {
        self.root.join("gh-state.json")
    }

    fn read_gh_state(&self) -> anyhow::Result<Value> {
        let path = self.gh_state_path();
        if !path.exists() {
            return Ok(json!({"prs": [], "comments": {}, "pr_numbers": {}}));
        }
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }

    fn write_gh_state(&self, state: &Value) -> anyhow::Result<()> {
        fs::write(self.gh_state_path(), serde_json::to_string(state)?).map_err(Into::into)
    }

    /// Seed an existing open PR's metadata. Head/base oids and merged-state are
    /// computed live from the real remote by the fake gh, so only the logical
    /// metadata is stored here.
    pub fn seed_pr(
        &self,
        number: u64,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        let mut state = self.read_gh_state()?;
        let prs = state["prs"].as_array_mut().expect("prs array");
        prs.push(json!({
            "number": number,
            "state": "OPEN",
            "headRefName": head,
            "baseRefName": base,
            "title": title,
            "body": body,
        }));
        self.write_gh_state(&state)
    }

    /// Force the PR number the fake assigns when a given head branch is created.
    pub fn seed_pr_number(&self, head: &str, number: u64) -> anyhow::Result<()> {
        let mut state = self.read_gh_state()?;
        state["pr_numbers"][head] = json!(number);
        self.write_gh_state(&state)
    }

    pub fn seed_comment(&self, pr_number: u64, id: u64, body: &str) -> anyhow::Result<()> {
        let mut state = self.read_gh_state()?;
        let key = pr_number.to_string();
        let comments = state["comments"].as_object_mut().expect("comments object");
        comments
            .entry(key)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("comment list")
            .push(json!({
                "id": id,
                "body": body,
                "userLogin": "octocat",
                "updatedAt": "2026-06-03T17:00:00Z",
            }));
        self.write_gh_state(&state)
    }

    /// All PRs as stored (state reflects the last query that observed a merge).
    pub fn stored_prs(&self) -> anyhow::Result<Vec<Value>> {
        Ok(self
            .read_gh_state()?
            .get("prs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub fn stored_pr(&self, number: u64) -> anyhow::Result<Value> {
        self.stored_prs()?
            .into_iter()
            .find(|pr| pr["number"] == json!(number))
            .with_context(|| format!("PR #{number} not found in fake gh state"))
    }

    /// Every recorded `gh` invocation's argv.
    pub fn gh_requests(&self) -> anyhow::Result<Vec<Vec<String>>> {
        let path = self.root.join("gh-requests.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        fs::read_to_string(path)?
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let value: Value = serde_json::from_str(line)?;
                Ok(value
                    .get("args")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect())
            })
            .collect()
    }

    pub fn gh_request_matches(&self, prefix: &[&str]) -> anyhow::Result<bool> {
        Ok(self.gh_requests()?.iter().any(|args| {
            args.iter()
                .map(String::as_str)
                .take(prefix.len())
                .eq(prefix.iter().copied())
        }))
    }

    pub fn gh_request_has_field(&self, field: &str) -> anyhow::Result<bool> {
        Ok(self
            .gh_requests()?
            .iter()
            .any(|args| args.iter().any(|arg| arg == field)))
    }

    // ----- SQLite cache helpers -----

    pub fn cache_path(&self) -> PathBuf {
        self.work
            .join(".jj")
            .join("repo")
            .join(CONFIG_PREFIX)
            .join("cache.sqlite")
    }

    pub fn cache_entry(&self, change_id: &str) -> anyhow::Result<Value> {
        cache_entry_at(&self.cache_path(), change_id)
    }

    pub fn delete_cache(&self) -> anyhow::Result<()> {
        let path = self.cache_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Reset the recorded `gh` request log so a later assertion only sees calls
    /// from the next command run.
    pub fn clear_gh_requests(&self) -> anyhow::Result<()> {
        let path = self.root.join("gh-requests.jsonl");
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Write a `pr_cache` row directly, to seed stale/standalone cache state.
    #[allow(clippy::too_many_arguments)]
    pub fn write_cache_row(
        &self,
        change_id: &str,
        pr_number: u64,
        head_branch: &str,
        base_branch: &str,
        head_sha: &str,
        base_sha: &str,
        title: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        let path = self.cache_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        init_cache_schema(&conn)?;
        conn.execute(
            "INSERT OR REPLACE INTO pr_cache (
                repo, change_id, pr_number, pr_node_id, head_branch, base_branch, base_ref,
                head_repo_id, head_repo_node_id, head_repo_name, base_repo_id,
                base_repo_node_id, base_repo_name, head_sha, base_sha, author_login, title,
                body, created_at, stack_comment_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                      ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                "owner/repo",
                change_id,
                pr_number as i64,
                format!("PR_node_{pr_number}"),
                head_branch,
                base_branch,
                base_branch,
                "repo-id",
                "repo-node",
                "owner/repo",
                "repo-id",
                "repo-node",
                "owner/repo",
                head_sha,
                base_sha,
                "octocat",
                title,
                body,
                "2026-06-03T12:34:56Z",
                "comment-101",
            ],
        )?;
        Ok(())
    }
}

fn init_cache_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pr_cache (
            repo TEXT NOT NULL,
            change_id TEXT NOT NULL,
            pr_number INTEGER NOT NULL,
            pr_node_id TEXT NOT NULL DEFAULT '',
            head_branch TEXT NOT NULL,
            base_branch TEXT NOT NULL,
            base_ref TEXT NOT NULL,
            head_repo_id TEXT NOT NULL DEFAULT '',
            head_repo_node_id TEXT NOT NULL DEFAULT '',
            head_repo_name TEXT NOT NULL DEFAULT '',
            base_repo_id TEXT NOT NULL DEFAULT '',
            base_repo_node_id TEXT NOT NULL DEFAULT '',
            base_repo_name TEXT NOT NULL DEFAULT '',
            head_sha TEXT NOT NULL,
            base_sha TEXT NOT NULL,
            author_login TEXT NOT NULL DEFAULT '',
            title TEXT NOT NULL DEFAULT '',
            body TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT '',
            stack_comment_id TEXT,
            PRIMARY KEY (repo, change_id)
        );",
    )?;
    Ok(())
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub fn cache_entry_at(path: &Path, change_id: &str) -> anyhow::Result<Value> {
    let conn = Connection::open(path)?;
    let mut statement = conn.prepare(
        "SELECT pr_number, head_branch, base_branch, head_sha, base_sha, title, body
           FROM pr_cache
          WHERE repo = 'owner/repo' AND change_id = ?1",
    )?;
    let entry = statement.query_row([change_id], |row| {
        Ok(json!({
            "pr_number": row.get::<_, i64>(0)?,
            "head_branch": row.get::<_, String>(1)?,
            "base_branch": row.get::<_, String>(2)?,
            "head_sha": row.get::<_, String>(3)?,
            "base_sha": row.get::<_, String>(4)?,
            "title": row.get::<_, String>(5)?,
            "body": row.get::<_, String>(6)?,
        }))
    })?;
    Ok(entry)
}

/// Render the stack-comment body that `forklift get`/`sync` parse. `rows` are
/// `(change_id, pr_number, head_branch, base_branch, title)` ordered bottom→top.
pub fn stack_comment_body(
    rows: &[(&str, u64, &str, &str, &str)],
    current_change_id: &str,
) -> String {
    let mut body = "<!-- stack:v1 -->\nStack for owner/repo\n\n".to_owned();
    for (change_id, number, _, _, title) in rows.iter().rev() {
        let label = format!("[{title} #{number}](https://github.com/owner/repo/pull/{number})");
        let is_current = *change_id == current_change_id;
        let label = if is_current {
            format!("**{label}**")
        } else {
            label
        };
        let current_marker = if is_current { " 👈" } else { "" };
        let short_change_id = change_id.chars().take(8).collect::<String>();
        body.push_str(&format!(
            "- {label} _{short_change_id}_ · 2026-06-03 12:34:56{current_marker}\n"
        ));
    }
    body.push_str("- main\n");
    body.push('\n');
    if let Some((_, number, _, _, _)) = rows
        .iter()
        .find(|(change_id, _, _, _, _)| *change_id == current_change_id)
    {
        body.push_str(&format!("Check out this stack: `forklift get {number}`\n"));
    }
    body.push_str("Pull/update this stack: `forklift sync`\n");
    body.push_str("Publish local edits: `forklift submit`\n");
    body.push_str("Merge when ready: `forklift merge`\n");
    body
}

pub fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn run_ok(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(program).args(args).output()?;
    assert_success(&display_command(program, args), &output);
    Ok(())
}

fn run_ok_in(dir: &Path, program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(program).args(args).current_dir(dir).output()?;
    assert_success(&display_command(program, args), &output);
    Ok(())
}

fn run_stdout_in(dir: &Path, program: &str, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(program).args(args).current_dir(dir).output()?;
    assert_success(&display_command(program, args), &output);
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn display_command(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn unique_dir(name: &str) -> anyhow::Result<PathBuf> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = env::temp_dir().join(format!(
        "forklift-real-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn write_executable(path: &Path, contents: &str) -> anyhow::Result<()> {
    fs::write(path, contents)?;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).map_err(Into::into)
}

/// The single faked process. It stores PR/comment metadata in `gh-state.json`
/// under `$FORKLIFT_GH_DIR`, logs every invocation to `gh-requests.jsonl`, and
/// derives head/base oids and merged-state from the REAL remote (`git ls-remote`
/// + `git merge-base --is-ancestor`), so it tracks actual repository state.
const FAKE_GH: &str = r#"#!/usr/bin/env python3
import json
import os
import subprocess
import sys

args = sys.argv[1:]
gh_dir = os.environ.get("FORKLIFT_GH_DIR", ".")
state_path = os.path.join(gh_dir, "gh-state.json")

with open(os.path.join(gh_dir, "gh-requests.jsonl"), "a") as fh:
    fh.write(json.dumps({"args": args}) + "\n")

def load_state():
    if not os.path.exists(state_path):
        return {"prs": [], "comments": {}, "pr_numbers": {}}
    with open(state_path) as fh:
        state = json.load(fh)
    state.setdefault("prs", [])
    state.setdefault("comments", {})
    state.setdefault("pr_numbers", {})
    return state

def save_state(state):
    with open(state_path, "w") as fh:
        json.dump(state, fh)

def git_out(*git_args):
    try:
        return subprocess.check_output(["git", *git_args], text=True,
                                       stderr=subprocess.DEVNULL).strip()
    except subprocess.CalledProcessError:
        return ""

def remote_oid(branch):
    out = git_out("ls-remote", "origin", "refs/heads/" + branch)
    return out.split()[0] if out else ""

def is_ancestor(a, b):
    if not a or not b:
        return False
    return subprocess.call(["git", "merge-base", "--is-ancestor", a, b],
                           stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL) == 0

def resolve_state(state, pr):
    """A stored-OPEN PR becomes MERGED once its head lands in its base branch on
    the real remote. Persist the transition so later reads stay consistent."""
    if pr["state"].upper() != "OPEN":
        return pr["state"]
    head = remote_oid(pr["headRefName"])
    base = remote_oid(pr["baseRefName"])
    if head and base and is_ancestor(head, base):
        pr["state"] = "MERGED"
        save_state(state)
        return "MERGED"
    return pr["state"]

def pr_view(state, pr):
    head = remote_oid(pr["headRefName"]) or "headsha"
    base = remote_oid(pr["baseRefName"]) or "basesha"
    return {
        "number": pr["number"],
        "state": resolve_state(state, pr),
        "id": "PR_node_%d" % pr["number"],
        "headRefName": pr["headRefName"],
        "baseRefName": pr["baseRefName"],
        "headRefOid": head,
        "baseRefOid": base,
        "headRepository": {"id": "repo-id", "node_id": "repo-node",
                           "nameWithOwner": "owner/repo"},
        "baseRepository": {"id": "repo-id", "node_id": "repo-node",
                           "nameWithOwner": "owner/repo"},
        "author": {"login": "octocat"},
        "title": pr.get("title", ""),
        "body": pr.get("body", ""),
        "createdAt": "2026-06-03T12:34:56Z",
        "isDraft": False,
        "reviewDecision": "APPROVED",
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "statusCheckRollup": [{"context": "ci", "state": "SUCCESS"}],
        "autoMergeRequest": None,
    }

def find_pr(state, number):
    for pr in state["prs"]:
        if int(pr["number"]) == int(number):
            return pr
    return None

def field_values():
    values = {}
    for index, arg in enumerate(args):
        if arg == "-f" and index + 1 < len(args):
            key, _, value = args[index + 1].partition("=")
            values[key] = value
    return values

if args[:2] == ["repo", "view"]:
    print("owner/repo")
    sys.exit(0)

if args[:3] == ["api", "user", "--jq"]:
    print("octocat")
    sys.exit(0)

# Push-permission probe: `api repos/owner/repo --jq .permissions.push`.
if args[:2] == ["api", "repos/owner/repo"] and "--jq" in args:
    print("true")
    sys.exit(0)

if args[:2] == ["pr", "list"]:
    state = load_state()
    wanted = args[args.index("--state") + 1].upper() if "--state" in args else None
    head = args[args.index("--head") + 1] if "--head" in args else None
    out = []
    for pr in state["prs"]:
        view = pr_view(state, pr)
        if wanted and view["state"].upper() != wanted:
            continue
        if head and view["headRefName"] != head:
            continue
        out.append(view)
    print(json.dumps(out))
    sys.exit(0)

if args[:2] == ["pr", "view"]:
    state = load_state()
    pr = find_pr(state, args[2])
    if pr is None:
        print("not found", file=sys.stderr)
        sys.exit(1)
    jq = args[args.index("--jq") + 1] if "--jq" in args else None
    view = pr_view(state, pr)
    if jq == ".state":
        print(view["state"])
    else:
        print(json.dumps(view))
    sys.exit(0)

if args[:2] == ["pr", "merge"]:
    state = load_state()
    pr = find_pr(state, args[2])
    if pr is not None:
        pr["state"] = "MERGED"
        save_state(state)
    sys.exit(0)

# GET a single PR: `api repos/owner/repo/pulls/<n>` (no -X).
if args[:1] == ["api"] and len(args) >= 2 \
        and args[1].startswith("repos/owner/repo/pulls/") and "-X" not in args:
    state = load_state()
    pr = find_pr(state, args[1].rsplit("/", 1)[1])
    if pr is None:
        print("not found", file=sys.stderr)
        sys.exit(1)
    print(json.dumps(pr_view(state, pr)))
    sys.exit(0)

# List issue comments: `api --paginate repos/owner/repo/issues/<n>/comments`.
if args[:2] == ["api", "--paginate"] and "/issues/" in args[2] \
        and args[2].endswith("/comments"):
    state = load_state()
    pr_number = args[2].split("/issues/")[1].split("/")[0]
    for comment in state["comments"].get(pr_number, []):
        print(json.dumps(comment))
    sys.exit(0)

# Create a PR.
if args[:3] == ["api", "-X", "POST"] and args[3] == "repos/owner/repo/pulls":
    state = load_state()
    values = field_values()
    head = values["head"]
    forced = state["pr_numbers"].get(head)
    number = int(forced) if forced is not None else 100 + len(state["prs"])
    pr = {
        "number": number,
        "state": "OPEN",
        "headRefName": head,
        "baseRefName": values["base"],
        "title": values.get("title", ""),
        "body": values.get("body", ""),
    }
    state["prs"].append(pr)
    save_state(state)
    print(json.dumps(pr_view(state, pr)))
    sys.exit(0)

# Update a PR (retarget/title/body).
if args[:3] == ["api", "-X", "PATCH"] and args[3].startswith("repos/owner/repo/pulls/"):
    state = load_state()
    pr = find_pr(state, args[3].rsplit("/", 1)[1])
    if pr is None:
        print("not found", file=sys.stderr)
        sys.exit(1)
    values = field_values()
    pr["baseRefName"] = values.get("base", pr["baseRefName"])
    pr["title"] = values.get("title", pr.get("title", ""))
    pr["body"] = values.get("body", pr.get("body", ""))
    save_state(state)
    print(json.dumps(pr_view(state, pr)))
    sys.exit(0)

# Create an issue comment.
if args[:3] == ["api", "-X", "POST"] and "/issues/" in args[3] \
        and args[3].endswith("/comments"):
    state = load_state()
    pr_number = args[3].split("/issues/")[1].split("/")[0]
    values = field_values()
    existing = sum(len(items) for items in state["comments"].values())
    next_id = 100 + existing + 1
    state["comments"].setdefault(pr_number, []).append({
        "id": next_id,
        "body": values.get("body", ""),
        "userLogin": "octocat",
        "updatedAt": "2026-06-03T18:00:00Z",
    })
    save_state(state)
    print(json.dumps({"id": next_id}))
    sys.exit(0)

# Update an issue comment.
if args[:3] == ["api", "-X", "PATCH"] and "/issues/comments/" in args[3]:
    state = load_state()
    comment_id = int(args[3].rsplit("/", 1)[1])
    values = field_values()
    for items in state["comments"].values():
        for comment in items:
            if int(comment["id"]) == comment_id:
                comment["body"] = values.get("body", "")
                comment["updatedAt"] = "2026-06-03T18:30:00Z"
                save_state(state)
                print(json.dumps({"id": comment_id}))
                sys.exit(0)
    print("not found", file=sys.stderr)
    sys.exit(1)

print("unconfigured gh command: " + " ".join(args), file=sys.stderr)
sys.exit(1)
"#;
