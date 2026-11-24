use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tao::dpi::{PhysicalPosition, PhysicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::{EventLoopBuilderExtWindows, WindowBuilderExtWindows, WindowExtWindows};
use tao::window::{Window, WindowBuilder};
use tiny_skia::{BlendMode, FillRule, Paint, PathBuilder, Pixmap, Transform};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, POINT, WAIT_OBJECT_0,
};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, GetForegroundWindow, GetSystemMetrics, SM_CXSMICON, SW_SHOWNORMAL, WSF_VISIBLE,
};
use wry::{WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows};

use crate::config::{Config, Paths};

const PAGE: &str = include_str!("tray.html");
const LOGO: &str = include_str!("../assets/icon.svg");
const WIDTH: f64 = 360.0;
const MARGIN: f64 = 12.0;
const TICK: Duration = Duration::from_secs(3);
const REPLACES: &str = "CONFLUX_TRAY_REPLACES";

enum Message {
    Tray(TrayIconEvent),
    Menu(MenuId),
    Page(String),
    Reply(u64, Value),
    CheckFocus,
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
    Conflux {
        args: Vec<String>,
    },
    Restart,
    Open {
        url: String,
    },
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

pub fn run() -> Result<()> {
    if !on_desktop() {
        bail!("the tray app needs a signed-in Windows desktop");
    }
    let paths = Paths::from_env()?;
    let Some(reopened) = single_instance(&paths.config_dir) else {
        return Ok(());
    };
    let exe = std::env::current_exe().context("find the conflux binary")?;
    let built = modified(&exe);
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
        .with_inner_size(PhysicalSize::new(WIDTH, 200.0))
        .build(&event_loop)
        .context("open the tray window")?;
    round_corners(&window);

    let mut context = WebContext::new(Some(paths.data_dir.join("tray")));
    let page_proxy = proxy.clone();
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_html(PAGE)
        .with_ipc_handler(move |request| {
            let _ = page_proxy.send_event(Message::Page(request.into_body()));
        })
        .with_navigation_handler(|url| url.starts_with("about:") || url.starts_with("data:"))
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .with_default_context_menus(false)
        .with_browser_accelerator_keys(false)
        .with_hotkeys_zoom(false)
        .build(&window)
        .context("start WebView2. Install the WebView2 runtime from Microsoft and try again")?;

    let open_dashboard = MenuItem::new("Open dashboard", true, None);
    let exit = MenuItem::new("Exit tray app", true, None);
    let menu = Menu::with_items(&[&open_dashboard, &exit])?;
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
        http: reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?,
        exe: exe.clone(),
        paths,
        dev_copy,
    });
    let mut flyout = Flyout {
        window,
        webview,
        tray,
        height: 200.0,
        hidden_at: Instant::now(),
        anchor: None,
        badge: (None, false, light_taskbar()),
    };

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        let _ = &context;
        match event {
            Event::WindowEvent {
                event: WindowEvent::Focused(false),
                ..
            } => check_focus_soon(&bridge.proxy),
            Event::UserEvent(Message::CheckFocus) => {
                if unsafe { GetForegroundWindow() } != flyout.window.hwnd() as HWND {
                    flyout.hide();
                }
            }
            Event::UserEvent(Message::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            })) => {
                flyout.anchor = Some(PhysicalPosition::new(
                    rect.position.x + f64::from(rect.size.width) / 2.0,
                    rect.position.y + f64::from(rect.size.height) / 2.0,
                ));
                flyout.toggle();
            }
            Event::UserEvent(Message::Show) => {
                flyout.anchor = flyout.tray.rect().map(|rect| {
                    PhysicalPosition::new(
                        rect.position.x + f64::from(rect.size.width) / 2.0,
                        rect.position.y + f64::from(rect.size.height) / 2.0,
                    )
                });
                flyout.show();
            }
            Event::UserEvent(Message::Menu(id)) => {
                if id == *open_dashboard.id() {
                    let _ = flyout.webview.evaluate_script("conflux.openDashboard()");
                } else if id == *exit.id() {
                    *flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(Message::Reply(id, value)) => {
                let _ = flyout.webview.evaluate_script(&format!("conflux.reply({id}, {value})"));
            }
            Event::UserEvent(Message::Tick) => {
                if modified(&exe).is_some_and(|now| Some(now) != built) {
                    let _ = spawn(&exe, Some(std::process::id()));
                    *flow = ControlFlow::Exit;
                    return;
                }
                let light = light_taskbar();
                if light != flyout.badge.2 {
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
                        if flyout.window.is_visible() {
                            flyout.place();
                        }
                    }
                    Request::Icon { badge, dimmed, tooltip } => {
                        flyout.set_icon(badge, dimmed);
                        let _ = flyout.tray.set_tooltip(Some(tooltip));
                    }
                    Request::Hide => flyout.hide(),
                    Request::Blur => check_focus_soon(&bridge.proxy),
                    Request::Quit => {
                        *flow = ControlFlow::Exit;
                        return;
                    }
                    Request::Open { url } => open(&url),
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

struct Flyout {
    window: Window,
    webview: WebView,
    tray: TrayIcon,
    height: f64,
    hidden_at: Instant,
    anchor: Option<PhysicalPosition<f64>>,
    badge: (Option<String>, bool, bool),
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
            GetMonitorInfoW(monitor, &mut info);
            let (mut dpi, mut unused) = (96, 96);
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut unused);
            (info.rcWork, info.rcMonitor, f64::from(dpi) / 96.0)
        };
        let width = (WIDTH * scale).round();
        let height = (self.height * scale)
            .round()
            .min(f64::from(work.bottom - work.top) - 2.0 * MARGIN * scale);
        let margin = MARGIN * scale;
        let center = self.anchor.map_or(f64::from(work.right), |anchor| anchor.x);
        let x = (center - width / 2.0)
            .min(f64::from(work.right) - width - margin)
            .max(f64::from(work.left) + margin);
        let taskbar_hidden = work.top == screen.top && work.bottom == screen.bottom;
        let taskbar_on_top = work.top > screen.top || (taskbar_hidden && point.y < (screen.top + screen.bottom) / 2);
        let y = if taskbar_on_top {
            f64::from(work.top) + margin
        } else {
            f64::from(work.bottom) - height - margin
        };
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

struct Bridge {
    proxy: EventLoopProxy<Message>,
    http: reqwest::Client,
    exe: PathBuf,
    paths: Paths,
    dev_copy: bool,
}

impl Bridge {
    fn base(&self) -> String {
        let port = Config::load(&self.paths)
            .map(|c| c.port)
            .unwrap_or(crate::config::DEFAULT_PORT);
        format!("http://127.0.0.1:{port}")
    }

    async fn answer(&self, request: Request) -> Value {
        match request {
            Request::Local => tokio::task::block_in_place(|| self.local()),
            Request::Get { path } => answered(self.http.get(format!("{}{path}", self.base())).send().await).await,
            Request::Post { path, body } => {
                let sent = self
                    .http
                    .post(format!("{}{path}", self.base()))
                    .timeout(Duration::from_secs(30))
                    .json(&body)
                    .send()
                    .await;
                answered(sent).await
            }
            Request::Conflux { args } => {
                let exe = self.exe.clone();
                let ran = tokio::task::spawn_blocking(move || {
                    Command::new(exe)
                        .args(args)
                        .stdin(std::process::Stdio::null())
                        .creation_flags(CREATE_NO_WINDOW)
                        .output()
                })
                .await;
                match ran {
                    Ok(Ok(output)) => {
                        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                        text.push_str(&String::from_utf8_lossy(&output.stderr));
                        json!({ "ok": output.status.success(), "output": text.trim() })
                    }
                    Ok(Err(e)) => json!({ "ok": false, "output": e.to_string() }),
                    Err(e) => json!({ "ok": false, "output": e.to_string() }),
                }
            }
            Request::Restart if self.dev_copy => {
                json!({ "ok": false, "output": "This is a dev copy. It has no background service to restart." })
            }
            Request::Restart => match tokio::task::spawn_blocking(crate::service::restart).await {
                Ok(Ok(())) => json!({ "ok": true }),
                Ok(Err(e)) => json!({ "ok": false, "output": format!("{e:#}") }),
                Err(e) => json!({ "ok": false, "output": e.to_string() }),
            },
            _ => Value::Null,
        }
    }

    fn local(&self) -> Value {
        let config = Config::load(&self.paths).ok();
        let routing = crate::setup::load_joined(&self.paths.config_dir)
            .ok()
            .flatten()
            .map(|joined| joined.active);
        let (light, dark) = accent();
        json!({
            "port": config.as_ref().map_or(crate::config::DEFAULT_PORT, |c| c.port),
            "role": config.as_ref().map(|c| c.role),
            "routing": routing,
            "serviceInstalled": !self.dev_copy && crate::service::installed(),
            "devCopy": self.dev_copy,
            "version": crate::update::CURRENT,
            "accent": { "light": light, "dark": dark },
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

fn check_focus_soon(proxy: &EventLoopProxy<Message>) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
        let _ = proxy.send_event(Message::CheckFocus);
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

struct Signal(HANDLE);

unsafe impl Send for Signal {}

fn single_instance(config_dir: &Path) -> Option<Signal> {
    if let Some(pid) = std::env::var(REPLACES).ok().and_then(|pid| pid.parse().ok()) {
        unsafe {
            let previous = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if !previous.is_null() {
                WaitForSingleObject(previous, 10_000);
                CloseHandle(previous);
            }
        }
    }
    let digest = Sha256::digest(config_dir.to_string_lossy().to_lowercase().as_bytes());
    let tag: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let name = wide(&format!("Local\\conflux-tray-{tag}"));
    unsafe {
        let event = CreateEventW(std::ptr::null(), 0, 0, name.as_ptr());
        if event.is_null() {
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            AllowSetForegroundWindow(ASFW_ANY);
            SetEvent(event);
            CloseHandle(event);
            return None;
        }
        Some(Signal(event))
    }
}

fn modified(exe: &Path) -> Option<SystemTime> {
    std::fs::metadata(exe).and_then(|m| m.modified()).ok()
}

pub fn launch(exe: &Path) -> std::io::Result<()> {
    spawn(exe, None)
}

fn spawn(exe: &Path, replaces: Option<u32>) -> std::io::Result<()> {
    let mut command = Command::new(exe);
    command
        .arg("tray")
        .creation_flags(CREATE_NO_WINDOW)
        .env_remove(REPLACES);
    if let Some(pid) = replaces {
        command.env(REPLACES, pid.to_string());
    }
    command.spawn().map(drop)
}

fn open(url: &str) {
    if !url.starts_with("http://127.0.0.1:") {
        return;
    }
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

fn round_corners(window: &Window) {
    let preference = DWMWCP_ROUND;
    unsafe {
        DwmSetWindowAttribute(
            window.hwnd() as HWND,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&preference as *const i32).cast(),
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
    (read == 0).then_some(data)
}

fn light_taskbar() -> bool {
    registry::<4>(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "SystemUsesLightTheme",
        RRF_RT_REG_DWORD,
    )
    .is_some_and(|value| u32::from_le_bytes(value) != 0)
}

fn accent() -> (String, String) {
    let hex = |palette: &[u8; 32], i: usize| {
        format!(
            "#{:02x}{:02x}{:02x}",
            palette[i * 4],
            palette[i * 4 + 1],
            palette[i * 4 + 2]
        )
    };
    match registry::<32>(
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent",
        "AccentPalette",
        RRF_RT_REG_BINARY,
    ) {
        Some(palette) => (hex(&palette, 4), hex(&palette, 1)),
        None => ("#005fb8".into(), "#60cdff".into()),
    }
}

fn logo() -> Option<tiny_skia::Path> {
    let start = LOGO.find(" d=\"")? + 4;
    let end = start + LOGO[start..].find('"')?;
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
    for c in LOGO[start..end].chars().chain(std::iter::once(' ')) {
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
