use charger_core::error::ChargerError;
use clap::{Parser, Subcommand};

mod cmd;
mod display;

#[derive(Parser)]
#[command(name = "charger-ctl", about = "Advanced battery charging control CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

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
    /// Grant permissions to charging nodes
    GrantPerms,
}

#[derive(Subcommand)]
enum SetTarget {
    Limit { value: u8 },
    Resume { value: u8 },
    Thermal { state: String },
    MaxTemp { value: i32 },
}

fn main() -> Result<(), ChargerError> {
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
            let chg = charger_core::battery::nodes::detect_node(
                charger_core::battery::nodes::CHARGING_NODES,
            );
            let sus = charger_core::battery::nodes::detect_node(
                charger_core::battery::nodes::SUSPEND_NODES,
            );
            println!("Charging node: {:?}", chg);
            println!("Suspend node: {:?}", sus);
        }
        Commands::GrantPerms => {
            charger_core::battery::control::grant_node_permissions()?;
            println!("Granted 0644 to sysfs charging nodes");
        }
    }

    Ok(())
}
