use std::{env, path::PathBuf, process::Stdio, sync::Arc};

use clap::ValueEnum;
use color_eyre::eyre::{eyre, Context, OptionExt, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::Mutex,
};
use tracing::{debug, error, info, warn};

use crate::protocol::Packet;

pub mod service_threads;

use service_threads::ServiceThread;

#[derive(Debug, Clone, ValueEnum, Serialize, Deserialize, PartialEq)]
#[clap(rename_all = "kebab_case")]
pub enum RestartOption {
    Always,
    UnlessStopped,
    OnCrash,
    Never,
}

impl Default for RestartOption {
    fn default() -> Self {
        RestartOption::UnlessStopped
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    pub executable_path: PathBuf,
    pub arguments: Vec<String>,
    pub env_vars: Vec<(String, String)>,
    pub working_directory: PathBuf,
    pub restart_option: RestartOption,
    /// default is /tmp/rsm-[NAME].log
    pub log_file: Option<PathBuf>,
}

impl Service {
    async fn spawn_child(&self) -> Result<Child> {
        let log_file_path = self
            .log_file
            .clone()
            .filter(|x| x.exists())
            .unwrap_or(env::temp_dir().join(format!("rsm-{}.log", self.name)));

        // tokio File doesn't implement into stdio
        let out_file = std::fs::File::options()
            .create(true)
            .write(true)
            .append(true)
            .open(log_file_path)
            .context("failed to open log file with write permission")?;

        let process = Command::new(self.executable_path.clone())
            .args(self.arguments.clone())
            .envs(self.env_vars.clone())
            .current_dir(self.working_directory.clone())
            .stderr(out_file.try_clone()?)
            .stdout(out_file)
            .stdin(Stdio::null())
            .spawn()?;

        debug!(
            "Process for service {} started with pid: {:?}",
            self.name,
            process.id()
        );

        Ok(process)
    }
}

async fn handle_client(
    mut stream: UnixStream,
    service_threads: Arc<Mutex<Vec<ServiceThread>>>,
) -> Result<()> {
    loop {
        let Ok(packet) = Packet::from_stream(&mut stream).await else {
            break; // If parsing the packet failed, client has probably disconnected
        };

        match packet {
            Packet::AddService(service) => {
                let exists_already = {
                    let mut exists = false;
                    for thread in service_threads.clone().lock().await.iter() {
                        if thread.clone_service().await.name == service.name {
                            exists = true;
                            break;
                        }
                    }
                    exists
                };
                if exists_already {
                    Packet::AddServiceResponse(Err(
                        "service with that name already exists".to_owned()
                    ))
                    .build_and_write(&mut stream)
                    .await?;
                } else {
                    ServiceThread::add_to(service_threads.clone(), service).await;
                    Packet::AddServiceResponse(Ok(()))
                        .build_and_write(&mut stream)
                        .await?;
                }
            }
            Packet::RunCommand(service_name, command) => 'scope: {
                let mut locked = service_threads.lock().await;
                let mut maybe_thread = None;
                for thread in locked.iter_mut() {
                    if thread.clone_service().await.name == service_name {
                        maybe_thread = Some(thread)
                    }
                }

                let Some(thread) = maybe_thread else {
                    Packet::RunCommandResponse(Err("couldn't find service".to_owned()))
                        .build_and_write(&mut stream)
                        .await?;
                    break 'scope;
                };

                let response = command.send(thread).await?;

                Packet::RunCommandResponse(Ok(response))
                    .build_and_write(&mut stream)
                    .await?;
            }
            Packet::ServicesInfo() => {
                let mut services_info = vec![];

                let mut locked = service_threads.lock().await;

                for thread in locked.iter_mut() {
                    thread
                        .sender
                        .send(service_threads::ServiceThreadCommand::Status)
                        .await?;
                    services_info.push((
                        thread.clone_service().await,
                        thread
                            .receiver
                            .recv()
                            .await
                            .ok_or_eyre("couldn't read status for service")?,
                    ))
                }

                Packet::ServicesInfoResponse(Ok(services_info))
                    .build_and_write(&mut stream)
                    .await?;
            }
            Packet::RunCommandResponse(..)
            | Packet::AddServiceResponse(..)
            | Packet::ServicesInfoResponse(..) => {
                Err(eyre!("wrong way, daemon received response"))?
            }
        }
    }
    Ok(())
}

pub async fn main() -> Result<()> {
    let _guard = crate::init_logger()?;

    let start_log = "Starting daemon\n".to_owned() + include_str!("../../banner.ansi");
    info!("{start_log}");

    debug!("Debug logging enabled");

    // TODO: removing file doesn't crash other instances
    if crate::protocol::SOCKET_PATH.exists() {
        fs::remove_file(crate::protocol::SOCKET_PATH.as_path())
            .await
            .context("failed to remove previous socket")?;
    }

    let listener = UnixListener::bind(crate::protocol::SOCKET_PATH.as_path())?;

    info!("Listening...");

    // TODO: save services to disk on update and recover on startup

    let service_threads: Arc<Mutex<Vec<ServiceThread>>> = Arc::new(Mutex::new(vec![]));

    for i in 0.. {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream,addr)) => {
                        debug!("Connection accepted `{addr:?}`, connection thread {i} spawned");

                        let clone = service_threads.clone();

                        // Spawn a new thread to handle the client
                        tokio::spawn(async move {
                            match handle_client(stream, clone).await {
                                Ok(_) => {
                                    debug!("Connection thread {i} exited")
                                }
                                Err(e) => {
                                    warn!("Thread {i} crashed:{e:?}");
                                }
                            }
                        });
                    }
                    Err(err) => {
                        error!("Error accepting connection: {}", err);
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received exit signal, quitting");
                // TODO: more stuff should happen here
                break
            }
        }
    }

    Ok(())
}
