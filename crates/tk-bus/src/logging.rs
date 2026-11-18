use std::fs::OpenOptions;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

use crate::paths;

/// Log to `<home>/logs/<name>.log` (and stderr when `TK_LOG_STDERR` is set).
pub fn init(name: &str) {
    let _ = std::fs::create_dir_all(paths::logs_dir());
    let filter = EnvFilter::try_from_env("TK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    if std::env::var("TK_LOG_STDERR").is_ok() {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
        return;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::logs_dir().join(format!("{name}.log")));
    match file {
        Ok(f) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(Mutex::new(f))
                .try_init();
        }
        Err(_) => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    }
    let name = name.to_string();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("{name} panicked: {info}");
        prev(info);
    }));
}
