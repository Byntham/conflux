use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::{EventLoopBuilderExtWindows, WindowBuilderExtWindows, WindowExtWindows};
use tao::window::{Window, WindowBuilder};
use tiny_skia::{BlendMode, FillRule, Paint, PathBuilder, Pixmap, Transform};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, POINT, RECT, WAIT_OBJECT_0,
};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_TRANSITIONS_FORCEDISABLED, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
use windows_sys::Win32::System::Console::GetConsoleProcessList;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, GetForegroundWindow, GetSystemMetrics, MB_ICONERROR, MB_OK, MessageBoxW,
    SM_CXSMICON, SW_SHOWNORMAL, WSF_VISIBLE,
};
use wry::{WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows};

use crate::config::{Config, Paths};

const PAGE: &str = include_str!("tray.html");
const LOGO: &str = include_str!("../assets/icon.svg");
const WIDTH: f64 = 360.0;
const MARGIN: f64 = 12.0;
const TICK: Duration = Duration::from_secs(3);
const SETTLE: Duration = Duration::from_secs(120);
const REPLACES: &str = "CONFLUX_TRAY_REPLACES";
const MODE: &str = "CONFLUX_TRAY_MODE";

enum Message {
    Tray(TrayIconEvent),
    Menu(MenuId),
    Page(String),
    Reply(u64, Value),
    CheckFocus(u64),
    Show,
    Tick,
}

#[derive(Deserialize)]
struct Call {
    id: u64,
    #[serde(flatten)]
    request: Request,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum Request {
    Local,
    Get {
        path: String,
    },
    Post {
        path: String,
        body: Value,
    },
    Routing {
        on: bool,
    },
    SetUp {
        hub: bool,
    },
    Restart,
    OpenDashboard,
    Size {
        height: f64,
    },
    Icon {
        badge: Option<String>,
        dimmed: bool,
        tooltip: String,
    },
    Hide,
    Blur,
    Quit,
}

#[derive(Clone, Copy, PartialEq)]
enum Launch {
    Quiet,
    Open,
}

pub fn run() -> Result<()> {
    if !on_desktop() {
        bail!("the tray app needs a signed-in Windows desktop");
    }
    let paths = Paths::from_env()?;
    let exe = std::env::current_exe().context("find the conflux binary")?;
    let quiet = std::env::var(MODE).is_ok_and(|mode| mode == "quiet");
    let Some(reopened) = single_instance(&paths.config_dir, !quiet) else {
        return Ok(());
    };
    if in_terminal() {
        drop(reopened);
        if wry::webview_version().is_err() {
            bail!(
                "the tray app needs the Microsoft Edge WebView2 runtime. Install it from Microsoft, then run `conflux tray` again"
            );
        }
        unsafe { AllowSetForegroundWindow(ASFW_ANY) };
        return spawn(&exe, Launch::Open, None).context("start the tray app");
    }
    let open = std::env::var(MODE).is_ok_and(|mode| mode == "open");
    let failed = serve(paths, exe, reopened, open).unwrap_err();
    if open {
        alert(&format!("conflux could not open its tray app: {failed:#}"));
    }
    Err(failed)
}

fn serve(paths: Paths, exe: PathBuf, reopened: Signal, mut open: bool) -> Result<std::convert::Infallible> {
    let dev_copy = std::env::var_os("CONFLUX_HOME").is_some();

    let event_loop = EventLoopBuilder::<Message>::with_user_event()
        .with_any_thread(true)
        .build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("conflux")
        .with_decorations(false)
        .with_resizable(false)
        .with_visible(false)
        .with_always_on_top(true)
        .with_skip_taskbar(true)
        .with_undecorated_shadow(true)
        .with_inner_size(LogicalSize::new(WIDTH, 200.0))
        .build(&event_loop)
        .context("open the tray window")?;
    style_flyout(&window);

    let mut context = WebContext::new(Some(paths.data_dir.join("tray")));
    let page_proxy = proxy.clone();
    let background = if light_apps() {
        (0xf3, 0xf3, 0xf3, 0xff)
    } else {
        (0x2c, 0x2c, 0x2c, 0xff)
    };
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_html(PAGE.replace("__LOGO__", logo_data().unwrap_or_default()))
        .with_background_color(background)
        .with_ipc_handler(move |request| {
            let _ = page_proxy.send_event(Message::Page(request.into_body()));
        })
        .with_navigation_handler(|url| url.starts_with("data:"))
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .with_default_context_menus(false)
        .with_browser_accelerator_keys(false)
        .with_hotkeys_zoom(false)
        .build(&window)
        .with_context(|| {
            format!(
                "start WebView2 with its data in {}. If the WebView2 runtime is missing, install it from Microsoft",
                paths.data_dir.join("tray").display()
            )
        })?;

    let open_panel = MenuItem::new("Open conflux", true, None);
    let open_dashboard = MenuItem::new("Open dashboard", true, None);
    let exit = MenuItem::new("Exit tray app", true, None);
    let menu = Menu::with_items(&[&open_panel, &open_dashboard, &exit])?;
    let tray = TrayIconBuilder::new()
        .with_icon(icon(None, false))
        .with_tooltip("conflux")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
        .context("add the tray icon")?;
    let tray_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tray_proxy.send_event(Message::Tray(event));
    }));
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = menu_proxy.send_event(Message::Menu(event.id));
    }));
    let show_proxy = proxy.clone();
    std::thread::spawn(move || {
        let reopened = reopened;
        while unsafe { WaitForSingleObject(reopened.0, INFINITE) } == WAIT_OBJECT_0
            && show_proxy.send_event(Message::Show).is_ok()
        {}
    });
    let tick_proxy = proxy.clone();
    std::thread::spawn(move || {
        while tick_proxy.send_event(Message::Tick).is_ok() {
            std::thread::sleep(TICK);
        }
    });

    let runtime = tokio::runtime::Handle::current();
    let bridge = Arc::new(Bridge {
        proxy,
        http: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        exe: exe.clone(),
        paths,
        dev_copy,
        seen: Mutex::default(),
    });
    let mut flyout = Flyout {
        window,
        webview,
        tray,
        height: 200.0,
        hidden_at: Instant::now(),
        anchor: None,
        badge: (None, false, light_taskbar()),
        shown: 0,
        shown_at: Instant::now(),
    };
    let mut update = Update {
        built: modified(&exe),
        seen: None,
    };

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        let _ = &context;
        match event {
            Event::WindowEvent {
                event: WindowEvent::Focused(false),
                ..
            } => check_focus_soon(&bridge.proxy, flyout.shown),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => flyout.hide(),
            Event::UserEvent(Message::CheckFocus(shown)) => {
                if shown == flyout.shown && unsafe { GetForegroundWindow() } != flyout.window.hwnd() as HWND {
                    flyout.hide();
                }
            }
            Event::UserEvent(Message::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            })) => {
                let double_click = Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() }));
                if !(flyout.window.is_visible() && flyout.shown_at.elapsed() < double_click) {
                    flyout.anchor = Some(center(&rect));
                    flyout.toggle();
                }
            }
            Event::UserEvent(Message::Tray(TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                rect,
                ..
            })) => {
                if flyout.window.is_visible() {
                    flyout.shown += 1;
                    flyout.window.set_focus();
                    let _ = flyout.webview.focus();
                } else {
                    flyout.anchor = Some(center(&rect));
                    flyout.show();
                }
            }
            Event::UserEvent(Message::Show) => {
                flyout.anchor = flyout.tray.rect().map(|rect| center(&rect));
                flyout.show();
            }
            Event::UserEvent(Message::Menu(id)) => {
                if id == *open_panel.id() {
                    flyout.anchor = flyout.tray.rect().map(|rect| center(&rect));
                    flyout.show();
                } else if id == *open_dashboard.id() {
                    if let Some(base) = bridge.base() {
                        open_url(&base);
                    }
                    flyout.hide();
                } else if id == *exit.id() {
                    *flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(Message::Reply(id, value)) => {
                let _ = flyout.webview.evaluate_script(&format!("conflux.reply({id}, {value})"));
            }
            Event::UserEvent(Message::Tick) => {
                if !dev_copy && update.ready(&exe) && spawn(&exe, Launch::Quiet, Some(std::process::id())).is_ok() {
                    *flow = ControlFlow::Exit;
                    return;
                }
                if light_taskbar() != flyout.badge.2 {
                    let (badge, dimmed, _) = flyout.badge.clone();
                    flyout.set_icon(badge, dimmed);
                }
                let _ = flyout.webview.evaluate_script("conflux.tick()");
            }
            Event::UserEvent(Message::Page(body)) => {
                let Ok(Call { id, request }) = serde_json::from_str::<Call>(&body) else {
                    return;
                };
                match request {
                    Request::Size { height } => {
                        flyout.height = height;
                        if open {
                            open = false;
                            flyout.anchor = flyout.tray.rect().map(|rect| center(&rect));
                            flyout.show();
                        } else if flyout.window.is_visible() {
                            flyout.place();
                        }
                    }
                    Request::Icon { badge, dimmed, tooltip } => {
                        flyout.set_icon(badge, dimmed);
                        let _ = flyout.tray.set_tooltip(Some(tooltip));
                    }
                    Request::Hide => flyout.hide(),
                    Request::Blur => check_focus_soon(&bridge.proxy, flyout.shown),
                    Request::Quit => {
                        *flow = ControlFlow::Exit;
                        return;
                    }
                    Request::OpenDashboard => {
                        if let Some(base) = bridge.base() {
                            open_url(&base);
                        }
                        flyout.hide();
                    }
                    request => {
                        let bridge = bridge.clone();
                        runtime.spawn(async move {
                            let value = bridge.answer(request).await;
                            let _ = bridge.proxy.send_event(Message::Reply(id, value));
                        });
                        return;
                    }
                }
                let _ = flyout.webview.evaluate_script(&format!("conflux.reply({id}, null)"));
            }
            _ => {}
        }
    })
}

struct Update {
    built: Option<SystemTime>,
    seen: Option<(SystemTime, Instant)>,
}

impl Update {
    fn ready(&mut self, exe: &Path) -> bool {
        let Some(now) = modified(exe) else {
            return false;
        };
        if self.built.is_none() {
            self.built = Some(now);
        }
        if Some(now) == self.built {
            self.seen = None;
            return false;
        }
        match self.seen {
            Some((seen, since)) if seen == now => since.elapsed() >= SETTLE,
            _ => {
                self.seen = Some((now, Instant::now()));
                false
            }
        }
    }
}

fn center(rect: &tray_icon::Rect) -> PhysicalPosition<f64> {
    PhysicalPosition::new(
        rect.position.x + f64::from(rect.size.width) / 2.0,
        rect.position.y + f64::from(rect.size.height) / 2.0,
    )
}

struct Flyout {
    window: Window,
    webview: WebView,
    tray: TrayIcon,
    height: f64,
    hidden_at: Instant,
    anchor: Option<PhysicalPosition<f64>>,
    badge: (Option<String>, bool, bool),
    shown: u64,
    shown_at: Instant,
}

impl Flyout {
    fn toggle(&mut self) {
        if self.window.is_visible() {
            self.hide();
        } else if self.hidden_at.elapsed() > Duration::from_millis(300) {
            self.show();
        }
    }

    fn show(&mut self) {
        self.shown += 1;
        self.shown_at = Instant::now();
        self.place();
        self.window.set_visible(true);
        self.window.set_focus();
        let _ = self.webview.focus();
        let _ = self.webview.evaluate_script("conflux.shown()");
    }

    fn hide(&mut self) {
        if !self.window.is_visible() {
            return;
        }
        self.window.set_visible(false);
        self.hidden_at = Instant::now();
        let _ = self.webview.evaluate_script("conflux.hidden()");
    }

    fn place(&self) {
        let point = self.anchor.map_or(POINT { x: 0, y: 0 }, |anchor| POINT {
            x: anchor.x as i32,
            y: anchor.y as i32,
        });
        let (work, screen, scale) = unsafe {
            let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return;
            }
            let (mut dpi, mut unused) = (96, 96);
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut unused);
            (info.rcWork, info.rcMonitor, f64::from(dpi.max(96)) / 96.0)
        };
        let margin = MARGIN * scale;
        let (left, top, right, bottom) = (
            f64::from(work.left) + margin,
            f64::from(work.top) + margin,
            f64::from(work.right) - margin,
            f64::from(work.bottom) - margin,
        );
        let width = (WIDTH * scale).round().min(right - left).max(1.0);
        let height = (self.height * scale).ceil().min(bottom - top).max(1.0);
        let (cx, cy) = self.anchor.map_or((right, bottom), |anchor| (anchor.x, anchor.y));
        let (x, y) = match taskbar_edge(&work, &screen, self.anchor) {
            Edge::Top => ((cx - width / 2.0).min(right - width).max(left), top),
            Edge::Bottom => ((cx - width / 2.0).min(right - width).max(left), bottom - height),
            Edge::Left => (left, (cy - height / 2.0).min(bottom - height).max(top)),
            Edge::Right => (right - width, (cy - height / 2.0).min(bottom - height).max(top)),
        };
        self.window.set_outer_position(PhysicalPosition::new(x, y));
        self.window.set_inner_size(PhysicalSize::new(width, height));
        self.window.set_outer_position(PhysicalPosition::new(x, y));
    }

    fn set_icon(&mut self, badge: Option<String>, dimmed: bool) {
        let light = light_taskbar();
        if self.badge == (badge.clone(), dimmed, light) {
            return;
        }
        let _ = self.tray.set_icon(Some(icon(badge.as_deref(), dimmed)));
        self.badge = (badge, dimmed, light);
    }
}

enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

fn taskbar_edge(work: &RECT, screen: &RECT, anchor: Option<PhysicalPosition<f64>>) -> Edge {
    if work.bottom < screen.bottom {
        return Edge::Bottom;
    }
    if work.top > screen.top {
        return Edge::Top;
    }
    if work.left > screen.left {
        return Edge::Left;
    }
    if work.right < screen.right {
        return Edge::Right;
    }
    let Some(anchor) = anchor else {
        return Edge::Bottom;
    };
    let distances = [
        (anchor.y - f64::from(screen.top), Edge::Top),
        (f64::from(screen.bottom) - anchor.y, Edge::Bottom),
        (anchor.x - f64::from(screen.left), Edge::Left),
        (f64::from(screen.right) - anchor.x, Edge::Right),
    ];
    distances
        .into_iter()
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(Edge::Bottom, |(_, edge)| edge)
}

struct Bridge {
    proxy: EventLoopProxy<Message>,
    http: reqwest::Client,
    exe: PathBuf,
    paths: Paths,
    dev_copy: bool,
    seen: Mutex<HashMap<url::Url, (reqwest::header::HeaderValue, String)>>,
}

impl Bridge {
    fn base(&self) -> Option<String> {
        let port = Config::load(&self.paths).map(|c| c.port);
        if self.dev_copy
            && (!self.paths.config_file().exists() || port.as_ref().is_ok_and(|p| *p == crate::config::DEFAULT_PORT))
        {
            return None;
        }
        let port = match port {
            Ok(port) => port,
            Err(_) if self.dev_copy => return None,
            Err(_) => crate::config::DEFAULT_PORT,
        };
        Some(format!("http://127.0.0.1:{port}/"))
    }

    fn url(&self, path: &str) -> Option<url::Url> {
        let base = url::Url::parse(&self.base()?).ok()?;
        let url = base.join(path).ok()?;
        (url.origin() == base.origin() && url.path().starts_with("/conflux/") && !path.contains('\\')).then_some(url)
    }

    async fn answer(&self, request: Request) -> Value {
        match request {
            Request::Local => tokio::task::block_in_place(|| self.local()),
            Request::Get { path } => match self.url(&path) {
                Some(url) => self.unless_seen(url).await,
                None => json!({ "unreachable": true }),
            },
            Request::Post { path, body } => match self.url(&path) {
                Some(url) => {
                    let sent = self
                        .http
                        .post(url)
                        .timeout(Duration::from_secs(30))
                        .json(&body)
                        .send()
                        .await;
                    answered(sent).await
                }
                None => json!({ "unreachable": true }),
            },
            Request::Routing { .. } if self.dev_copy && !isolated_clients(&self.paths) => json!({
                "ok": false,
                "output": "This dev copy would change your real coding tool settings. Start it with `cargo run --example dev -- --app`."
            }),
            Request::Routing { on } => self.conflux(&[if on { "on" } else { "off" }]).await,
            Request::SetUp { .. } | Request::Restart if self.dev_copy => {
                json!({ "ok": false, "output": "This is a dev copy. It has no background service." })
            }
            Request::SetUp { hub: true } => self.conflux(&["join", "self"]).await,
            Request::SetUp { hub: false } => self.conflux(&["wait"]).await,
            Request::Restart => match tokio::task::spawn_blocking(crate::service::restart).await {
                Ok(Ok(())) => json!({ "ok": true }),
                Ok(Err(e)) => json!({ "ok": false, "output": format!("{e:#}") }),
                Err(e) => json!({ "ok": false, "output": e.to_string() }),
            },
            _ => Value::Null,
        }
    }

    async fn unless_seen(&self, url: url::Url) -> Value {
        let known = self.seen.lock().unwrap().get(&url).cloned();
        let mut asked = self.http.get(url.clone());
        if let Some((tag, _)) = &known {
            asked = asked.header(reqwest::header::IF_NONE_MATCH, tag);
        }
        let Ok(response) = asked.send().await else {
            return json!({ "unreachable": true });
        };
        if let (reqwest::StatusCode::NOT_MODIFIED, Some((_, body))) = (response.status(), known) {
            return json!({ "status": 200, "body": body });
        }
        let status = response.status();
        let tag = response.headers().get(reqwest::header::ETAG).cloned();
        let Ok(body) = response.text().await else {
            return json!({ "unreachable": true });
        };
        if status == reqwest::StatusCode::OK {
            let mut seen = self.seen.lock().unwrap();
            match tag {
                Some(tag) => seen.insert(url, (tag, body.clone())),
                None => seen.remove(&url),
            };
        }
        json!({ "status": status.as_u16(), "body": body })
    }

    async fn conflux(&self, args: &[&'static str]) -> Value {
        let ran = tokio::process::Command::new(&self.exe)
            .args(args)
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(Duration::from_secs(120), ran).await {
            Ok(Ok(output)) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                json!({ "ok": output.status.success(), "output": text.trim() })
            }
            Ok(Err(e)) => json!({ "ok": false, "output": e.to_string() }),
            Err(_) => json!({ "ok": false, "output": "conflux did not finish within 2 minutes." }),
        }
    }

    fn local(&self) -> Value {
        let config = Config::load(&self.paths).ok();
        let routing = crate::setup::load_joined(&self.paths.config_dir)
            .ok()
            .flatten()
            .map(|joined| joined.active);
        let role = config
            .as_ref()
            .filter(|_| self.paths.config_file().exists())
            .map(|c| c.role);
        let (light, dark) = accent();
        json!({
            "role": role,
            "routing": routing,
            "serviceInstalled": !self.dev_copy && crate::service::installed(),
            "version": crate::update::CURRENT,
            "accent": { "light": hex(light), "dark": hex(dark) },
        })
    }
}

async fn answered(sent: reqwest::Result<reqwest::Response>) -> Value {
    let Ok(response) = sent else {
        return json!({ "unreachable": true });
    };
    let status = response.status().as_u16();
    match response.text().await {
        Ok(body) => json!({ "status": status, "body": body }),
        Err(_) => json!({ "unreachable": true }),
    }
}

fn check_focus_soon(proxy: &EventLoopProxy<Message>, shown: u64) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
        let _ = proxy.send_event(Message::CheckFocus(shown));
    });
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn on_desktop() -> bool {
    unsafe {
        let station = GetProcessWindowStation();
        let mut flags: USEROBJECTFLAGS = std::mem::zeroed();
        let read = GetUserObjectInformationW(
            station as HANDLE,
            UOI_FLAGS,
            (&mut flags as *mut USEROBJECTFLAGS).cast(),
            size_of::<USEROBJECTFLAGS>() as u32,
            std::ptr::null_mut(),
        );
        read != 0 && flags.dwFlags & WSF_VISIBLE as u32 != 0
    }
}

fn in_terminal() -> bool {
    let mut processes = [0u32; 4];
    unsafe { GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) > 1 }
}

struct Signal(HANDLE);

unsafe impl Send for Signal {}

impl Drop for Signal {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn single_instance(config_dir: &Path, show_running: bool) -> Option<Signal> {
    if let Some(pid) = std::env::var(REPLACES).ok().and_then(|pid| pid.parse().ok()) {
        unsafe {
            let previous = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if !previous.is_null() {
                WaitForSingleObject(previous, 10_000);
                CloseHandle(previous);
            }
        }
    }
    let config_dir = std::path::absolute(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
    let digest = Sha256::digest(config_dir.to_string_lossy().to_lowercase().as_bytes());
    let tag: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let name = wide(&format!("Local\\conflux-tray-{tag}"));
    unsafe {
        let event = CreateEventW(std::ptr::null(), 0, 0, name.as_ptr());
        if event.is_null() {
            return None;
        }
        let signal = Signal(event);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if show_running {
                AllowSetForegroundWindow(ASFW_ANY);
                SetEvent(signal.0);
            }
            return None;
        }
        Some(signal)
    }
}

fn modified(exe: &Path) -> Option<SystemTime> {
    std::fs::metadata(exe).and_then(|m| m.modified()).ok()
}

pub fn open_with_service() {
    if std::env::var_os("CONFLUX_HOME").is_some() || !crate::service::installed() || !on_desktop() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = spawn(&exe, Launch::Quiet, None);
    }
}

fn spawn(exe: &Path, launch: Launch, replaces: Option<u32>) -> std::io::Result<()> {
    crate::service::keep_std_handles_private();
    let mut command = Command::new(exe);
    command
        .arg("tray")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env(MODE, if launch == Launch::Open { "open" } else { "quiet" })
        .env_remove(REPLACES);
    if let Some(pid) = replaces {
        command.env(REPLACES, pid.to_string());
    }
    command.spawn().map(drop)
}

fn isolated_clients(paths: &Paths) -> bool {
    let Some(copy) = paths.config_dir.parent().and_then(Path::parent) else {
        return false;
    };
    let home = crate::config::home_dir().unwrap_or_default();
    let inside = |dir: &Path| {
        dir.is_absolute()
            && dir.starts_with(copy)
            && !dir.components().any(|part| part == std::path::Component::ParentDir)
    };
    let lower = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
    copy.is_absolute()
        && !lower(&home).starts_with(lower(copy))
        && ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "GROK_HOME", "XDG_CONFIG_HOME"]
            .iter()
            .all(|name| std::env::var(name).is_ok_and(|dir| inside(Path::new(&dir))))
}

fn alert(text: &str) {
    let (text, title) = (wide(text), wide("conflux"));
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_ICONERROR | MB_OK,
        )
    };
}

fn open_url(url: &str) {
    let (verb, target) = (wide("open"), wide(url));
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn style_flyout(window: &Window) {
    let preference = DWMWCP_ROUND;
    let no_transitions: i32 = 1;
    unsafe {
        DwmSetWindowAttribute(
            window.hwnd() as HWND,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&preference as *const i32).cast(),
            size_of::<i32>() as u32,
        );
        DwmSetWindowAttribute(
            window.hwnd() as HWND,
            DWMWA_TRANSITIONS_FORCEDISABLED as u32,
            (&no_transitions as *const i32).cast(),
            size_of::<i32>() as u32,
        );
    }
}

fn registry<const N: usize>(key: &str, value: &str, kind: u32) -> Option<[u8; N]> {
    let (key, value) = (wide(key), wide(value));
    let mut data = [0u8; N];
    let mut size = N as u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            kind,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (read == 0 && size == N as u32).then_some(data)
}

fn light_theme(value: &str) -> bool {
    registry::<4>(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        value,
        RRF_RT_REG_DWORD,
    )
    .is_some_and(|value| u32::from_le_bytes(value) != 0)
}

fn light_taskbar() -> bool {
    light_theme("SystemUsesLightTheme")
}

fn light_apps() -> bool {
    light_theme("AppsUseLightTheme")
}

fn accent() -> ([u8; 3], [u8; 3]) {
    let color = |palette: &[u8; 32], i: usize| [palette[i * 4], palette[i * 4 + 1], palette[i * 4 + 2]];
    match registry::<32>(
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent",
        "AccentPalette",
        RRF_RT_REG_BINARY,
    ) {
        Some(palette) => (color(&palette, 4), color(&palette, 1)),
        None => ([0x00, 0x5f, 0xb8], [0x60, 0xcd, 0xff]),
    }
}

fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn logo_data() -> Option<&'static str> {
    let start = LOGO.find(" d=\"")? + 4;
    let end = start + LOGO[start..].find('"')?;
    Some(&LOGO[start..end])
}

fn logo() -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();
    let mut numbers: Vec<f32> = Vec::new();
    let mut command = 'M';
    let mut token = String::new();
    let finish = |command: char, numbers: &mut Vec<f32>, builder: &mut PathBuilder| {
        match command {
            'M' => {
                for (i, [x, y]) in numbers.as_chunks::<2>().0.iter().enumerate() {
                    if i == 0 {
                        builder.move_to(*x, *y);
                    } else {
                        builder.line_to(*x, *y);
                    }
                }
            }
            'L' => numbers
                .as_chunks::<2>()
                .0
                .iter()
                .for_each(|[x, y]| builder.line_to(*x, *y)),
            'C' => numbers
                .as_chunks::<6>()
                .0
                .iter()
                .for_each(|[a, b, c, d, x, y]| builder.cubic_to(*a, *b, *c, *d, *x, *y)),
            'Z' | 'z' => builder.close(),
            _ => {}
        }
        numbers.clear();
    };
    for c in logo_data()?.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() || c == '.' {
            token.push(c);
            continue;
        }
        if let Ok(n) = token.parse() {
            numbers.push(n);
        }
        token.clear();
        if c == '-' {
            token.push('-');
        } else if c.is_ascii_alphabetic() {
            finish(command, &mut numbers, &mut builder);
            command = c;
        }
    }
    finish(command, &mut numbers, &mut builder);
    builder.finish()
}

fn icon(badge: Option<&str>, dimmed: bool) -> Icon {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64) as u32;
    let side = size as f32;
    let light = light_taskbar();
    let mut pixmap = Pixmap::new(size, size).expect("icon size is not zero");
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    let glyph = if light { 0x1c } else { 0xff };
    paint.set_color_rgba8(glyph, glyph, glyph, if dimmed { 140 } else { 255 });
    if let Some(path) = logo() {
        let bounds = path.bounds();
        let inset = side * 0.06;
        let scale = ((side - 2.0 * inset) / bounds.width()).min((side - 2.0 * inset) / bounds.height());
        let transform = Transform::from_translate(
            side / 2.0 - (bounds.left() + bounds.width() / 2.0) * scale,
            side / 2.0 - (bounds.top() + bounds.height() / 2.0) * scale,
        )
        .pre_scale(scale, scale);
        pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
    }
    if let Some(color) = badge.map(|name| badge_color(name, light)) {
        let radius = side * 0.2;
        let (cx, cy) = (side - radius, side - radius);
        let mut clear = Paint {
            anti_alias: true,
            blend_mode: BlendMode::Clear,
            ..Paint::default()
        };
        clear.set_color_rgba8(0, 0, 0, 255);
        if let Some(ring) = PathBuilder::from_circle(cx, cy, radius + side * 0.09) {
            pixmap.fill_path(&ring, &clear, FillRule::Winding, Transform::identity(), None);
        }
        paint.set_color_rgba8(color[0], color[1], color[2], 255);
        if let Some(dot) = PathBuilder::from_circle(cx, cy, radius) {
            pixmap.fill_path(&dot, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Icon::from_rgba(rgba, size, size).expect("icon pixels match its size")
}

fn badge_color(name: &str, light: bool) -> [u8; 3] {
    match (name, light) {
        ("red", true) => [0xc4, 0x2b, 0x1c],
        ("red", false) => [0xff, 0x6b, 0x6b],
        ("orange", true) => [0xd9, 0x7f, 0x06],
        ("orange", false) => [0xfb, 0xbf, 0x24],
        (_, true) => [0x00, 0x5f, 0xb8],
        (_, false) => [0x60, 0xcd, 0xff],
    }
}
