use std::path::Path;
use tracing_subscriber::{fmt, EnvFilter};

pub fn init_logger(log_path: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    let subscriber = fmt::Subscriber::builder()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with_writer(file)
        .with_ansi(false) // No colors in file
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set tracing subscriber");

    Ok(())
}
