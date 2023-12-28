use color_eyre::eyre::{eyre, Context, Result};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};
use tracing::{debug, info, warn};

use tokio::{
    process::Child,
    sync::{mpsc, Mutex},
};

use crate::daemon::RestartOption;

use super::Service;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServiceThreadCommand {
    Start,
    Stop,
    Remove,
    Status,
    Kill,
}

impl ServiceThreadCommand {
    pub async fn send(&self, thread: &mut ServiceThread) -> Result<ServiceState> {
        thread
            .sender
            .send(self.clone())
            .await
            .context("couldn't send ServiceThreadCommand")?;
        Ok(thread
            .receiver
            .recv()
            .await
            .ok_or(eyre!("channel closed"))
            .context("ServiceThreadCommand response failed")?)
    }
}

static GLOBAL_THREAD_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceState {
    /// None => is_alive = false, Some => is_alive = true
    pub alive_since: Option<SystemTime>,
    pub starts: usize,
    pub explicitly_stopped: bool,
}

#[derive(Debug)]
pub struct ServiceThread {
    service: Arc<Mutex<Service>>,
    pub sender: mpsc::Sender<ServiceThreadCommand>,
    pub receiver: mpsc::Receiver<ServiceState>,
    id: usize,
}

async fn service_thread(
    sender: mpsc::Sender<ServiceState>,
    mut receiver: mpsc::Receiver<ServiceThreadCommand>,
    pool: Arc<Mutex<Vec<ServiceThread>>>,
    service_mutex: Arc<Mutex<Service>>,
    thread_id: usize,
) -> Result<()> {
    let mut process = {
        let service = service_mutex.lock().await;
        service.spawn_child().await?
    };

    let mut state = ServiceState {
        alive_since: Some(SystemTime::now()),
        starts: 1,
        explicitly_stopped: false,
    };

    // TODO: here we should check for receiver input and if process has exited
    loop {
        async fn handle_restart(
            process: &mut Child,
            state: &mut ServiceState,
            service_mutex: Arc<Mutex<Service>>,
        ) -> Result<()> {
            // Restart if process has exited according to restart_option
            let maybe_code = process.try_wait().unwrap_or(None);
            if let Some(successful_exit) = maybe_code.map(|x| x.success()) {
                let restart_option = { service_mutex.lock().await.restart_option.clone() };
                let mut should_restart = match successful_exit {
                    // match {} to drop lock asap, since scope ends
                    true => match restart_option {
                        RestartOption::Always | RestartOption::UnlessStopped => true,
                        RestartOption::OnCrash | RestartOption::Never => false,
                    },
                    false => match restart_option {
                        RestartOption::Always
                        | RestartOption::UnlessStopped
                        | RestartOption::OnCrash => true,
                        RestartOption::Never => false,
                    },
                };

                if state.explicitly_stopped && restart_option != RestartOption::Always {
                    should_restart = false
                }

                let service_name = { service_mutex.lock().await.name.clone() };
                debug!(
                    "Process for service {} exited with code {:?} (pid: {:?})",
                    &service_name,
                    maybe_code,
                    process.id()
                );

                if should_restart {
                    debug!("Restarting process for service {}", &service_name);
                    *state = ServiceState {
                        alive_since: Some(SystemTime::now()),
                        starts: state.starts + 1,
                        explicitly_stopped: false,
                    };

                    *process = {
                        let service = service_mutex.lock().await;
                        service.spawn_child().await?
                    }
                }
            }

            Ok(())
        }

        // TODO: more debug logging

        tokio::select! {
            _ = async { // TODO: how does cancelling work here?
                let _ = process.wait().await;
                state.alive_since = None;
                // don't restart instantly to avoid burning cpu
                tokio::time::sleep(Duration::from_millis(500)).await;
            } => {
                handle_restart(&mut process, &mut state, service_mutex.clone()).await?
            },
            Some(msg) = receiver.recv() => {
                match msg {
                    ServiceThreadCommand::Start => {
                        state.explicitly_stopped = false;
                        if process.try_wait()?.is_some() {
                            state.alive_since = Some(SystemTime::now());
                            state.starts = state.starts + 1;

                            process = {
                                let service = service_mutex.lock().await;
                                service.spawn_child().await?
                            }
                        }

                        sender.send(state.clone()).await?;
                    },
                    ServiceThreadCommand::Stop => {
                        state.explicitly_stopped = true;
                        if process.try_wait()?.is_none() {
                            let pid = process.id().ok_or(eyre!("couldn't find pid")).context("failed killing process (service)")?;
                            nix::sys::signal::kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGTERM)?;
                            tokio::select! {
                                _ = tokio::time::sleep(Duration::from_millis(1000)) => {process.kill().await?}
                                _ = process.wait() => {}
                            }
                        }
                        state.alive_since = None;

                        sender.send(state.clone()).await?;
                    }
                    ServiceThreadCommand::Remove => {
                        state.explicitly_stopped = true;
                        if process.try_wait()?.is_none() {
                            let pid = process.id().ok_or(eyre!("couldn't find pid")).context("failed killing process (service)")?;
                            nix::sys::signal::kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGTERM)?;
                            tokio::select! {
                                _ = tokio::time::sleep(Duration::from_millis(1000)) => {process.kill().await?}
                                _ = process.wait() => {}
                            }
                        }
                        state.alive_since = None;

                        sender.send(state.clone()).await?;
                        break;
                    }
                    ServiceThreadCommand::Kill => {
                        state.explicitly_stopped = true;
                        if process.try_wait()?.is_none() {
                            process.kill().await?
                        }
                        state.alive_since = None;

                        sender.send(state.clone()).await?;
                    }
                    ServiceThreadCommand::Status => {
                        sender.send(state.clone()).await?;
                    }
                }
            }
        }
    }

    // if we get to this point, thread is exiting and should be removed from pool
    pool.lock().await.retain(|x| x.id != thread_id);

    let service_name = { service_mutex.lock().await.name.clone() };

    info!("Removed service with name {}", service_name);

    Ok(())
}

impl ServiceThread {
    pub async fn add_to(pool: Arc<Mutex<Vec<ServiceThread>>>, service: Service) {
        let (command_sender, command_receiver) = mpsc::channel(1);
        let (result_sender, result_receiver) = mpsc::channel(1);
        let arc_mutex_service = Arc::new(Mutex::new(service.clone()));

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

        info!("Added service with name {}", service.name);
    }
    pub async fn clone_service(&self) -> Service {
        (*self.service.lock().await).clone()
    }
}
