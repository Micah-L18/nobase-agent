//! No-Base Agent — runs on managed servers, connects to the gateway via WSS.
//!
//! Usage:
//!   no-base run                         # Normal operation (reads /etc/no-base/agent.toml)
//!   no-base run --config ./agent.toml   # Custom config path
//!   no-base start                       # Start the agent service
//!   no-base stop                        # Stop the agent service
//!   no-base restart                     # Restart the agent service
//!   no-base status                      # Show agent service status
//!   no-base logs                        # View live agent logs
//!   no-base logs --lines 100            # View last N log lines
//!   no-base enable                      # Enable agent on boot
//!   no-base disable                     # Disable agent on boot
//!   no-base update                      # Self-update binary from backend
//!   no-base uninstall                   # Remove agent, config, and system service
//!   no-base version                     # Print version info

mod config;
mod connection;
mod dispatcher;
mod handlers;
pub mod metrics_buffer;
pub mod updater;

use clap::{Parser, Subcommand};
use tracing::{info, error};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "no-base", about = "No-Base Agent", version = VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the agent (default mode)
    Run {
        /// Path to config file
        #[arg(short, long, default_value = "/etc/no-base/agent.toml")]
        config: String,
    },
    /// Start the agent service
    Start,
    /// Stop the agent service
    Stop,
    /// Restart the agent service
    Restart,
    /// Show agent service status
    Status,
    /// View agent logs
    Logs {
        /// Number of recent log lines to show (default: follow live)
        #[arg(short = 'n', long)]
        lines: Option<u32>,
        /// Follow live log output (default when --lines is not set)
        #[arg(short, long)]
        follow: bool,
    },
    /// Enable agent to start on boot
    Enable,
    /// Disable agent from starting on boot
    Disable,
    /// Self-update: download latest binary from backend and restart service
    Update {
        /// Path to config file (to read backend_url)
        #[arg(short, long, default_value = "/etc/no-base/agent.toml")]
        config: String,
    },
    /// Uninstall: stop service, remove binary, config, and systemd unit
    Uninstall,
    /// Print version information
    Version,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { config: config_path } => {
            let cfg = match config::AgentConfig::load(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to load config from {config_path}: {e}");
                    std::process::exit(1);
                }
            };

            // Initialize logging from config (supports "text" or "json" format)
            let filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cfg.logging.level));
            if cfg.logging.format == "json" {
                tracing_subscriber::fmt().json().with_env_filter(filter).init();
            } else {
                tracing_subscriber::fmt().with_env_filter(filter).init();
            }

            info!("No-Base Agent v{VERSION} starting...");

            info!(
                gateway = %cfg.connection.gateway_url,
                "Connecting to gateway"
            );

            // Run the main connection loop (reconnects automatically)
            if let Err(e) = connection::run_connection_loop(cfg, config_path).await {
                error!("Agent terminated with error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Start => {
            run_systemctl(&["start", "no-base"], "Agent started");
        }
        Commands::Stop => {
            run_systemctl(&["stop", "no-base"], "Agent stopped");
        }
        Commands::Restart => {
            run_systemctl(&["restart", "no-base"], "Agent restarted");
        }
        Commands::Status => {
            let status = std::process::Command::new("systemctl")
                .args(["status", "no-base"])
                .status();
            std::process::exit(status.map(|s| s.code().unwrap_or(1)).unwrap_or(1));
        }
        Commands::Logs { lines, follow } => {
            let mut args = vec!["-u", "no-base"];
            let lines_str;
            if let Some(n) = lines {
                lines_str = format!("{n}");
                args.push("-n");
                args.push(&lines_str);
                if follow {
                    args.push("-f");
                }
            } else {
                // Default to follow mode when no --lines specified
                args.push("-f");
            }
            let status = std::process::Command::new("journalctl")
                .args(&args)
                .status();
            std::process::exit(status.map(|s| s.code().unwrap_or(1)).unwrap_or(1));
        }
        Commands::Enable => {
            run_systemctl(&["enable", "no-base"], "Agent enabled on boot");
        }
        Commands::Disable => {
            run_systemctl(&["disable", "no-base"], "Agent disabled on boot");
        }
        Commands::Update { config: config_path } => {
            eprintln!("No-Base Agent — Self-Update");
            eprintln!();

            // Load config to get backend_url
            let cfg = match config::AgentConfig::load(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to load config: {e}");
                    std::process::exit(1);
                }
            };

            if !nix::unistd::geteuid().is_root() {
                eprintln!("  ✗ This command must be run as root (use sudo)");
                std::process::exit(1);
            }

            eprintln!("  Current version: v{VERSION}");
            eprintln!("  Backend:         {}", cfg.connection.backend_url);
            eprintln!();

            match updater::check_and_update(&cfg.connection.backend_url, true).await {
                Ok(updater::UpdateResult::AlreadyUpToDate) => {
                    eprintln!("  ✓ Already up-to-date");
                }
                Ok(updater::UpdateResult::Updated { .. }) => {
                    eprintln!("  ✓ Update complete! Service is restarting.");
                }
                Ok(updater::UpdateResult::NotConfigured) => {
                    eprintln!("  ✗ backend_url is not set in config. Re-run the install script.");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("  ✗ Update failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::Uninstall => {
            eprintln!("No-Base Agent — Uninstall");
            eprintln!();

            if let Err(e) = uninstall().await {
                eprintln!("Uninstall failed: {e}");
                std::process::exit(1);
            }
        }
        Commands::Version => {
            println!("no-base v{VERSION}");
            println!("OS: {} {}", std::env::consts::OS, std::env::consts::ARCH);
        }
    }
}

/// Helper: run a systemctl command and print a status message.
fn run_systemctl(args: &[&str], success_msg: &str) {
    match std::process::Command::new("systemctl").args(args).status() {
        Ok(s) if s.success() => eprintln!("  ✓ {success_msg}"),
        Ok(s) => {
            eprintln!("  ✗ systemctl {} exited with {s}", args.join(" "));
            std::process::exit(s.code().unwrap_or(1));
        }
        Err(e) => {
            eprintln!("  ✗ Failed to run systemctl: {e}");
            std::process::exit(1);
        }
    }
}

/// Uninstall: stop service, remove binary, config dir, systemd unit, logs, and metrics DB.
async fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    if !nix::unistd::geteuid().is_root() {
        return Err("This command must be run as root (use sudo)".into());
    }

    eprintln!("  This will completely remove the No-Base Agent.");
    eprintln!();

    // Step 1: Stop and disable the systemd service
    let _ = std::process::Command::new("systemctl")
        .args(["stop", "no-base"])
        .status();
    eprintln!("  ✓ Service stopped");

    let _ = std::process::Command::new("systemctl")
        .args(["disable", "no-base", "--quiet"])
        .status();

    // Step 2: Remove systemd unit file
    let service_path = "/etc/systemd/system/no-base.service";
    if std::path::Path::new(service_path).exists() {
        std::fs::remove_file(service_path)?;
        let _ = std::process::Command::new("systemctl")
            .args(["daemon-reload"])
            .status();
        eprintln!("  ✓ Systemd service removed");
    }

    // Step 3: Remove configuration directory
    let config_dir = "/etc/no-base";
    if std::path::Path::new(config_dir).exists() {
        std::fs::remove_dir_all(config_dir)?;
        eprintln!("  ✓ Configuration removed ({config_dir})");
    }

    // Step 4: Remove log file
    let log_file = "/var/log/no-base.log";
    if std::path::Path::new(log_file).exists() {
        let _ = std::fs::remove_file(log_file);
        eprintln!("  ✓ Log file removed");
    }

    // Step 5: Remove metrics database
    let metrics_db = "/var/lib/no-base/metrics.db";
    if std::path::Path::new(metrics_db).exists() {
        let _ = std::fs::remove_file(metrics_db);
        // Remove parent dir if empty
        let _ = std::fs::remove_dir("/var/lib/no-base");
        eprintln!("  ✓ Metrics database removed");
    }

    // Step 6: Remove the old standalone uninstall script if it exists
    let old_uninstall = "/usr/local/bin/no-base-uninstall";
    if std::path::Path::new(old_uninstall).exists() {
        let _ = std::fs::remove_file(old_uninstall);
        eprintln!("  ✓ Legacy uninstall script removed");
    }

    // Step 7: Remove binary (this is us — do it last)
    let binary_path = "/usr/local/bin/no-base";
    if std::path::Path::new(binary_path).exists() {
        std::fs::remove_file(binary_path)?;
        eprintln!("  ✓ Binary removed");
    }

    eprintln!();
    eprintln!("  No-Base Agent has been completely removed.");
    Ok(())
}
