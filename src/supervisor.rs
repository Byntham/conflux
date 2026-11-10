use std::io::Read;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};

const RESTART_AFTER: Duration = Duration::from_secs(3);

pub fn run() -> Result<()> {
    let exe = std::env::current_exe().context("find the conflux binary")?;
    loop {
        let (mut workers_alive, lifeline) = std::io::pipe().context("create the worker lifeline")?;
        let spawned = Command::new(&exe).arg("worker").stdin(lifeline).spawn();
        match spawned {
            Ok(mut worker) => {
                std::thread::spawn(move || {
                    let _ = worker.wait();
                });
                let _ = workers_alive.read_to_end(&mut Vec::new());
                tracing::warn!("every conflux worker stopped; starting a new one");
            }
            Err(e) => tracing::error!("could not start the conflux worker: {e}"),
        }
        std::thread::sleep(RESTART_AFTER);
    }
}
