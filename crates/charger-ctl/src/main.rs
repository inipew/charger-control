use charger_core::error::ChargerError;
use clap::{Parser, Subcommand};

mod client;
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
    Status {
        /// Output status in JSON format
        #[arg(long)]
        json: bool,
    },

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

    /// Debugging & observation utilities
    Debug {
        #[command(subcommand)]
        command: DebugCommands,
    },
}

#[derive(Subcommand)]
enum DebugCommands {
    /// Live device observation & dry-run simulation (Safe read-only, no sysfs actuator writes)
    Observe {
        /// File path to save output log (e.g. /data/adb/charger-control/observe.log)
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,

        /// Polling interval in seconds
        #[arg(short, long, default_value = "2")]
        interval: u64,

        /// Simulated charge limit (50-100%)
        #[arg(short, long)]
        limit: Option<u8>,

        /// Simulated resume limit
        #[arg(short, long)]
        resume: Option<u8>,

        /// Simulated max charge current in mA (e.g. 1500 for Gentle Mode, 0 for unconstrained)
        #[arg(long)]
        max_current: Option<u32>,

        /// Simulated thermal throttle toggle (true/false)
        #[arg(long)]
        thermal_throttle: Option<bool>,
    },

    /// Deep probe of all /sys/class/power_supply sysfs nodes & permissions
    Nodes {
        /// File path to save output log
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,
    },

    /// Live raw kernel netlink uevent stream dumper
    Uevent,
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

    /// Set maximum charge current in mA (0 for unconstrained, or 500..=10000 mA)
    MaxCurrent {
        value: u32,
    },

    /// Enable or disable stepped adaptive thermal throttling
    ThermalThrottle {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },

    /// Enable or disable overall charging control daemon management
    Enable {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
}

fn main() -> Result<(), ChargerError> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Status { json } => {
            cmd::status::run(*json)?;
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

            SetTarget::MaxCurrent { value } => {
                cmd::set::max_current(*value)?;
            }

            SetTarget::ThermalThrottle { state } => {
                cmd::set::thermal_throttle(state == "on")?;
            }

            SetTarget::Enable { state } => {
                cmd::set::enable(state == "on")?;
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

        Commands::Debug { command } => match command {
            DebugCommands::Observe {
                output,
                interval,
                limit,
                resume,
                max_current,
                thermal_throttle,
            } => {
                cmd::debug::run_observer(
                    output.clone(),
                    *interval,
                    *limit,
                    *resume,
                    *max_current,
                    *thermal_throttle,
                )?;
            }
            DebugCommands::Nodes { output } => {
                cmd::debug::run_node_dump(output.as_deref())?;
            }
            DebugCommands::Uevent => {
                cmd::debug::run_uevent_dumper()?;
            }
        },
    }

    Ok(())
}
