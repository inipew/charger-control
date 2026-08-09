use std::path::Path;

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub fn init_logger(log_path: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log_path)?;

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let layer = fmt::layer()
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::rfc_3339())
        .with_writer(file)
        .with_ansi(false)
        .with_target(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(layer)
        .try_init()
        .map_err(|e| std::io::Error::other(format!("failed to initialize tracing: {e}")))
}
