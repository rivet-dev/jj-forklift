# Desired Code Layout

This is the target structure for splitting the current forklift CLI into smaller, reviewable modules.

The main rule: modules should be grouped by **responsibility**, not by "things that are shared." Avoid a catch-all `shared.rs`; if code is shared, name the domain it belongs to.

## Source Layout

```text
src/
  main.rs
  lib.rs

  app.rs
  cli.rs
  ui.rs
  tracing_setup.rs
  errors.rs
  runner.rs
  config.rs
  cache.rs
  diagnostics.rs

  commands/
    mod.rs
    submit.rs
    sync.rs
    merge.rs
    get.rs
    repair.rs
    unfreeze.rs
    status.rs
    pr.rs

  jj/
    mod.rs
    repo.rs
    revsets.rs
    stack.rs
    bookmarks.rs
    branches.rs
    rebase.rs

  github/
    mod.rs
    context.rs
    pr.rs
    comments.rs
    api.rs

  workflows/
    mod.rs
    types.rs
    submit.rs
    sync.rs
    merge.rs
    get.rs
    repair.rs
    unfreeze.rs
    status.rs
```

## Responsibilities

`main.rs` should stay tiny: parse process exit through `forklift::app::main()`.

`lib.rs` should expose crate modules and small public helpers that integration tests need. It should not become the real application body.

`app.rs` owns top-level startup and dispatch:

- initialize tracing and UI
- resolve cwd
- run preflight
- load config
- dispatch `Commands::*`
- render top-level errors

`cli.rs` owns only Clap types:

- `Cli`
- `Commands`
- command option structs
- command display/name helpers

`commands/*.rs` should be thin orchestration adapters. Each command module should translate CLI options into workflow calls and print the final command summary. Command files should not contain low-level jj/GitHub/cache logic.

Example shape:

```rust
pub(crate) fn run(
    runner: &impl CommandRunner,
    config: &AppConfig,
    options: SubmitOptions,
    diagnostics: Diagnostics,
    verbose: bool,
    dry_run: bool,
) -> Result<()>
```

`workflows/*.rs` should contain the command behavior currently represented by functions like:

- `submit_stack`
- `sync_stack`
- `merge_stack`
- `get_stack`
- `repair_stack_comments`
- `unfreeze_stack`
- `status_report`

These modules are allowed to coordinate jj, GitHub, cache, and UI diagnostics, but they should delegate low-level operations.

`jj/*` owns all direct jj concepts:

- repo discovery and `.jj/repo` resolution
- revset construction and single-rev resolution
- stack parsing and validation
- bookmark listing, tracking, deletion, frozen bookmark helpers
- rebase and trunk movement helpers

`github/*` owns all direct GitHub concepts:

- resolving repo/user context
- PR fetch/create/update/lookup
- merge PR metadata
- stack comments
- GraphQL/REST command wrappers around `gh`

`runner.rs` owns process execution:

- `CommandRunner`
- `SystemRunner`
- `CommandOutput`
- `run_required`
- `git_run`
- `gh_run`
- command display formatting

`cache.rs` owns SQLite cache shape and persistence:

- `CacheStore`
- `PrCacheEntry`
- schema initialization

`errors.rs` owns user-facing diagnostics:

- `CliError`
- merge retry marker errors
- phase error wrapping
- terminal rendering of diagnostics

`ui.rs` owns display primitives:

- `ui_info!`
- `ui_warn!`
- progress lines
- progress bars
- color toggle
- phase labels

`diagnostics.rs` owns the `Diagnostics` type and its logging/plan/progress methods.

`workflows/types.rs` owns shared workflow result and plan types such as `SubmitSummary`, `MergeSummary`, `SubmitPlan`, `RepairPlan`, and `StatusReport`.

## Test Layout

Keep tests in `tests/`. Do **not** add inline `#[cfg(test)] mod tests` blocks in `src/`.

Target test layout:

```text
tests/
  common/
    mod.rs

  submit.rs
  sync.rs
  merge.rs
  get.rs
  repair.rs
  unfreeze.rs
  status.rs
  pr.rs

  jj_stack.rs
  github_comments.rs
  cache.rs
  review_decision.rs

  real_jj.rs
  real_github.rs
```

Command behavior tests should mirror `commands/*.rs` and `workflows/*.rs`.

Pure/string/domain tests should be integration tests that call the smallest public API exposed through `src/lib.rs`. Examples:

- stack comment parsing/rendering in `tests/github_comments.rs`
- stack log parsing or branch naming in `tests/jj_stack.rs`
- cache schema or cache entry conversion in `tests/cache.rs`

Do not mock `jj` or `git`. Keep using the real jj/git harness in `tests/common/mod.rs`. `gh` remains the only fake process.

## Migration Order

Prefer small, compiling slices:

1. Keep `main.rs` tiny and move Clap types to `cli.rs`.
2. Move UI, tracing, errors, runner, config, cache, and diagnostics into named shared modules.
3. Move command entrypoints into `commands/*.rs`.
4. Move command behavior into `workflows/*.rs`.
5. Move jj primitives into `jj/*`.
6. Move GitHub primitives into `github/*`.
7. Move pure helper tests into command/domain-specific integration test files.

Each step should compile before the next step starts.

## Current Layout

The codebase follows this layout now:

- `main.rs` is a tiny binary entrypoint.
- `cli.rs` owns Clap declarations.
- `commands/*.rs` own per-command adapters.
- `workflows/*.rs` own command behavior.
- `workflows/types.rs` owns shared workflow result/plan/report types.
- `jj/*` owns jj-specific primitives.
- `github/*` owns GitHub-specific primitives.
- `ui.rs`, `tracing_setup.rs`, `runner.rs`, `config.rs`, `cache.rs`, `errors.rs`, and `diagnostics.rs` own shared infrastructure by responsibility.
- command integration tests are split by command.
- domain integration-test homes exist for jj stack behavior, GitHub comments, cache behavior, and review-decision parsing.
