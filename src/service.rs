use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const UNIT_NAME: &str = "conflux.service";
const LAUNCHD_LABEL: &str = "dev.conflux";
const MENU_BAR_LABEL: &str = "dev.conflux.menubar";

fn home() -> Result<PathBuf> {
    crate::config::home_dir()
}

const WINDOWS_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

pub fn windows_launch_command(exe: &Path) -> String {
    format!(r#"conhost.exe --headless "{}" serve"#, exe.display())
}

pub fn windows_start_menu_shortcut() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA").filter(|appdata| !appdata.is_empty())?;
    Some(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\conflux.lnk"))
}

pub fn windows_write_start_menu_shortcut(exe: &Path) -> bool {
    let Some(shortcut) = windows_start_menu_shortcut() else {
        return false;
    };
    let script = "$link = (New-Object -ComObject WScript.Shell).CreateShortcut($env:CONFLUX_SHORTCUT); \
                  $link.TargetPath = \"$env:SystemRoot\\System32\\conhost.exe\"; \
                  $link.Arguments = '--headless \"' + $env:CONFLUX_EXE + '\" tray'; \
                  $link.IconLocation = $env:CONFLUX_EXE + ',0'; \
                  $link.Save()";
    let mut command = Command::new("powershell");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("CONFLUX_SHORTCUT", shortcut)
        .env("CONFLUX_EXE", exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    command.output().is_ok_and(|output| output.status.success())
}

fn windows_stop() {
    let not_me = format!("PID ne {}", std::process::id());
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", "conflux.exe", "/FI", &not_me])
        .output();
}

fn windows_stop_service() {
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or(Path::new("")).display().to_string();
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name = 'conflux.exe' AND ProcessId <> {}\" | \
         Where-Object {{ $_.ExecutablePath -and [IO.Path]::GetDirectoryName($_.ExecutablePath) -eq '{}' -and \
         $_.CommandLine -match '\\s\"?(serve|worker)\"?\\s*$' }} | \
         ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}",
        std::process::id(),
        dir.replace('\'', "''")
    );
    let stopped = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .is_ok_and(|o| o.status.success());
    if !stopped {
        windows_stop();
    }
}

#[cfg(windows)]
pub fn keep_std_handles_private() {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe { SetHandleInformation(GetStdHandle(id), HANDLE_FLAG_INHERIT, 0) };
    }
}

#[cfg(windows)]
fn windows_start(exe: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    keep_std_handles_private();
    Command::new(exe)
        .arg("serve")
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start conflux")?;
    Ok(())
}

#[cfg(not(windows))]
fn windows_start(_exe: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn windows_installed() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_ANY, RegGetValueW};
    let wide = |text: &str| text.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let value = wide("conflux");
    let mut size = 0u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_ANY,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    read == 0
}

#[cfg(not(windows))]
fn windows_installed() -> bool {
    false
}

pub fn systemd_unit(exe: &Path) -> String {
    format!(
        "[Unit]\nDescription=conflux subscription proxy\nAfter=network-online.target tailscaled.service\nWants=network-online.target\n\n[Service]\nExecStart={} serve\nRestart=always\nRestartSec=3\nEnvironment=CONFLUX_LOG=info\n\n[Install]\nWantedBy=default.target\n",
        exe.display()
    )
}

fn xml(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn launchd_plist(exe: &Path, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml(exe),
        xml(log),
        xml(log)
    )
}

fn menu_bar_plist(app: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{MENU_BAR_LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>AbandonProcessGroup</key><true/>
</dict>
</plist>
"#,
        xml(&app.join("Contents/MacOS/Conflux"))
    )
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("run {cmd}"))?;
    if !status.success() {
        bail!("`{cmd} {}` failed", args.join(" "));
    }
    Ok(())
}

fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn plist_path_for(label: &str) -> Result<PathBuf> {
    Ok(home()?.join("Library/LaunchAgents").join(format!("{label}.plist")))
}

fn plist_path() -> Result<PathBuf> {
    plist_path_for(LAUNCHD_LABEL)
}

fn launchd_unload(label: &str) -> Result<()> {
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{label}", uid())])
        .output();
    let path = plist_path_for(label)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn launchd_loaded_with(label: &str, plist: &str) -> Result<bool> {
    let same = std::fs::read_to_string(plist_path_for(label)?).is_ok_and(|on_disk| on_disk == plist);
    Ok(same
        && Command::new("launchctl")
            .args(["print", &format!("gui/{}/{label}", uid())])
            .output()
            .is_ok_and(|o| o.status.success()))
}

fn launchd_load(label: &str, plist: &str) -> Result<()> {
    launchd_unload(label)?;
    let path = plist_path_for(label)?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, plist)?;
    run(
        "launchctl",
        &["bootstrap", &format!("gui/{}", uid()), &path.display().to_string()],
    )
}

fn unit_path() -> Result<PathBuf> {
    Ok(home()?.join(".config/systemd/user").join(UNIT_NAME))
}

pub fn install() -> Result<String> {
    let exe = crate::update::running_exe()?;
    if cfg!(windows) {
        let launch = windows_launch_command(&exe);
        let added = Command::new("reg")
            .args([
                "add",
                WINDOWS_RUN_KEY,
                "/v",
                "conflux",
                "/t",
                "REG_SZ",
                "/d",
                &launch,
                "/f",
            ])
            .output()
            .context("run reg")?;
        if !added.status.success() {
            bail!(
                "could not add the startup entry: {}",
                String::from_utf8_lossy(&added.stderr).trim()
            );
        }
        if !windows_write_start_menu_shortcut(&exe) {
            eprintln!("Warning: could not add conflux to the Start menu. Run `conflux tray` to open the tray app.");
        }
        windows_stop_service();
        windows_start(&exe)?;
        Ok("a startup entry that runs conflux and its tray app when you sign in".to_string())
    } else if cfg!(target_os = "macos") {
        let log = home()?.join("Library/Logs/conflux.log");
        launchd_load(LAUNCHD_LABEL, &launchd_plist(&exe, &log))?;
        match crate::update::app_bundle(&exe) {
            Some(app) => {
                let plist = menu_bar_plist(app);
                if !launchd_loaded_with(MENU_BAR_LABEL, &plist)?
                    && let Err(e) = launchd_load(MENU_BAR_LABEL, &plist)
                {
                    eprintln!("Warning: the menu bar app will not open at login: {e:#}");
                }
            }
            None => launchd_unload(MENU_BAR_LABEL)?,
        }
        Ok(format!("launchd agent {LAUNCHD_LABEL} (logs in {})", log.display()))
    } else {
        let path = unit_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, systemd_unit(&exe))?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", UNIT_NAME])?;
        run("systemctl", &["--user", "restart", UNIT_NAME])?;
        let user = std::env::var("USER").unwrap_or_default();
        let lingering = !user.is_empty()
            && Command::new("loginctl")
                .args(["enable-linger", &user])
                .status()
                .is_ok_and(|status| status.success());
        if !lingering {
            eprintln!(
                "Could not enable linger. conflux will stop when you log out unless an admin enables linger for your user."
            );
        }
        Ok(format!(
            "systemd user service {UNIT_NAME} (logs: journalctl --user -u conflux)"
        ))
    }
}

pub fn restart() -> Result<()> {
    if cfg!(windows) {
        windows_stop_service();
        windows_start(&std::env::current_exe().context("find the conflux binary")?)
    } else if cfg!(target_os = "macos") {
        run(
            "launchctl",
            &["kickstart", "-k", &format!("gui/{}/{LAUNCHD_LABEL}", uid())],
        )
    } else {
        run("systemctl", &["--user", "restart", UNIT_NAME])
    }
}

pub fn installed() -> bool {
    if cfg!(windows) {
        return windows_installed();
    }
    if cfg!(target_os = "macos") {
        plist_path()
    } else {
        unit_path()
    }
    .is_ok_and(|p| p.exists())
}

pub fn uninstall() -> Result<()> {
    if cfg!(windows) {
        windows_stop();
        let _ = Command::new("reg")
            .args(["delete", WINDOWS_RUN_KEY, "/v", "conflux", "/f"])
            .output();
        if let Some(shortcut) = windows_start_menu_shortcut() {
            let _ = std::fs::remove_file(shortcut);
        }
    } else if cfg!(target_os = "macos") {
        launchd_unload(LAUNCHD_LABEL)?;
        launchd_unload(MENU_BAR_LABEL)?;
    } else {
        let path = unit_path()?;
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", UNIT_NAME])
            .status();
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_command_quotes_the_path() {
        let c = windows_launch_command(Path::new(r"C:\Users\Me Too\conflux.exe"));
        assert_eq!(c, r#"conhost.exe --headless "C:\Users\Me Too\conflux.exe" serve"#);
    }
}
