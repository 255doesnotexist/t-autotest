pub mod recorder;

use clap::{Parser, Subcommand};
use std::{env, io::IsTerminal, path::Path};
use t_binding::api::{Api, RustApi};
use t_config::Config;
use t_runner::{DriverForScript, ServerBuilder};
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

#[derive(clap::Parser, Debug)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Run {
        #[clap(short, long)]
        config: String,
        #[clap(short, long)]
        script: String,
    },
    Record {
        #[clap(short, long)]
        config: Option<String>,
    },
    VncDo {
        #[clap(short, long)]
        config: String,
        #[command(subcommand)]
        action: VNCAction,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum VNCAction {
    Move { x: u16, y: u16 },
    Click,
    RClick,
}

fn main() {
    let format = tracing_subscriber::fmt::format()
        .without_time()
        .with_target(false)
        .with_level(true)
        .with_ansi(std::io::stdout().is_terminal())
        .with_source_location(true)
        .compact();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(match env::var("RUST_LOG") {
            Ok(l) => match l.as_str() {
                "trace" => Level::TRACE,
                "debug" => Level::DEBUG,
                "warn" => Level::WARN,
                "error" => Level::ERROR,
                _ => Level::INFO,
            },
            _ => Level::INFO,
        })
        .event_format(format)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let cli = Cli::parse();
    info!(msg = "current cli", cli = ?cli);

    match cli.command {
        Commands::Run { script, config } => {
            // init config
            let config = Config::from_toml_file(config.as_str()).expect("config not valid");
            info!(msg = "current config", config = ?config);

            let ext = Path::new(script.as_str())
                .extension()
                .unwrap()
                .to_string_lossy()
                .to_string();

            match DriverForScript::new_with_engine(config, ext.as_str()) {
                Ok(mut d) => {
                    d.start().run_file(script).stop();
                }
                Err(e) => {
                    error!(msg = "Driver init failed", reason = ?e)
                }
            }
        }
        Commands::Record { config } => {
            let config =
                config.map(|c| Config::from_toml_file(c.as_str()).expect("config not valid"));
            info!(msg = "current config", config = ?config);

            let builder = ServerBuilder::new(config);
            match builder.build() {
                Ok((server, tx, stop_tx)) => {
                    server.start_non_blocking();
                    recorder::RecorderBuilder::new(stop_tx, tx).build().start();
                }
                Err(e) => {
                    error!(msg = "Driver init failed", reason = ?e)
                }
            }
        }
        Commands::VncDo { action, config } => {
            // init config
            let mut config = Config::from_toml_file(config.as_str()).expect("config not valid");
            info!(msg = "current config", config = ?config);

            config.ssh = None;
            config.serial = None;
            let builder = ServerBuilder::new(Some(config));
            match builder.build() {
                Ok((server, tx, stop_tx)) => {
                    let api = RustApi::new(tx);
                    server.start_non_blocking();
                    if let Err(e) = match action {
                        VNCAction::Move { x, y } => api.vnc_mouse_move(x, y),
                        VNCAction::Click => api.vnc_mouse_click(),
                        VNCAction::RClick => api.vnc_mouse_rclick(),
                    } {
                        error!(msg = "do vnc action failed", reason=?e);
                    }
                    if let Err(e) = stop_tx.send(()) {
                        error!(msg = "server stop failed", reason=?e);
                    }
                }
                Err(e) => {
                    error!(msg = "Driver init failed", reason = ?e)
                }
            }
        }
    }
}
