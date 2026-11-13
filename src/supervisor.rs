use std::io::{Read, Write};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};

const RESTART_AFTER: Duration = Duration::from_secs(3);
const SUPERVISED: &str = "CONFLUX_SUPERVISED";
const STOP: &[u8] = b"stop";

pub fn run() -> Result<()> {
    let exe = std::env::current_exe().context("find the conflux binary")?;
    loop {
        let (mut workers_alive, lifeline) = std::io::pipe().context("create the worker lifeline")?;
        let spawned = Command::new(&exe)
            .arg("worker")
            .env(SUPERVISED, "1")
            .stdin(lifeline)
            .spawn();
        match spawned {
            Ok(mut worker) => {
                std::thread::spawn(move || {
                    let _ = worker.wait();
                });
                let mut said = Vec::new();
                let _ = workers_alive.read_to_end(&mut said);
                if said.ends_with(STOP) {
                    tracing::info!("the conflux worker left the hub; stopping");
                    return Ok(());
                }
                tracing::warn!("every conflux worker stopped; starting a new one");
            }
            Err(e) => tracing::error!("could not start the conflux worker: {e}"),
        }
        std::thread::sleep(RESTART_AFTER);
    }
}

pub fn stop_when_workers_exit() -> Result<()> {
    if std::env::var_os(SUPERVISED).is_none() {
        return Ok(());
    }
    #[cfg(unix)]
    let lifeline = std::os::fd::AsFd::as_fd(&std::io::stdin()).try_clone_to_owned()?;
    #[cfg(windows)]
    let lifeline = std::os::windows::io::AsHandle::as_handle(&std::io::stdin()).try_clone_to_owned()?;
    std::fs::File::from(lifeline)
        .write_all(STOP)
        .context("tell the supervisor to stop")
}
