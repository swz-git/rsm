use std::{
    fs,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    process::{Command, Stdio},
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
    /// default is /tmp/rsm-[NAME]-[PID].log
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

static GLOBAL_THREAD_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct ServiceThread {
    service: Arc<Mutex<Service>>,
    sender: Sender<ServiceThreadCommand>,
    receiver: Receiver<Result<()>>,
    id: usize,
}

impl ServiceThread {
    fn add_to(pool: Arc<Mutex<Vec<ServiceThread>>>, service: Service) {
        let (command_sender, command_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let arc_mutex_service = Arc::new(Mutex::new(service));

        let pool = pool.clone();
        let pool_for_thread = pool.clone();

        let id = GLOBAL_THREAD_COUNT.fetch_add(1, Ordering::SeqCst);

        let service_for_thread = arc_mutex_service.clone();

        thread::spawn(move || {
            let (sender, receiver) = (result_sender, command_receiver);
            let pool = pool_for_thread;
            let service_mutex = service_for_thread;

            let process_template = {
                let service = service_mutex.lock();
                Command::new(service.executable_path.clone())
                    .args(service.arguments.clone())
                    .envs(service.env_vars.clone())
                    .current_dir(service.working_directory.clone())
                    .stderr(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stdin(Stdio::null())
            };

            // TODO: handle logging!

            // TODO: run process
            let process = todo!();

            // TODO: here we should check for receiver input and if process has exited
            loop {
                thread::sleep(Duration::from_millis(1000));
                break;
            }

            // if we get to this point, thread is exiting and should be removed from pool
            pool.lock().retain(|x| x.id != id);
        });

        pool.lock().push(ServiceThread {
            service: arc_mutex_service.clone(),
            sender: command_sender,
            receiver: result_receiver,
            id,
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
    print!(include_str!("../../banner.ansi"));
    info!("Starting daemon");

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
