use std::{
    env, fs,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::Duration,
};

use clap::ValueEnum;
use color_eyre::eyre::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

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
    sender: Sender<ServiceThreadCommand>,
    receiver: Receiver<Result<()>>,
    id: usize,
}

fn spawn_service(service: &Service) -> Result<Child> {
    let log_file_path = service
        .log_file
        .clone()
        .filter(|x| x.exists())
        .unwrap_or(env::temp_dir().join(format!("rsm-{}.log", service.name)));

    let out_file = fs::File::options()
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

fn service_thread(
    sender: Sender<Result<()>>,
    receiver: Receiver<ServiceThreadCommand>,
    pool: Arc<Mutex<Vec<ServiceThread>>>,
    service_mutex: Arc<Mutex<Service>>,
    thread_id: usize,
) -> Result<()> {
    let mut process = {
        let service = service_mutex.lock();
        spawn_service(&service)?
    };

    // TODO: here we should check for receiver input and if process has exited
    loop {
        // Don't murder cpu
        thread::sleep(Duration::from_millis(200));

        // Restart if process has exited according to restart_option
        if let Some(successful_exit) = process.try_wait().unwrap_or(None).map(|x| x.success()) {
            let should_restart = match successful_exit {
                // match {} to drop lock asap, since scope ends
                true => match { service_mutex.lock().restart_option.clone() } {
                    RestartOption::Always | RestartOption::UnlessStopped => true,
                    RestartOption::OnCrash | RestartOption::Never => false,
                },
                false => match { service_mutex.lock().restart_option.clone() } {
                    RestartOption::Always
                    | RestartOption::UnlessStopped
                    | RestartOption::OnCrash => true,
                    RestartOption::Never => false,
                },
            };
            if should_restart {
                process = {
                    let service = service_mutex.lock();
                    spawn_service(&service)?
                }
            }
        }

        // Check for commands
    }

    // if we get to this point, thread is exiting and should be removed from pool
    pool.lock().retain(|x| x.id != thread_id);

    Ok(())
}

impl ServiceThread {
    fn add_to(pool: Arc<Mutex<Vec<ServiceThread>>>, service: Service) {
        let (command_sender, command_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let arc_mutex_service = Arc::new(Mutex::new(service));

        let pool = pool.clone();

        let thread_id = GLOBAL_THREAD_COUNTER.fetch_add(1, Ordering::SeqCst);

        let pool_for_thread = pool.clone();
        let service_for_thread = arc_mutex_service.clone();

        let service_name = { arc_mutex_service.clone().lock().name.clone() };

        thread::spawn(move || {
            service_thread(
                result_sender,
                command_receiver,
                pool_for_thread,
                service_for_thread,
                thread_id,
            )
            .map_err(|e| warn!("Service thread for `{}` crashed:{e:?}", service_name))
        });

        pool.lock().push(ServiceThread {
            service: arc_mutex_service.clone(),
            sender: command_sender,
            receiver: result_receiver,
            id: thread_id,
        });
    }
    fn clone_service(&self) -> Service {
        (*self.service.lock()).clone()
    }
}

fn handle_client(
    mut stream: UnixStream,
    service_threads: Arc<Mutex<Vec<ServiceThread>>>,
) -> Result<()> {
    loop {
        let Ok(packet) = Packet::from_stream(&mut stream) else {
            break; // If parsing the packet failed, client has probably disconnected
        };
        // TODO: more packets, stop, start, remove etc
        match packet {
            Packet::AddService(service) => {
                ServiceThread::add_to(service_threads.clone(), service);
            }
        }
    }
    Ok(())
}

pub fn main() -> Result<()> {
    let _guard = crate::init_logger()?;

    let start_log = "Starting daemon\n".to_owned() + include_str!("../../banner.ansi");
    info!("{start_log}");

    // TODO: removing file doesn't crash other instances
    if crate::protocol::SOCKET_PATH.exists() {
        fs::remove_file(crate::protocol::SOCKET_PATH.as_path())
            .context("failed to remove previous socket")?;
    }

    let listener = UnixListener::bind(crate::protocol::SOCKET_PATH.as_path())?;

    info!("Listening...");

    // TODO: save services to disk on update and recover on startup

    let mut service_threads: Arc<Mutex<Vec<ServiceThread>>> = Arc::new(Mutex::new(vec![]));

    for (i, stream) in listener.incoming().enumerate() {
        match stream {
            Ok(stream) => {
                // info!("Connection accepted, connection thread {} spawned", i);

                let clone = service_threads.clone();

                // Spawn a new thread to handle the client
                thread::spawn(move || match handle_client(stream, clone) {
                    Ok(_) => {
                        // info!("Connection thread {i} exited")
                    }
                    Err(e) => {
                        warn!("Thread {i} crashed:{e:?}");
                    }
                });
            }
            Err(err) => {
                error!("Error accepting connection: {}", err);
                break;
            }
        }
    }

    Ok(())
}
