use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;

struct TestRepo {
    root: PathBuf,
    work: PathBuf,
    bin: PathBuf,
    remote: PathBuf,
}

#[derive(Clone, Debug)]
struct TestChange {
    commit_id: String,
    change_id: String,
}

impl TestRepo {
    fn new(name: &str) -> anyhow::Result<Self> {
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

        write_fake_gh(&bin.join("gh"))?;

        Ok(Self {
            root,
            work,
            bin,
            remote,
        })
    }

    fn path_with_fake_gh(&self) -> String {
        let old_path = env::var("PATH").unwrap_or_default();
        format!("{}:{old_path}", self.bin.display())
    }

    fn init_main(&self) -> anyhow::Result<TestChange> {
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

    fn create_change(&self, name: &str, title: &str, body: &str) -> anyhow::Result<TestChange> {
        if !self.current_change_is_empty_undescribed()? {
            run_ok_in(&self.work, "jj", &["new"])?;
        }
        let contents = format!("{name}\n{title}\n{body}\n");
        fs::write(self.work.join(format!("{name}.txt")), contents)?;
        run_ok_in(&self.work, "jj", &["describe", "-m", title, "-m", body])?;
        self.change_at("@")
    }

    fn create_linear_stack(&self, count: usize) -> anyhow::Result<Vec<TestChange>> {
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

    fn create_empty_change(&self, title: &str) -> anyhow::Result<TestChange> {
        if !self.current_change_is_empty_undescribed()? {
            run_ok_in(&self.work, "jj", &["new"])?;
        }
        run_ok_in(&self.work, "jj", &["describe", "-m", title])?;
        self.change_at("@")
    }

    fn create_sibling_changes(&self, base: &str) -> anyhow::Result<(TestChange, TestChange)> {
        run_ok_in(&self.work, "jj", &["new", base])?;
        let left = self.create_change("sibling-left", "sibling left", "")?;
        run_ok_in(&self.work, "jj", &["new", base])?;
        let right = self.create_change("sibling-right", "sibling right", "")?;
        Ok((left, right))
    }

    fn create_merge_parent_history(&self, base: &str) -> anyhow::Result<TestChange> {
        let (left, right) = self.create_sibling_changes(base)?;
        run_ok_in(
            &self.work,
            "jj",
            &["new", &left.commit_id, &right.commit_id],
        )?;
        run_ok_in(
            &self.work,
            "jj",
            &["describe", "-m", "merge parent history"],
        )?;
        self.change_at("@")
    }

    fn create_conflicted_change(&self, base: &str) -> anyhow::Result<TestChange> {
        run_ok_in(&self.work, "jj", &["new", base])?;
        fs::write(self.work.join("file.txt"), "left\n")?;
        run_ok_in(&self.work, "jj", &["describe", "-m", "conflict left"])?;
        let left = self.change_at("@")?;

        run_ok_in(&self.work, "jj", &["new", base])?;
        fs::write(self.work.join("file.txt"), "right\n")?;
        run_ok_in(&self.work, "jj", &["describe", "-m", "conflict right"])?;
        let right = self.change_at("@")?;

        let output = Command::new("jj")
            .args(["rebase", "-r", &right.change_id, "-d", &left.change_id])
            .current_dir(&self.work)
            .output()?;
        assert!(
            output.status.success(),
            "jj rebase should leave a conflicted change but still succeed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        self.change_at(&right.change_id)
    }

    fn set_bookmark(&self, name: &str, rev: &str) -> anyhow::Result<()> {
        run_ok_in(&self.work, "jj", &["bookmark", "set", name, "-r", rev])
    }

    fn push_bookmark(&self, name: &str) -> anyhow::Result<()> {
        run_ok_in(
            &self.work,
            "jj",
            &["git", "push", "--remote", "origin", "--bookmark", name],
        )
    }

    fn fetch_origin(&self) -> anyhow::Result<()> {
        run_ok_in(&self.work, "jj", &["git", "fetch", "--remote", "origin"])
    }

    fn bookmark_target(&self, name: &str) -> anyhow::Result<String> {
        self.rev_commit_id(name)
    }

    fn tracked_remote_target(&self, name: &str) -> anyhow::Result<String> {
        self.rev_commit_id(&format!("{name}@origin"))
    }

    fn git_remote_branch_target(&self, name: &str) -> anyhow::Result<String> {
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

    fn is_mutable(&self, rev: &str) -> anyhow::Result<bool> {
        let output = run_stdout_in(
            &self.work,
            "jj",
            &["log", "--no-graph", "-r", rev, "-T", "immutable"],
        )?;
        Ok(output.trim() == "false")
    }

    fn repo_config(&self, key: &str) -> anyhow::Result<String> {
        run_stdout_in(
            &self.work,
            "jj",
            &["config", "get", "--repository", ".", key],
        )
        .map(|value| value.trim().to_owned())
    }

    fn change_at(&self, rev: &str) -> anyhow::Result<TestChange> {
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

    fn rev_commit_id(&self, rev: &str) -> anyhow::Result<String> {
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

    fn run_forklift(&self, args: &[&str]) -> anyhow::Result<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_forklift"))
            .args(args)
            .current_dir(&self.work)
            .env("PATH", self.path_with_fake_gh())
            .output()?)
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn real_jj_test_repo_helpers_create_stack_and_inspect_cache() -> anyhow::Result<()> {
    let repo = TestRepo::new("harness-smoke")?;
    let main = repo.init_main()?;
    assert!(repo.remote.exists());
    assert!(repo.is_mutable("@")?);

    let stack = repo.create_linear_stack(2)?;
    assert_eq!(stack.len(), 2);
    assert_ne!(stack[0].commit_id, stack[1].commit_id);
    assert_ne!(stack[0].change_id, stack[1].change_id);
    assert!(repo.is_mutable("@")?);

    repo.set_bookmark("stack/test-helper", "@")?;
    assert_eq!(
        repo.bookmark_target("stack/test-helper")?,
        stack[1].commit_id
    );
    repo.push_bookmark("stack/test-helper")?;
    repo.fetch_origin()?;
    assert_eq!(
        repo.tracked_remote_target("stack/test-helper")?,
        stack[1].commit_id
    );

    let configured_name = repo.repo_config("user.name")?;
    assert_eq!(configured_name, "Test User");
    assert_eq!(repo.bookmark_target("main")?, main.commit_id);

    let empty = repo.create_empty_change("empty helper")?;
    assert_eq!(empty.commit_id, repo.rev_commit_id("@")?);

    let (left, right) = repo.create_sibling_changes(&main.commit_id)?;
    assert_ne!(left.commit_id, right.commit_id);

    let merge = repo.create_merge_parent_history(&main.commit_id)?;
    assert_eq!(merge.commit_id, repo.rev_commit_id("@")?);

    let conflicted = repo.create_conflicted_change(&main.commit_id)?;
    let conflict = run_stdout_in(
        &repo.work,
        "jj",
        &[
            "log",
            "--no-graph",
            "-r",
            &conflicted.change_id,
            "-T",
            "conflict",
        ],
    )?;
    assert_eq!(conflict.trim(), "true");

    Ok(())
}

#[test]
fn real_jj_submit_keeps_own_pushed_branch_mutable_after_fetch() -> anyhow::Result<()> {
    let repo = TestRepo::new("tracked-bookmark-submit")?;

    repo.init_main()?;
    repo.create_change("change", "change title", "")?;

    let output = repo.run_forklift(&["submit", "--yes"])?;
    assert_success("forklift submit", &output);

    repo.fetch_origin()?;
    assert!(
        repo.is_mutable("@")?,
        "submitted own branch should remain mutable after fetch"
    );

    run_ok_in(&repo.work, "jj", &["describe", "-m", "change title edited"])?;

    Ok(())
}

#[test]
fn real_jj_submit_updates_existing_pr_after_edit() -> anyhow::Result<()> {
    let repo = TestRepo::new("tracked-bookmark-update")?;

    repo.init_main()?;
    let original = repo.create_change("change", "change title", "")?;

    let output = repo.run_forklift(&["submit", "--yes"])?;
    assert_success("initial forklift submit", &output);

    fs::write(
        repo.work.join("change.txt"),
        "change\nchange title\nedited\n",
    )?;
    run_ok_in(&repo.work, "jj", &["describe", "-m", "change title edited"])?;
    let edited = repo.change_at("@")?;
    assert_ne!(edited.commit_id, original.commit_id);

    let output = repo.run_forklift(&["submit", "--yes"])?;
    assert_success("updated forklift submit", &output);

    let local_after_submit = repo.change_at("@")?;
    repo.fetch_origin()?;
    let branch = "stack/change-title".to_owned() + "-" + &edited.change_id[..8];
    assert_eq!(repo.bookmark_target(&branch)?, local_after_submit.commit_id);
    assert_eq!(repo.git_remote_branch_target(&branch)?, edited.commit_id);
    assert!(repo.is_mutable("@")?);
    let fake_state = fs::read_to_string(repo.work.join(".fake-gh-state.json"))?;
    assert!(fake_state.contains("change title edited"), "{fake_state}");
    assert!(fake_state.contains("2026-06-03T12:35:56Z"), "{fake_state}");

    Ok(())
}

#[test]
fn real_jj_frozen_pr_bookmark_makes_imported_revision_immutable() -> anyhow::Result<()> {
    let repo = TestRepo::new("frozen-import-immutable")?;

    repo.init_main()?;
    let imported = repo.create_change("imported", "imported title", "")?;
    repo.set_bookmark("forklift/frozen/pr-11", &imported.commit_id)?;
    run_ok_in(
        &repo.work,
        "jj",
        &[
            "config",
            "set",
            "--repo",
            "revset-aliases.\"forklift_frozen_heads()\"",
            "bookmarks(glob:'forklift/frozen/*')",
        ],
    )?;
    run_ok_in(
        &repo.work,
        "jj",
        &[
            "config",
            "set",
            "--repo",
            "revset-aliases.\"immutable_heads()\"",
            "builtin_immutable_heads() | forklift_frozen_heads()",
        ],
    )?;

    assert!(
        !repo.is_mutable("forklift/frozen/pr-11")?,
        "frozen imported PR revision should be immutable"
    );
    let output = Command::new("jj")
        .args([
            "describe",
            "-r",
            "forklift/frozen/pr-11",
            "-m",
            "edited imported title",
        ])
        .current_dir(&repo.work)
        .output()?;
    assert!(
        !output.status.success(),
        "normal jj edit should reject frozen imported revision\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("immutable"),
        "error should explain immutable protection\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(())
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

fn assert_success(command: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{command} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn display_command(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn submit_outside_jj_repo_fails_with_clear_error() -> anyhow::Result<()> {
    let dir = unique_dir("no-jj-repo")?;
    let output = Command::new(env!("CARGO_BIN_EXE_forklift"))
        .args(["submit", "--dry-run"])
        .current_dir(&dir)
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "expected failure outside a jj repo, got: {stderr}"
    );
    assert!(
        stderr.contains("not inside a jj repository"),
        "expected clear preflight error, got: {stderr}"
    );
    assert!(
        !stderr.contains("No repo config path found"),
        "should not leak the raw jj config error, got: {stderr}"
    );
    assert!(
        !stderr.contains("phase=") && !stderr.contains("safe-next-command="),
        "should not leak structured breadcrumbs to the terminal, got: {stderr}"
    );

    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

fn unique_dir(name: &str) -> anyhow::Result<PathBuf> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = env::temp_dir().join(format!(
        "forklift-real-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn write_fake_gh(path: &Path) -> anyhow::Result<()> {
    fs::write(path, FAKE_GH)?;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

const FAKE_GH: &str = r#"#!/usr/bin/env python3
import json
import os
import subprocess
import sys

args = sys.argv[1:]
state_path = ".fake-gh-state.json"

def field_values(args):
    values = {}
    index = 0
    while index < len(args):
        if args[index] == "-f" and index + 1 < len(args):
            key, _, value = args[index + 1].partition("=")
            values[key] = value
            index += 2
        else:
            index += 1
    return values

def git_stdout(*args):
    return subprocess.check_output(["git", *args], text=True).strip()

def load_state():
    if not os.path.exists(state_path):
        return {}
    with open(state_path) as f:
        return json.load(f)

def save_state(state):
    with open(state_path, "w") as f:
        json.dump(state, f)

def pr_json(number, head, base, title, body):
    head_oid = git_stdout("ls-remote", "origin", f"refs/heads/{head}").split()[0]
    base_oid = git_stdout("rev-parse", base)
    return {
        "number": number,
        "state": "OPEN",
        "headRefName": head,
        "baseRefName": base,
        "headRefOid": head_oid,
        "baseRefOid": base_oid,
        "id": f"PR_node_{number}",
        "headRepository": {
            "id": "repo-id",
            "node_id": "repo-node",
            "nameWithOwner": "owner/repo",
        },
        "baseRepository": {
            "id": "repo-id",
            "node_id": "repo-node",
            "nameWithOwner": "owner/repo",
        },
        "author": {
            "login": "octocat",
        },
        "title": title,
        "body": body,
        "createdAt": "2026-06-03T12:34:56Z",
    }

if args == ["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"]:
    print("owner/repo")
    sys.exit(0)

if args == ["api", "user", "--jq", ".login"]:
    print("octocat")
    sys.exit(0)

if args[:2] == ["pr", "list"]:
    print("[]")
    sys.exit(0)

if args[:4] == ["api", "-X", "POST", "repos/owner/repo/pulls"]:
    values = field_values(args)
    head = values["head"]
    base = values["base"]
    state = load_state()
    state["pr"] = {
        "number": 9,
        "head": head,
        "base": base,
        "title": values.get("title", ""),
        "body": values.get("body", ""),
    }
    save_state(state)
    print(json.dumps(pr_json(9, head, base, state["pr"]["title"], state["pr"]["body"])))
    sys.exit(0)

if len(args) == 4 and args[:2] == ["api", "repos/owner/repo/pulls/9"] and args[2] == "--jq":
    pr = load_state().get("pr")
    if not pr:
        print("missing fake PR", file=sys.stderr)
        sys.exit(1)
    print(json.dumps(pr_json(pr["number"], pr["head"], pr["base"], pr["title"], pr["body"])))
    sys.exit(0)

if args[:4] == ["api", "-X", "PATCH", "repos/owner/repo/pulls/9"]:
    values = field_values(args)
    state = load_state()
    pr = state.get("pr")
    if not pr:
        print("missing fake PR", file=sys.stderr)
        sys.exit(1)
    pr["base"] = values.get("base", pr["base"])
    pr["title"] = values.get("title", pr["title"])
    pr["body"] = values.get("body", pr["body"])
    state["pr"] = pr
    save_state(state)
    print(json.dumps(pr_json(pr["number"], pr["head"], pr["base"], pr["title"], pr["body"])))
    sys.exit(0)

if len(args) >= 4 and args[:2] == ["api", "--paginate"] and args[2] == "repos/owner/repo/issues/9/comments":
    comment = load_state().get("comment")
    if comment:
        print(json.dumps({
            "id": comment["id"],
            "body": comment["body"],
            "userLogin": "octocat",
            "updatedAt": comment["updatedAt"],
        }))
    sys.exit(0)

if args[:4] == ["api", "-X", "POST", "repos/owner/repo/issues/9/comments"]:
    values = field_values(args)
    state = load_state()
    state["comment"] = {
        "id": 101,
        "body": values.get("body", ""),
        "updatedAt": "2026-06-03T12:34:56Z",
    }
    save_state(state)
    print(json.dumps({"id": 101}))
    sys.exit(0)

if args[:4] == ["api", "-X", "PATCH", "repos/owner/repo/issues/comments/101"]:
    values = field_values(args)
    state = load_state()
    comment = state.get("comment", {"id": 101})
    comment["body"] = values.get("body", comment.get("body", ""))
    comment["updatedAt"] = "2026-06-03T12:35:56Z"
    state["comment"] = comment
    save_state(state)
    print(json.dumps({"id": 101}))
    sys.exit(0)

print("unconfigured gh command: " + " ".join(args), file=sys.stderr)
sys.exit(1)
"#;
