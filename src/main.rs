use std::time::SystemTime;
use std::{env, fs, path::PathBuf, str::FromStr};

use color_eyre::eyre::{eyre, Context, Report, Result};

use clap::{Parser, Subcommand};
use daemon::service_threads::ServiceState;
use daemon::{RestartOption, Service};
use humantime::format_duration;
use once_cell::sync::Lazy;
use protocol::Packet;
use tabled::{Table, Tabled};
use tokio::net::UnixStream;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::EnvFilter;

use crate::daemon::service_threads::ServiceThreadCommand;

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

    /// Get status of all services
    Status {}, // TODO: add service specific status?

    /// Stop a running service
    Stop {
        /// Name of service to stop
        name: String,
    },

    /// Start a stopped service
    Start {
        /// Name of service to start
        name: String,
    },

    /// Remove a service
    Remove {
        /// Name of service to remove
        name: String,
    },
    // TODO: Toml file load and kill
}

pub static APP_FILES_DIR: Lazy<PathBuf> = Lazy::new(|| {
    let dir = directories::BaseDirs::new()
        .ok_or(eyre!("couldn't init basedirs"))
        .context("init logger error")
        .unwrap()
        .data_dir()
        .join("rsm");
    fs::create_dir_all(&dir)
        .context("creating app files dir failed")
        .unwrap();
    dir
});

pub fn init_logger() -> Result<WorkerGuard> {
    let log_dir = APP_FILES_DIR.join("logs");

    fs::create_dir_all(&log_dir)?;

    let (file_appender, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(log_dir, "rsm.log"));

    #[cfg(debug_assertions)]
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::TRACE.into())
        .from_env()?;

    #[cfg(not(debug_assertions))]
    let filter = EnvFilter::from_default_env();

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

async fn run_command_wrapper(
    stream: &mut UnixStream,
    (name, command): (String, ServiceThreadCommand),
) -> Result<Result<ServiceState, String>> {
    Packet::RunCommand(name, command)
        .build_and_write(stream)
        .await?;

    let Ok(Packet::RunCommandResponse(response)) = Packet::from_stream(stream).await else {
        return Err(eyre!("failed reading response packet"));
    };

    Ok(response)
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon {} => daemon::main().await?,
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

            let Ok(Packet::AddServiceResponse(response)) =
                Packet::from_stream(&mut connection).await
            else {
                return Err(eyre!("failed reading response packet"));
            };

            response.map_err(|err| Report::msg(err))?;
            println!("Successfully added service");
        }
        Commands::Status {} => {
            let mut connection = UnixStream::connect(protocol::SOCKET_PATH.as_path()).await?;

            Packet::ServicesInfo()
                .build_and_write(&mut connection)
                .await?;

            let Ok(Packet::ServicesInfoResponse(response)) =
                Packet::from_stream(&mut connection).await
            else {
                return Err(eyre!("failed reading response packet"));
            };

            let service_info = match response {
                Ok(x) => x,
                Err(e) => Err(eyre!(e)).context("couldn't get services_info")?,
            };

            #[derive(Tabled)]
            struct ServiceColumn {
                name: String,
                executable: String,
                restarts: String,
                alive_for: String,
                pid: String,
                starts: usize,
                stopped: bool,
            }

            let table_data: Vec<_> = service_info
                .iter()
                .map(|(service, state)| ServiceColumn {
                    name: service.name.clone(),
                    executable: service.executable_path.to_str().unwrap().to_owned(),
                    restarts: format!("{:?}", service.restart_option),
                    alive_for: state
                        .alive_since
                        .map(|x| {
                            format_duration(SystemTime::now().duration_since(x).unwrap())
                                .to_string()
                        })
                        .unwrap_or("dead".to_owned()),
                    pid: match state.pid {
                        Some(x) => x.to_string(),
                        None => "None".to_owned(),
                    },
                    starts: state.starts,
                    stopped: state.explicitly_stopped,
                })
                .collect();

            let mut table = Table::new(&table_data);
            table.with(tabled::settings::Style::rounded());
            println!("{}", table.to_string())
        }
        Commands::Stop { name } => {
            let mut connection = UnixStream::connect(protocol::SOCKET_PATH.as_path()).await?;
            let response: Result<ServiceState> =
                run_command_wrapper(&mut connection, (name, ServiceThreadCommand::Stop))
                    .await?
                    .map_err(|err| color_eyre::eyre::Error::msg(err));

            let state = response.context("command failed")?;

            println!("{:?}", state);
        }
        Commands::Start { name } => {
            let mut connection = UnixStream::connect(protocol::SOCKET_PATH.as_path()).await?;
            let response: Result<ServiceState> =
                run_command_wrapper(&mut connection, (name, ServiceThreadCommand::Start))
                    .await?
                    .map_err(|err| color_eyre::eyre::Error::msg(err));

            let state = response.context("command failed")?;

            println!("{:?}", state);
        }
        Commands::Remove { name } => {
            let mut connection = UnixStream::connect(protocol::SOCKET_PATH.as_path()).await?;
            let response: Result<ServiceState> = run_command_wrapper(
                &mut connection,
                (name.clone(), ServiceThreadCommand::Remove),
            )
            .await?
            .map_err(|err| color_eyre::eyre::Error::msg(err));

            response?;

            println!("Removed service `{}`", name);
        }
    };

    Ok(())
}
