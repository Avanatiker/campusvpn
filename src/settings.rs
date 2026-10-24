//! Application settings and login autostart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::types::ServerType;

pub const APP_ID: &str = "io.github.avanatiker.campusvpn";
const NAME: &str = "campusvpn";
// The project was called eduvpn-rs up to version 0.1.
const LEGACY_NAME: &str = "eduvpn-rs";
const LEGACY_APP_ID: &str = "io.github.avanatiker.eduvpn-rs";

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

pub fn cache_home() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
}

/// Directory used by eduvpn-common. Shared with the official client so servers carry over.
pub fn common_dir() -> PathBuf {
    config_home().join("eduvpn")
}

fn settings_path() -> PathBuf {
    config_home().join(NAME).join("settings.json")
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
    write_autostart(&path)
}

fn write_autostart(path: &Path) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(
        path,
        format!(
            "[Desktop Entry]\nType=Application\nName=CampusVPN\nComment=Connect to your VPN server\n\
             Exec=\"{}\" --autostart\nIcon={APP_ID}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        ),
    )
}

/// Moves the settings and autostart entry of versions released as "eduvpn-rs".
pub fn migrate_legacy() {
    migrate_legacy_in(&config_home(), &cache_home());
}

fn migrate_legacy_in(config: &Path, cache: &Path) {
    let old_dir = config.join(LEGACY_NAME);
    let old_settings = old_dir.join("settings.json");
    let new_settings = config.join(NAME).join("settings.json");
    if old_settings.exists() && !new_settings.exists() {
        let moved = std::fs::create_dir_all(new_settings.parent().unwrap())
            .and_then(|_| std::fs::rename(&old_settings, &new_settings));
        match moved {
            Ok(()) => log::info!("moved settings to {}", new_settings.display()),
            Err(e) => log::warn!("moving {}: {e}", old_settings.display()),
        }
    }
    // Only removed once empty.
    let _ = std::fs::remove_dir(&old_dir);

    let autostart = config.join("autostart");
    let old_autostart = autostart.join(format!("{LEGACY_APP_ID}.desktop"));
    if old_autostart.exists() {
        match write_autostart(&autostart.join(format!("{APP_ID}.desktop"))) {
            Ok(()) => {
                let _ = std::fs::remove_file(&old_autostart);
            }
            Err(e) => log::warn!("moving autostart entry: {e}"),
        }
    }

    // Only held the bundled icons.
    let _ = std::fs::remove_dir_all(cache.join(LEGACY_NAME));
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

    #[test]
    fn migrates_legacy_files() {
        let tmp = std::env::temp_dir().join(format!("campusvpn-test-{}", uuid::Uuid::new_v4()));
        let (config, cache) = (tmp.join("config"), tmp.join("cache"));
        std::fs::create_dir_all(config.join(LEGACY_NAME)).unwrap();
        std::fs::write(config.join(LEGACY_NAME).join("settings.json"), "{}").unwrap();
        std::fs::create_dir_all(config.join("autostart")).unwrap();
        let old_autostart = config
            .join("autostart")
            .join(format!("{LEGACY_APP_ID}.desktop"));
        std::fs::write(&old_autostart, "").unwrap();
        std::fs::create_dir_all(cache.join(LEGACY_NAME).join("icons")).unwrap();

        migrate_legacy_in(&config, &cache);

        assert!(config.join(NAME).join("settings.json").exists());
        assert!(!config.join(LEGACY_NAME).exists());
        assert!(!old_autostart.exists());
        let entry =
            std::fs::read_to_string(config.join("autostart").join(format!("{APP_ID}.desktop")))
                .unwrap();
        assert!(entry.contains("--autostart"));
        assert!(!cache.join(LEGACY_NAME).exists());
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
