use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(super) struct CacheFile {
    #[serde(default)]
    pub(super) repos: BTreeMap<String, RepoCache>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(super) struct RepoCache {
    #[serde(default)]
    pub(super) changes: BTreeMap<String, PrCacheEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PrCacheEntry {
    pub(super) pr_number: u64,
    #[serde(default)]
    pub(super) pr_node_id: String,
    pub(super) head_branch: String,
    pub(super) base_branch: String,
    pub(super) base_ref: String,
    #[serde(default)]
    pub(super) head_repo_id: String,
    #[serde(default)]
    pub(super) head_repo_node_id: String,
    #[serde(default)]
    pub(super) head_repo_name: String,
    #[serde(default)]
    pub(super) base_repo_id: String,
    #[serde(default)]
    pub(super) base_repo_node_id: String,
    #[serde(default)]
    pub(super) base_repo_name: String,
    pub(super) head_sha: String,
    pub(super) base_sha: String,
    #[serde(default)]
    pub(super) author_login: String,
    #[serde(default)]
    pub(super) title: String,
    #[serde(default)]
    pub(super) body: String,
    #[serde(default)]
    pub(super) created_at: String,
    pub(super) stack_comment_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct CacheStore {
    pub(super) path: PathBuf,
    pub(super) cache: CacheFile,
}

impl CacheStore {
    #[tracing::instrument(skip_all, fields(phase = phase))]
    pub(super) fn load_current_best_effort(
        runner: &impl CommandRunner,
        diagnostics: Diagnostics,
        phase: &str,
    ) -> Result<Self> {
        let repo_dir = resolve_current_jj_repo_dir(runner)?;
        let path = repo_dir.join(CONFIG_PREFIX).join("cache.sqlite");
        match Self::load(path.clone()) {
            Ok(store) => Ok(store),
            Err(error) => {
                diagnostics.warn(format!(
                    "phase={phase} object={} error=failed to read SQLite cache; continuing with live discovery: {error:#}",
                    path.display()
                ));
                Ok(Self::empty(path))
            }
        }
    }

    #[tracing::instrument(skip_all)]
    pub(super) fn load(path: PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::empty(path));
        }

        let conn = Connection::open(&path)
            .with_context(|| format!("open SQLite cache {}", path.display()))?;
        init_cache_schema(&conn)
            .with_context(|| format!("initialize SQLite cache {}", path.display()))?;
        let mut statement = conn
            .prepare(
                "SELECT repo, change_id, pr_number, pr_node_id, head_branch, base_branch,
                        base_ref, head_repo_id, head_repo_node_id, head_repo_name, base_repo_id,
                        base_repo_node_id, base_repo_name, head_sha, base_sha, author_login,
                        title, body, created_at, stack_comment_id
                   FROM pr_cache",
            )
            .with_context(|| format!("prepare SQLite cache read {}", path.display()))?;
        let rows = statement
            .query_map([], |row| {
                let repo: String = row.get(0)?;
                let change_id: String = row.get(1)?;
                let pr_number: i64 = row.get(2)?;
                let entry = PrCacheEntry {
                    pr_number: pr_number as u64,
                    pr_node_id: row.get(3)?,
                    head_branch: row.get(4)?,
                    base_branch: row.get(5)?,
                    base_ref: row.get(6)?,
                    head_repo_id: row.get(7)?,
                    head_repo_node_id: row.get(8)?,
                    head_repo_name: row.get(9)?,
                    base_repo_id: row.get(10)?,
                    base_repo_node_id: row.get(11)?,
                    base_repo_name: row.get(12)?,
                    head_sha: row.get(13)?,
                    base_sha: row.get(14)?,
                    author_login: row.get(15)?,
                    title: row.get(16)?,
                    body: row.get(17)?,
                    created_at: row.get(18)?,
                    stack_comment_id: row.get(19)?,
                };
                Ok((repo, change_id, entry))
            })
            .with_context(|| format!("query SQLite cache {}", path.display()))?;
        let mut cache = CacheFile::default();
        for row in rows {
            let (repo, change_id, entry) =
                row.with_context(|| format!("read SQLite cache row {}", path.display()))?;
            cache
                .repos
                .entry(repo)
                .or_default()
                .changes
                .insert(change_id, entry);
        }

        Ok(Self { path, cache })
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(super) fn empty(path: PathBuf) -> Self {
        Self {
            path,
            cache: CacheFile::default(),
        }
    }

    #[tracing::instrument(skip_all)]
    pub(super) fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create cache directory {}", parent.display()))?;
        }

        let mut conn = Connection::open(&self.path)
            .with_context(|| format!("open SQLite cache {}", self.path.display()))?;
        init_cache_schema(&conn)
            .with_context(|| format!("initialize SQLite cache {}", self.path.display()))?;
        let tx = conn
            .transaction()
            .with_context(|| format!("start SQLite cache transaction {}", self.path.display()))?;
        tx.execute("DELETE FROM pr_cache", [])
            .with_context(|| format!("clear SQLite cache {}", self.path.display()))?;
        for (repo, repo_cache) in &self.cache.repos {
            for (change_id, entry) in &repo_cache.changes {
                tx.execute(
                    "INSERT INTO pr_cache (
                        repo, change_id, pr_number, pr_node_id, head_branch, base_branch,
                        base_ref, head_repo_id, head_repo_node_id, head_repo_name, base_repo_id,
                        base_repo_node_id, base_repo_name, head_sha, base_sha, author_login,
                        title, body, created_at, stack_comment_id
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                              ?15, ?16, ?17, ?18, ?19, ?20)",
                    params![
                        repo,
                        change_id,
                        entry.pr_number as i64,
                        entry.pr_node_id,
                        entry.head_branch,
                        entry.base_branch,
                        entry.base_ref,
                        entry.head_repo_id,
                        entry.head_repo_node_id,
                        entry.head_repo_name,
                        entry.base_repo_id,
                        entry.base_repo_node_id,
                        entry.base_repo_name,
                        entry.head_sha,
                        entry.base_sha,
                        entry.author_login,
                        entry.title,
                        entry.body,
                        entry.created_at,
                        entry.stack_comment_id,
                    ],
                )
                .with_context(|| {
                    format!(
                        "write SQLite cache row {}:{} to {}",
                        repo,
                        change_id,
                        self.path.display()
                    )
                })?;
            }
        }
        tx.commit()
            .with_context(|| format!("commit SQLite cache {}", self.path.display()))
    }

    #[tracing::instrument(skip_all, fields(phase = phase))]
    pub(super) fn save_best_effort(&self, diagnostics: Diagnostics, phase: &str) -> bool {
        match self.save() {
            Ok(()) => true,
            Err(error) => {
                diagnostics.warn(format!(
                    "phase={phase} object={} error=failed to write SQLite cache; continuing because cache is not authoritative: {error:#}",
                    self.path.display()
                ));
                false
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, fields(repo = repo, change = change_id))]
    pub(super) fn get_pr(&self, repo: &str, change_id: &str) -> Option<&PrCacheEntry> {
        self.cache
            .repos
            .get(repo)
            .and_then(|repo_state| repo_state.changes.get(change_id))
    }

    #[tracing::instrument(skip_all, fields(repo = repo, change = change_id))]
    pub(super) fn upsert_pr(&mut self, repo: &str, change_id: &str, entry: PrCacheEntry) {
        self.cache
            .repos
            .entry(repo.to_owned())
            .or_default()
            .changes
            .insert(change_id.to_owned(), entry);
    }
}

#[tracing::instrument(level = "trace", skip_all)]
pub(super) fn init_cache_schema(conn: &Connection) -> Result<()> {
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
    )
    .context("create SQLite cache schema")
}
