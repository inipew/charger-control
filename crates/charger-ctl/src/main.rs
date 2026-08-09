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

    /// Manage bypass mode
    Bypass {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },

    /// Manage background daemon
    Daemon {
        #[arg(
            value_parser = [
                "start",
                "stop",
                "restart",
                "status",
                "reload"
            ]
        )]
        action: String,
    },

    /// Find available charging nodes
    Nodes,

    /// Grant permissions to charging nodes
    GrantPerms,

    /// Debugging utilities
    Debug {
        #[arg(value_parser = ["uevent"])]
        action: String,
    },
}

#[derive(Subcommand)]
enum SetTarget {
    Limit {
        value: u8,
    },

    Resume {
        value: u8,
    },

    Thermal {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },

    MaxTemp {
        value: i32,
    },
}

fn main() -> Result<(), ChargerError> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Status => {
            cmd::status::run()?;
        }

        Commands::Set { target } => match target {
            SetTarget::Limit { value } => {
                cmd::set::limit(*value)?;
            }

            SetTarget::Resume { value } => {
                cmd::set::resume(*value)?;
            }

            SetTarget::Thermal { state } => {
                cmd::set::thermal(state == "on")?;
            }

            SetTarget::MaxTemp { value } => {
                cmd::set::max_temp(*value)?;
            }
        },

        Commands::Bypass { state } => {
            cmd::bypass::run(state == "on")?;
        }

        Commands::Daemon { action } => {
            cmd::daemon::run(action)?;
        }

        Commands::Nodes => {
            let charging = charger_core::battery::nodes::detect_node(
                charger_core::battery::nodes::CHARGING_NODES,
            );

            let suspend = charger_core::battery::nodes::detect_node(
                charger_core::battery::nodes::SUSPEND_NODES,
            );

            println!("Charging nodes: {:?}", charging);

            println!("Suspend nodes : {:?}", suspend);
        }

        Commands::GrantPerms => {
            charger_core::battery::control::grant_node_permissions()?;

            println!("Charging node permissions updated.");
        }

        Commands::Debug { action } => {
            if action == "uevent" {
                cmd::debug::run_uevent_dumper()?;
            }
        }
    }

    Ok(())
}
