use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

const HANDOFF_ENV: &str = "CONFLUX_HANDOFF";
static INVITATION_TAKEN: AtomicBool = AtomicBool::new(false);
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const DRAIN_LIMIT: Duration = Duration::from_secs(60 * 60);
const DRAIN_POLL: Duration = Duration::from_millis(100);

#[derive(Serialize, Deserialize)]
struct Invitation {
    port: u16,
    token: String,
    listeners: Vec<(SocketAddr, u64)>,
}

pub type InFlight = Arc<()>;

pub struct Lifecycle {
    draining: watch::Sender<bool>,
    in_flight: InFlight,
    listeners: Mutex<Vec<(SocketAddr, std::net::TcpListener)>>,
    inherited: Mutex<HashMap<SocketAddr, std::net::TcpListener>>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            draining: watch::channel(false).0,
            in_flight: Arc::default(),
            listeners: Mutex::default(),
            inherited: Mutex::default(),
        }
    }
}

impl Lifecycle {
    pub fn is_draining(&self) -> bool {
        *self.draining.borrow()
    }

    pub fn watch_drain(&self) -> watch::Receiver<bool> {
        self.draining.subscribe()
    }

    pub fn drain_started(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut draining = self.draining.subscribe();
        async move {
            let _ = draining.wait_for(|d| *d).await;
        }
    }

    pub fn in_flight(&self) -> InFlight {
        self.in_flight.clone()
    }

    pub fn begin_drain(&self) {
        self.draining.send_replace(true);
    }

    pub async fn drained(&self) {
        self.drain_started().await;
        let deadline = Instant::now() + DRAIN_LIMIT;
        while Arc::strong_count(&self.in_flight) > 1 {
            if Instant::now() > deadline {
                tracing::warn!("requests were still running an hour after the handoff; stopping anyway");
                return;
            }
            tokio::time::sleep(DRAIN_POLL).await;
        }
    }

    pub fn listen(&self, addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
        let inherited = self.inherited.lock().unwrap().remove(&addr);
        let listener = match inherited {
            Some(listener) => listener,
            None => std::net::TcpListener::bind(addr)?,
        };
        self.register(listener)
    }

    pub fn listen_inherited(&self) -> Vec<tokio::net::TcpListener> {
        let inherited: Vec<_> = self.inherited.lock().unwrap().drain().collect();
        inherited
            .into_iter()
            .filter_map(|(addr, listener)| match self.register(listener) {
                Ok(listener) => Some(listener),
                Err(e) => {
                    tracing::warn!(%addr, "could not take over a listener: {e}");
                    None
                }
            })
            .collect()
    }

    fn register(&self, listener: std::net::TcpListener) -> std::io::Result<tokio::net::TcpListener> {
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        self.listeners.lock().unwrap().push((addr, listener.try_clone()?));
        tokio::net::TcpListener::from_std(listener)
    }

    pub fn close_listeners(&self) {
        self.listeners.lock().unwrap().clear();
        self.inherited.lock().unwrap().clear();
    }

    pub async fn spawn_successor(&self, exe: &Path) -> Result<Successor> {
        let door = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let invitation = Invitation {
            port: door.local_addr()?.port(),
            token: uuid::Uuid::new_v4().simple().to_string(),
            listeners: self
                .listeners
                .lock()
                .unwrap()
                .iter()
                .map(|(addr, listener)| (*addr, raw_handle(listener)))
                .collect(),
        };
        let mut cmd = Command::new(exe);
        cmd.arg("worker").env(HANDOFF_ENV, serde_json::to_string(&invitation)?);
        share_listeners(&mut cmd, &invitation.listeners)?;
        let mut child = Reaped(Some(cmd.spawn().with_context(|| format!("start {}", exe.display()))?));

        let deadline = Instant::now() + HELLO_TIMEOUT;
        let stream = loop {
            if let Ok(accepted) = tokio::time::timeout(Duration::from_millis(100), door.accept()).await {
                break accepted?.0;
            }
            if let Some(status) = child.exited()? {
                bail!("the new conflux stopped before it took over ({status})");
            }
            if Instant::now() > deadline {
                bail!("the new conflux did not start in time");
            }
        };
        let mut link = BufReader::new(stream);
        let hello = receive(&mut link, HELLO_TIMEOUT).await?;
        if hello["token"].as_str() != Some(&invitation.token) {
            bail!("the handoff connection did not come from the new conflux");
        }
        Ok(Successor { child, link })
    }

    pub async fn take_over(&self) -> Result<Option<Takeover>> {
        let Ok(raw) = std::env::var(HANDOFF_ENV) else {
            return Ok(None);
        };
        if INVITATION_TAKEN.swap(true, Ordering::SeqCst) {
            return Ok(None);
        }
        let invitation: Invitation = serde_json::from_str(&raw).context("read the handoff invitation")?;
        let stream = TcpStream::connect(("127.0.0.1", invitation.port))
            .await
            .context("reach the running conflux")?;
        let mut link = BufReader::new(stream);
        send(&mut link, &json!({ "token": invitation.token })).await?;
        let state = receive(&mut link, HELLO_TIMEOUT).await?;
        let mut inherited = self.inherited.lock().unwrap();
        for (addr, handle) in invitation.listeners {
            let passed = unsafe { adopt(handle) };
            inherited.insert(addr, passed.try_clone().context("take over a listener")?);
        }
        Ok(Some(Takeover { link, state }))
    }
}

struct Reaped(Option<Child>);

impl Reaped {
    fn exited(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self.0.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub struct Successor {
    child: Reaped,
    link: BufReader<TcpStream>,
}

impl Successor {
    pub async fn hand_over(
        mut self,
        state: &Value,
        mut later: mpsc::UnboundedReceiver<Value>,
        in_flight: InFlight,
    ) -> Result<()> {
        send(&mut self.link, state).await?;
        let reply = receive(&mut self.link, READY_TIMEOUT).await?;
        if reply["ready"] != true {
            bail!("the new conflux did not confirm that it is serving");
        }
        if let Some(mut child) = self.child.0.take() {
            std::thread::spawn(move || {
                let status = child.wait();
                if !status.as_ref().is_ok_and(|s| s.success()) {
                    tracing::error!(
                        ?status,
                        "the new conflux crashed while this one was finishing its requests"
                    );
                    std::process::exit(1);
                }
            });
        }
        let mut link = self.link;
        tokio::spawn(async move {
            loop {
                match tokio::time::timeout(DRAIN_POLL, later.recv()).await {
                    Ok(Some(message)) if send(&mut link, &message).await.is_ok() => {}
                    Err(_) if Arc::strong_count(&in_flight) > 2 || !later.is_empty() => {}
                    _ => break,
                }
            }
        });
        Ok(())
    }
}

pub struct Takeover {
    link: BufReader<TcpStream>,
    pub state: Value,
}

impl Takeover {
    pub async fn ready(mut self, mut later: impl FnMut(Value) + Send + 'static) -> Result<()> {
        send(&mut self.link, &json!({ "ready": true })).await?;
        tokio::spawn(async move {
            let mut line = String::new();
            while let Ok(read) = self.link.read_line(&mut line).await
                && read > 0
            {
                if let Ok(message) = serde_json::from_str(&line) {
                    later(message);
                }
                line.clear();
            }
        });
        Ok(())
    }
}

async fn send(link: &mut BufReader<TcpStream>, message: &Value) -> Result<()> {
    let mut line = message.to_string();
    line.push('\n');
    link.get_mut().write_all(line.as_bytes()).await?;
    Ok(())
}

async fn receive(link: &mut BufReader<TcpStream>, limit: Duration) -> Result<Value> {
    let mut line = String::new();
    let read = tokio::time::timeout(limit, link.read_line(&mut line))
        .await
        .context("the handoff timed out")??;
    if read == 0 {
        bail!("the other conflux closed the handoff");
    }
    serde_json::from_str(&line).context("read a handoff message")
}

#[cfg(unix)]
fn raw_handle(listener: &std::net::TcpListener) -> u64 {
    use std::os::fd::AsRawFd;
    listener.as_raw_fd() as u64
}

#[cfg(windows)]
fn raw_handle(listener: &std::net::TcpListener) -> u64 {
    use std::os::windows::io::AsRawSocket;
    listener.as_raw_socket()
}

#[cfg(unix)]
fn share_listeners(cmd: &mut Command, listeners: &[(SocketAddr, u64)]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let fds: Vec<libc::c_int> = listeners.iter().map(|(_, handle)| *handle as libc::c_int).collect();
    unsafe {
        cmd.pre_exec(move || {
            for fd in &fds {
                let flags = libc::fcntl(*fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(windows)]
fn share_listeners(_cmd: &mut Command, listeners: &[(SocketAddr, u64)]) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
    for (_, handle) in listeners {
        let shared = unsafe { SetHandleInformation(*handle as HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        if shared == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
unsafe fn adopt(handle: u64) -> std::net::TcpListener {
    use std::os::fd::FromRawFd;
    unsafe { std::net::TcpListener::from_raw_fd(handle as std::os::fd::RawFd) }
}

#[cfg(windows)]
unsafe fn adopt(handle: u64) -> std::net::TcpListener {
    use std::os::windows::io::FromRawSocket;
    unsafe { std::net::TcpListener::from_raw_socket(handle) }
}
