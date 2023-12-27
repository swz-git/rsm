use std::{
    env,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use clap::ValueEnum;
use color_eyre::eyre::{anyhow, eyre, Context, Report, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{mpsc, Mutex},
};
use tracing::{debug, error, info, warn};

use crate::protocol::Packet;

#[derive(Debug, Clone, ValueEnum, Serialize, Deserialize)]
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
    fn start(&mut self) -> Result<()> {
        let process = Command::new(&self.executable_path)
            .args(&self.arguments)
            .current_dir(&self.working_directory)
            .envs(self.env_vars.clone());
        // spawn thread to watch quit?
        todo!()
    }
}

enum ServiceThreadCommand {
    Start,
    Stop,
    Remove,
    // TODO: kill?
}

static GLOBAL_THREAD_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct ServiceThread {
    service: Arc<Mutex<Service>>,
    sender: mpsc::UnboundedSender<ServiceThreadCommand>,
    receiver: mpsc::UnboundedReceiver<Result<()>>,
    id: usize,
}

async fn spawn_service(service: &Service) -> Result<Child> {
    let log_file_path = service
        .log_file
        .clone()
        .filter(|x| x.exists())
        .unwrap_or(env::temp_dir().join(format!("rsm-{}.log", service.name)));

    // tokio File doesn't implement into stdio
    let out_file = std::fs::File::options()
        .create(true)
        .write(true)
        .append(true)
        .open(log_file_path)
        .context("failed to open log file with write permission")?;

    let process = Command::new(service.executable_path.clone())
        .args(service.arguments.clone())
        .envs(service.env_vars.clone())
        .current_dir(service.working_directory.clone())
        .stderr(out_file.try_clone()?)
        .stdout(out_file)
        .stdin(Stdio::null())
        .spawn()?;

    Ok(process)
}

async fn service_thread(
    mut sender: mpsc::UnboundedSender<Result<()>>,
    mut receiver: mpsc::UnboundedReceiver<ServiceThreadCommand>,
    pool: Arc<Mutex<Vec<ServiceThread>>>,
    service_mutex: Arc<Mutex<Service>>,
    thread_id: usize,
) -> Result<()> {
    let mut process = {
        let service = service_mutex.lock().await;
        spawn_service(&service).await?
    };

    // TODO: here we should check for receiver input and if process has exited
    loop {
        async fn handle_restart(
            process: &mut Child,
            service_mutex: Arc<Mutex<Service>>,
        ) -> Result<()> {
            // Restart if process has exited according to restart_option
            if let Some(successful_exit) = process.try_wait().unwrap_or(None).map(|x| x.success()) {
                let should_restart = match successful_exit {
                    // match {} to drop lock asap, since scope ends
                    true => match { service_mutex.lock().await.restart_option.clone() } {
                        RestartOption::Always | RestartOption::UnlessStopped => true,
                        RestartOption::OnCrash | RestartOption::Never => false,
                    },
                    false => match { service_mutex.lock().await.restart_option.clone() } {
                        RestartOption::Always
                        | RestartOption::UnlessStopped
                        | RestartOption::OnCrash => true,
                        RestartOption::Never => false,
                    },
                };
                if should_restart {
                    *process = {
                        let service = service_mutex.lock().await;
                        spawn_service(&service).await?
                    }
                }
            }

            Ok(())
        }

        /// returns true if thread should delete itself
        async fn handle_msg(cmd: ServiceThreadCommand) -> bool {
            todo!("commands");
            // break;
        }

        tokio::select! {
            _ = async {
                let _ = process.wait().await;
                // don't restart instantly to avoid burning cpu
                tokio::time::sleep(Duration::from_millis(1000)).await;
            } => handle_restart(&mut process, service_mutex.clone()).await?,
            Some(msg) = receiver.recv() => match handle_msg(msg).await {true => break, _ => {}}
        }
    }

    // if we get to this point, thread is exiting and should be removed from pool
    pool.lock().await.retain(|x| x.id != thread_id);

    Ok(())
}

impl ServiceThread {
    async fn add_to(pool: Arc<Mutex<Vec<ServiceThread>>>, service: Service) {
        let (command_sender, command_receiver) = mpsc::unbounded_channel();
        let (result_sender, result_receiver) = mpsc::unbounded_channel();
        let arc_mutex_service = Arc::new(Mutex::new(service));

        let pool = pool.clone();

        let thread_id = GLOBAL_THREAD_COUNTER.fetch_add(1, Ordering::SeqCst);

        let pool_for_thread = pool.clone();
        let service_for_thread = arc_mutex_service.clone();

        let service_name = { arc_mutex_service.clone().lock().await.name.clone() };

        tokio::spawn(async move {
            service_thread(
                result_sender,
                command_receiver,
                pool_for_thread,
                service_for_thread,
                thread_id,
            )
            .await
            .map_err(|e| warn!("Service thread for `{}` crashed:{e:?}", service_name))
        });

        pool.lock().await.push(ServiceThread {
            service: arc_mutex_service.clone(),
            sender: command_sender,
            receiver: result_receiver,
            id: thread_id,
        });
    }
    async fn clone_service(&self) -> Service {
        (*self.service.lock().await).clone()
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
        // TODO: more packets, stop, start, remove etc
        match packet {
            Packet::AddService(service) => {
                ServiceThread::add_to(service_threads.clone(), service).await;
            }
        }
    }
    Ok(())
}

pub async fn main(tracing_level: Option<tracing::Level>) -> Result<()> {
    let _guard = crate::init_logger(tracing_level)?;

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
