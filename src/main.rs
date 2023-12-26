use std::{env, fs, os::unix::net::UnixStream, path::PathBuf, str::FromStr};

use color_eyre::eyre::{eyre, Context, Result};

use clap::{Parser, Subcommand};
use daemon::{RestartOption, Service};
use protocol::Packet;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
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
    Daemon {},

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

pub fn init_logger() -> Result<WorkerGuard> {
    let log_dir = directories::BaseDirs::new()
        .ok_or(eyre!("couldn't init basedirs"))
        .context("init logger error")?
        .data_dir()
        .join("rsm/logs");
    fs::create_dir_all(&log_dir)?;
    let (file_appender, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(log_dir, "rsm.log"));

    let collector = tracing_subscriber::registry()
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::TRACE.into()))
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

fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon {} => daemon::main()?,
        Commands::Command {
            command,
            name,
            cwd,
            restart,
            log_file,
        } => {
            let mut connection = UnixStream::connect(protocol::SOCKET_PATH.as_path())?;
            let service = Service {
                name: name.unwrap_or(command.chars().take(12).collect()),
                executable_path: PathBuf::from_str("/bin/sh")?,
                arguments: vec!["-c".to_owned(), command],
                env_vars: vec![],
                working_directory: cwd.unwrap_or(env::current_dir()?),
                restart_option: restart.unwrap_or_default(),
                log_file,
            };

            Packet::AddService(service).build_and_write(&mut connection)?;
        }
    };

    Ok(())
}
