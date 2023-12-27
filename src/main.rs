use std::{env, fs, path::PathBuf, str::FromStr};

use color_eyre::eyre::{eyre, Context, Result};

use clap::{Parser, Subcommand};
use daemon::{RestartOption, Service};
use protocol::Packet;
use tokio::net::UnixStream;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::EnvFilter;

mod daemon;
pub mod protocol;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Starts the daemon
    Daemon {
        /// Print debug info
        #[cfg(debug_assertions)] // if not release, enable debug by default
        #[arg(
            short,
            long,
            default_missing_value("true"),
            default_value("true"),
            num_args(0..=1),
            require_equals(true),
            action = clap::ArgAction::Set
        )]
        verbose: bool,

        /// Print debug info
        #[cfg(not(debug_assertions))] // if release, disable debug by default
        #[arg(
            short,
            long,
            default_missing_value("true"),
            default_value("false"),
            num_args(0..=1),
            require_equals(true),
            action = clap::ArgAction::Set
        )]
        verbose: bool,
    },

    /// Starts a basic service provided a /bin/sh command
    Command {
        /// Command to run with /bin/sh
        command: String,

        /// Set a name for the process
        #[arg(short, long)]
        name: Option<String>,

        /// Set a working directory for the process
        #[arg(short, long)]
        cwd: Option<PathBuf>,

        /// Set the option for when the process should restart
        #[arg(short, long)]
        restart: Option<RestartOption>,

        /// Set log file for the process
        #[arg(short, long)]
        log_file: Option<PathBuf>,
    },
}

pub fn init_logger(level: Option<tracing::Level>) -> Result<WorkerGuard> {
    let log_dir = directories::BaseDirs::new()
        .ok_or(eyre!("couldn't init basedirs"))
        .context("init logger error")?
        .data_dir()
        .join("rsm/logs");

    fs::create_dir_all(&log_dir)?;

    let (file_appender, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(log_dir, "rsm.log"));

    let filter = level
        .map(|level| EnvFilter::from_default_env().add_directive(level.into()))
        .unwrap_or({
            #[cfg(debug_assertions)]
            let filter = EnvFilter::from_default_env().add_directive(tracing::Level::TRACE.into());

            #[cfg(not(debug_assertions))]
            let filter = EnvFilter::from_default_env();

            filter
        });

    let collector = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::Layer::new()
                .with_ansi(true)
                .with_writer(std::io::stdout),
        )
        .with(
            tracing_subscriber::fmt::Layer::new()
                .with_ansi(false)
                .with_writer(file_appender),
        );

    tracing::subscriber::set_global_default(collector)
        .context("Unable to set a global collector")?;

    Ok(guard)
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon { verbose } => {
            daemon::main(match verbose {
                true => Some(tracing::Level::DEBUG),
                false => Some(tracing::Level::INFO),
            })
            .await?
        }
        Commands::Command {
            command,
            name,
            cwd,
            restart,
            log_file,
        } => {
            let mut connection = UnixStream::connect(protocol::SOCKET_PATH.as_path()).await?;
            let service = Service {
                name: name.unwrap_or(command.chars().take(12).collect()),
                executable_path: PathBuf::from_str("/bin/sh")?,
                arguments: vec!["-c".to_owned(), command],
                env_vars: vec![],
                working_directory: cwd.unwrap_or(env::current_dir()?),
                restart_option: restart.unwrap_or_default(),
                log_file,
            };

            Packet::AddService(service)
                .build_and_write(&mut connection)
                .await?;
        }
    };

    Ok(())
}
