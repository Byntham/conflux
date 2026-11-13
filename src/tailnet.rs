use std::net::IpAddr;
use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const TAILSCALE_COMMANDS: &[&str] = &[
    "tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    r"C:\Program Files\Tailscale\tailscale.exe",
];

const INSTALLABLE_OS: &[&str] = &["linux", "macOS", "windows"];

pub type Tailscale = Arc<dyn Fn(&[&str]) -> Option<Value> + Send + Sync>;

pub fn system_tailscale() -> Tailscale {
    Arc::new(run_tailscale)
}

fn run_tailscale(args: &[&str]) -> Option<Value> {
    let out = TAILSCALE_COMMANDS
        .iter()
        .find_map(|cmd| Command::new(cmd).args(args).env("TAILSCALE_BE_CLI", "1").output().ok())?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

pub fn status(tailscale: &Tailscale) -> Option<Value> {
    tailscale(&["status", "--json"])
}

#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub machine: TailnetSelf,
    pub os: String,
}

pub fn same_user_peers(status: &Value) -> Vec<Peer> {
    let me = &status["Self"]["UserID"];
    if me.is_null() {
        return Vec::new();
    }
    status["Peer"]
        .as_object()
        .into_iter()
        .flat_map(|peers| peers.values())
        .filter(|p| p["UserID"] == *me && p["Online"] == true)
        .filter(|p| p["Tags"].as_array().is_none_or(|tags| tags.is_empty()))
        .filter_map(|p| {
            let os = p["OS"].as_str().filter(|os| INSTALLABLE_OS.contains(os))?;
            Some(Peer {
                machine: TailnetSelf::from_peer(p),
                os: os.to_string(),
            })
        })
        .collect()
}

pub fn is_same_user(tailscale: &Tailscale, caller: IpAddr) -> bool {
    let Some(status) = status(tailscale) else {
        return false;
    };
    let Some(whois) = tailscale(&["whois", "--json", &caller.to_string()]) else {
        return false;
    };
    let me = &status["Self"]["UserID"];
    let tagged = whois["Node"]["Tags"].as_array().is_some_and(|t| !t.is_empty());
    !me.is_null() && whois["UserProfile"]["ID"] == *me && !tagged
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TailnetSelf {
    pub ips: Vec<IpAddr>,
    pub host_name: Option<String>,
    pub dns_name: Option<String>,
}

impl TailnetSelf {
    pub fn detect() -> Self {
        run_tailscale(&["status", "--json"])
            .map(|v| Self::from_status(&v))
            .unwrap_or_default()
    }

    pub fn from_status(v: &Value) -> Self {
        Self::from_peer(&v["Self"])
    }

    pub fn from_peer(me: &Value) -> Self {
        let ips = me["TailscaleIPs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|ip| ip.as_str()?.parse().ok())
            .collect();
        let s = |k: &str| {
            me[k]
                .as_str()
                .map(|s| s.trim_end_matches('.').to_lowercase())
                .filter(|s| !s.is_empty())
        };
        Self {
            ips,
            host_name: s("HostName"),
            dns_name: s("DNSName"),
        }
    }

    pub fn short_name(&self) -> Option<String> {
        self.dns_name
            .as_ref()
            .and_then(|d| d.split('.').next().map(str::to_string))
            .or(self.host_name.clone())
    }

    pub fn host_headers(&self, port: u16) -> Vec<String> {
        let mut out = Vec::new();
        for ip in &self.ips {
            out.push(match ip {
                IpAddr::V4(v4) => format!("{v4}:{port}"),
                IpAddr::V6(v6) => format!("[{v6}]:{port}"),
            });
        }
        for name in self.names() {
            out.push(format!("{name}:{port}"));
        }
        out
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = [self.short_name(), self.dns_name.clone(), self.host_name.clone()]
            .into_iter()
            .flatten()
            .collect();
        let mut seen = std::collections::HashSet::new();
        v.retain(|name| seen.insert(name.clone()));
        v
    }
}

pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xc0) == 64
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

pub async fn resolves_to_tailnet(host: &str, port: u16) -> bool {
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return is_tailnet_ip(ip);
    }
    match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();
            !addrs.is_empty() && addrs.iter().all(|a| is_tailnet_ip(a.ip()))
        }
        Err(_) => false,
    }
}

pub fn hostname() -> String {
    if let Ok(name) = std::env::var("CONFLUX_HOSTNAME") {
        return name;
    }
    Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_ranges() {
        assert!(is_tailnet_ip("100.101.102.103".parse().unwrap()));
        assert!(is_tailnet_ip("100.64.0.1".parse().unwrap()));
        assert!(!is_tailnet_ip("100.128.0.1".parse().unwrap()));
        assert!(!is_tailnet_ip("192.168.1.10".parse().unwrap()));
        assert!(is_tailnet_ip("fd7a:115c:a1e0::1234".parse().unwrap()));
        assert!(!is_tailnet_ip("fd00::1".parse().unwrap()));
    }

    #[test]
    fn parses_status() {
        let v = serde_json::json!({"Self": {
            "HostName": "hub-box",
            "DNSName": "hub-box.example.ts.net.",
            "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1234"]
        }});
        let t = TailnetSelf::from_status(&v);
        assert_eq!(t.short_name().as_deref(), Some("hub-box"));
        assert_eq!(
            t.host_headers(8317),
            vec![
                "100.101.102.103:8317",
                "[fd7a:115c:a1e0::1234]:8317",
                "hub-box:8317",
                "hub-box.example.ts.net:8317"
            ]
        );
    }
}
