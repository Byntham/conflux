use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const UNIT_NAME: &str = "conflux.service";
const LAUNCHD_LABEL: &str = "dev.conflux";

fn home() -> Result<PathBuf> {
    crate::config::home_dir()
}

const WINDOWS_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

pub fn windows_launch_command(exe: &Path) -> String {
    format!(r#"conhost.exe --headless "{}" serve"#, exe.display())
}

fn windows_stop() {
    let not_me = format!("PID ne {}", std::process::id());
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", "conflux.exe", "/FI", &not_me])
        .output();
}

fn windows_start(exe: &Path) -> Result<()> {
    Command::new("conhost.exe")
        .arg("--headless")
        .arg(exe)
        .arg("serve")
        .spawn()
        .context("start conflux")?;
    Ok(())
}

pub fn systemd_unit(exe: &Path) -> String {
    format!(
        "[Unit]\nDescription=conflux subscription proxy\nAfter=network-online.target tailscaled.service\nWants=network-online.target\n\n[Service]\nExecStart={} serve\nRestart=always\nRestartSec=3\nEnvironment=CONFLUX_LOG=info\n\n[Install]\nWantedBy=default.target\n",
        exe.display()
    )
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
        exe.display(),
        log.display(),
        log.display()
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

fn plist_path() -> Result<PathBuf> {
    Ok(home()?
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

fn unit_path() -> Result<PathBuf> {
    Ok(home()?.join(".config/systemd/user").join(UNIT_NAME))
}

pub fn install() -> Result<String> {
    let exe = std::env::current_exe().context("find the conflux binary")?;
    if cfg!(windows) {
        let launch = windows_launch_command(&exe);
        run(
            "reg",
            &[
                "add",
                WINDOWS_RUN_KEY,
                "/v",
                "conflux",
                "/t",
                "REG_SZ",
                "/d",
                &launch,
                "/f",
            ],
        )?;
        windows_stop();
        windows_start(&exe)?;
        Ok("a startup entry that runs conflux when you sign in".to_string())
    } else if cfg!(target_os = "macos") {
        let path = plist_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let log = home()?.join("Library/Logs/conflux.log");
        std::fs::write(&path, launchd_plist(&exe, &log))?;
        let target = format!("gui/{}", uid());
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{target}/{LAUNCHD_LABEL}")])
            .status();
        run("launchctl", &["bootstrap", &target, &path.display().to_string()])?;
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
        windows_stop();
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
        return Command::new("reg")
            .args(["query", WINDOWS_RUN_KEY, "/v", "conflux"])
            .output()
            .is_ok_and(|o| o.status.success());
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
    } else if cfg!(target_os = "macos") {
        let path = plist_path()?;
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{LAUNCHD_LABEL}", uid())])
            .status();
        if path.exists() {
            std::fs::remove_file(path)?;
        }
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

    #[test]
    fn unit_runs_serve() {
        let u = systemd_unit(Path::new("/home/me/.local/bin/conflux"));
        assert!(u.contains("ExecStart=/home/me/.local/bin/conflux serve"));
        assert!(u.contains("Restart=always"));
        let p = launchd_plist(Path::new("/usr/local/bin/conflux"), Path::new("/tmp/c.log"));
        assert!(p.contains("<string>/usr/local/bin/conflux</string><string>serve</string>"));
    }
}
