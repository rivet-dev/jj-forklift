use super::*;

const FORKLIFT_LOG_ENV: &str = "FORKLIFT_LOG";
const FORKLIFT_LOG_STDERR_ENV: &str = "FORKLIFT_LOG_STDERR";
const DEFAULT_FILE_LOG_FILTER: &str = "warn,forklift=debug";
const DEFAULT_STDERR_LOG_FILTER: &str = "info,forklift=debug";

/// Keeps the non-blocking file writer alive until the command exits.
pub(super) struct TraceLog {
    path: Option<PathBuf>,
    _guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl TraceLog {
    pub(super) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// Initializes the global `tracing` subscriber.
///
/// By default, trace events are written to a per-run log file using
/// `warn,forklift=debug`. Set `FORKLIFT_LOG` to override the file log filter,
/// or set it to `off`/`false`/`0` to disable file logging. Stderr tracing is
/// opt-in via `FORKLIFT_LOG_STDERR`.
pub(super) fn init_tracing(command_name: &str) -> TraceLog {
    let (file_writer, file_guard, log_path) = if file_logging_enabled() {
        match open_debug_log(command_name) {
            Ok((path, file)) => {
                let (writer, guard) = tracing_appender::non_blocking(file);
                (Some(writer), Some(guard), Some(path))
            }
            Err(error) => {
                eprintln!("warning: failed to create forklift debug log: {error:#}");
                (None, None, None)
            }
        }
    } else {
        (None, None, None)
    };

    let file_filter = env_filter_or_default(FORKLIFT_LOG_ENV, DEFAULT_FILE_LOG_FILTER);
    let file_layer = file_writer.map(|writer| {
        tracing_logfmt::builder()
            .layer()
            .with_writer(writer)
            .with_filter(file_filter)
    });

    let stderr_layer = stderr_filter().map(|filter| {
        tracing_logfmt::builder()
            .layer()
            .with_writer(std::io::stderr)
            .with_filter(filter)
    });

    let initialized = tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .is_ok();

    if initialized {
        TraceLog {
            path: log_path,
            _guard: file_guard,
        }
    } else {
        TraceLog {
            path: None,
            _guard: None,
        }
    }
}

fn file_logging_enabled() -> bool {
    match env::var(FORKLIFT_LOG_ENV) {
        Ok(value) => !is_false_env_value(&value),
        Err(_) => !cfg!(test),
    }
}

fn stderr_filter() -> Option<EnvFilter> {
    let value = env::var(FORKLIFT_LOG_STDERR_ENV).ok()?;
    if is_false_env_value(&value) {
        return None;
    }
    if is_true_env_value(&value) {
        return Some(EnvFilter::new(DEFAULT_STDERR_LOG_FILTER));
    }
    Some(env_filter_from_value(
        FORKLIFT_LOG_STDERR_ENV,
        &value,
        DEFAULT_STDERR_LOG_FILTER,
    ))
}

fn env_filter_or_default(name: &str, default_filter: &str) -> EnvFilter {
    match env::var(name) {
        Ok(value) => env_filter_from_value(name, &value, default_filter),
        Err(_) => EnvFilter::new(default_filter),
    }
}

fn env_filter_from_value(name: &str, value: &str, default_filter: &str) -> EnvFilter {
    EnvFilter::try_new(value).unwrap_or_else(|error| {
        eprintln!("warning: invalid {name}={value:?}; using {default_filter:?}: {error}");
        EnvFilter::new(default_filter)
    })
}

fn is_false_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

fn is_true_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn open_debug_log(command_name: &str) -> Result<(PathBuf, File)> {
    let log_dir = debug_log_dir();
    fs::create_dir_all(&log_dir)
        .with_context(|| format!("create debug log directory {}", log_dir.display()))?;
    let path = log_dir.join(debug_log_filename(command_name));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("create debug log {}", path.display()))?;
    Ok((path, file))
}

fn debug_log_dir() -> PathBuf {
    env::current_dir()
        .ok()
        .and_then(|cwd| discover_jj_repo_dir(&cwd))
        .map(|repo_dir| repo_dir.join(CONFIG_PREFIX).join("logs"))
        .unwrap_or_else(|| xdg_state_home().join("forklift").join("logs"))
}

fn discover_jj_repo_dir(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|path| path.join(".jj").exists())
        .and_then(|workspace_root| resolve_jj_repo_dir(workspace_root).ok())
}

fn xdg_state_home() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(env::temp_dir)
}

fn debug_log_filename(command_name: &str) -> String {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("{timestamp_ms}-{}-{command_name}.log", process::id())
}
