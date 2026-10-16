//! Application settings and login autostart.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::types::ServerType;

pub const APP_ID: &str = "io.github.avanatiker.eduvpn-rs";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastServer {
    pub kind: ServerType,
    pub id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Connect to the last used server when the app starts.
    pub autoconnect: bool,
    /// Start hidden in the tray (used when launched at login).
    pub start_minimized: bool,
    /// Closing the window hides it to the tray instead of quitting.
    pub close_to_tray: bool,
    /// Keep access to the local network (RFC1918) while on a full-tunnel WireGuard VPN.
    pub allow_lan: bool,
    /// Prefer TCP over UDP (for networks that block UDP).
    pub prefer_tcp: bool,
    pub last_server: Option<LastServer>,
    /// "<server id>|<network uuid>" -> unix time when UDP was found to be blocked there.
    pub udp_blocked: HashMap<String, u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autoconnect: true,
            start_minimized: true,
            close_to_tray: true,
            allow_lan: true,
            prefer_tcp: false,
            last_server: None,
            udp_blocked: HashMap::new(),
        }
    }
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
}

/// Directory used by eduvpn-common. Shared with the official client so servers carry over.
pub fn common_dir() -> PathBuf {
    config_home().join("eduvpn")
}

fn settings_path() -> PathBuf {
    config_home().join("eduvpn-rs").join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read_to_string(settings_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = settings_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, serde_json::to_string_pretty(self).unwrap()) {
            log::error!("saving settings: {e}");
        }
    }
}

/// How long to skip the UDP attempt after it failed on a network.
const UDP_BLOCKED_TTL: u64 = 14 * 24 * 3600;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Settings {
    pub fn is_udp_blocked(&self, key: &str) -> bool {
        self.udp_blocked
            .get(key)
            .is_some_and(|t| now().saturating_sub(*t) < UDP_BLOCKED_TTL)
    }

    pub fn mark_udp_blocked(&mut self, key: String) {
        self.udp_blocked
            .retain(|_, t| now().saturating_sub(*t) < UDP_BLOCKED_TTL);
        self.udp_blocked.insert(key, now());
    }
}

fn autostart_path() -> PathBuf {
    config_home()
        .join("autostart")
        .join(format!("{APP_ID}.desktop"))
}

pub fn autostart_enabled() -> bool {
    autostart_path().exists()
}

pub fn set_autostart(enabled: bool) -> std::io::Result<()> {
    let path = autostart_path();
    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName=eduvpn-rs\nComment=Connect to eduVPN\n\
             Exec=\"{}\" --autostart\nIcon={APP_ID}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_blocked_expires() {
        let mut s = Settings::default();
        assert!(!s.is_udp_blocked("srv|net"));
        s.mark_udp_blocked("srv|net".into());
        assert!(s.is_udp_blocked("srv|net"));
        assert!(!s.is_udp_blocked("srv|other"));
        s.udp_blocked
            .insert("srv|net".into(), now() - UDP_BLOCKED_TTL - 1);
        assert!(!s.is_udp_blocked("srv|net"));
    }

    #[test]
    fn old_settings_files_still_load() {
        let s: Settings = serde_json::from_str(r#"{"autoconnect": false}"#).unwrap();
        assert!(!s.autoconnect);
        assert!(s.close_to_tray);
        assert!(s.udp_blocked.is_empty());
    }
}
