use std::{env, os::unix::net::UnixStream, path::PathBuf, str::FromStr};

use color_eyre::eyre::Result;

use clap::{Parser, Subcommand};
use daemon::{RestartOption, Service};
use protocol::Packet;

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

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt::init();

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
