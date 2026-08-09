use charger_core::error::ChargerError;
use clap::{Parser, Subcommand};

#[cfg(any(target_os = "linux", target_os = "android"))]
mod cmd;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod display;

#[derive(Parser)]
#[command(name = "charger-ctl", about = "Advanced battery charging control CLI")]
struct Cli {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[command(subcommand)]
    command: Commands,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Subcommand)]
enum Commands {
    /// Show current battery status
    Status,
    /// Setup charging parameters
    Set {
        #[command(subcommand)]
        target: SetTarget,
    },
    /// Manage bypass mode (direct power, no battery charge)
    Bypass {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Manage background daemon
    Daemon {
        #[arg(value_parser = ["start", "stop", "restart", "status", "reload"])]
        action: String,
    },
    /// Find available charging nodes
    Nodes,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Subcommand)]
enum SetTarget {
    Limit { value: u8 },
    Resume { value: u8 },
    Thermal { state: String },
    MaxTemp { value: i32 },
}

// 1. Buat SATU fungsi main utama tanpa #[cfg]
fn main() -> Result<(), ChargerError> {
    run_app()
}

// 2. Fungsi run_app() khusus untuk Linux & Android
#[cfg(any(target_os = "linux", target_os = "android"))]
fn run_app() -> Result<(), ChargerError> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Status => cmd::status::run()?,
        Commands::Set { target } => match target {
            SetTarget::Limit { value } => cmd::set::limit(*value)?,
            SetTarget::Resume { value } => cmd::set::resume(*value)?,
            SetTarget::Thermal { state } => cmd::set::thermal(state == "on")?,
            SetTarget::MaxTemp { value } => cmd::set::max_temp(*value)?,
        },
        Commands::Bypass { state } => cmd::bypass::run(state == "on")?,
        Commands::Daemon { action } => cmd::daemon::run(action)?,
        Commands::Nodes => {
            let profile = &charger_core::hardware::profile::GENERIC_PROFILE;
            println!("Profile: {}", profile.name);
            println!("Charging nodes ({} configured):", profile.control.charging_nodes.len());
            for node in profile.control.charging_nodes {
                println!("  - {:?}", node);
            }
            println!("Suspend nodes ({} configured):", profile.control.suspend_nodes.len());
            for node in profile.control.suspend_nodes {
                println!("  - {:?}", node);
            }
        }
    }

    Ok(())
}

// 3. Fungsi run_app() fallback untuk OS selain Linux & Android
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn run_app() -> Result<(), ChargerError> {
    eprintln!("charger-ctl is only supported on Linux and Android.");
    std::process::exit(1);
}
