//! OAuth token storage.
//!
//! Tokens are stored in the Secret Service (KWallet / GNOME Keyring) using the same
//! schema and attributes as the official Python client, so existing logins are reused.
//! If no Secret Service is available a file in the config directory is used instead.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use secret_service::EncryptionType;
use secret_service::blocking::SecretService;
use serde_json::{Value, json};

use crate::types::{ServerType, Tokens};

const SCHEMA: &str = "eduVPN";

pub struct Keyring {
    file: PathBuf,
}

impl Keyring {
    pub fn new(config_dir: PathBuf) -> Self {
        Self { file: config_dir.join("keys") }
    }

    fn attributes(server_id: &str, kind: ServerType) -> HashMap<&str, &str> {
        HashMap::from([
            ("xdg:schema", SCHEMA),
            ("server", server_id),
            ("category", kind.keyring_label()),
        ])
    }

    /// Returns tokens in the format eduvpn-common expects.
    pub fn load(&self, server_id: &str, kind: ServerType) -> Option<String> {
        let stored = match self.load_secret_service(server_id, kind) {
            Ok(Some(s)) => s,
            Ok(None) => self.load_file(server_id, kind)?,
            Err(e) => {
                log::warn!("secret service unavailable ({e}), using file keyring");
                self.load_file(server_id, kind)?
            }
        };
        let v: Value = serde_json::from_str(&stored).ok()?;
        let expires = v.get("expires_at").or_else(|| v.get("expires"))?;
        let expires_at = match expires {
            Value::String(s) => s.parse().ok()?,
            other => other.as_i64()?,
        };
        let tokens = Tokens {
            access_token: v.get("access")?.as_str()?.to_owned(),
            refresh_token: v.get("refresh")?.as_str()?.to_owned(),
            expires_at,
        };
        serde_json::to_string(&tokens).ok()
    }

    pub fn save(&self, server_id: &str, kind: ServerType, tokens_json: &str) {
        let Ok(t) = serde_json::from_str::<Tokens>(tokens_json) else {
            log::warn!("could not parse tokens from eduvpn-common");
            return;
        };
        if t.access_token.is_empty() && t.refresh_token.is_empty() {
            return;
        }
        let stored = json!({
            "access": t.access_token,
            "refresh": t.refresh_token,
            "expires_at": t.expires_at.to_string(),
        })
        .to_string();
        if let Err(e) = self.save_secret_service(server_id, kind, &stored) {
            log::warn!("secret service unavailable ({e}), using file keyring");
            self.save_file(server_id, kind, Some(&stored));
        }
    }

    pub fn clear(&self, server_id: &str, kind: ServerType) {
        if let Ok(ss) = SecretService::connect(EncryptionType::Dh)
            && let Ok(found) = ss.search_items(Self::attributes(server_id, kind))
        {
            for item in found.unlocked.iter().chain(found.locked.iter()) {
                let _ = item.delete();
            }
        }
        self.save_file(server_id, kind, None);
    }

    fn load_secret_service(&self, server_id: &str, kind: ServerType) -> Result<Option<String>> {
        let ss = SecretService::connect(EncryptionType::Dh)?;
        let found = ss.search_items(Self::attributes(server_id, kind))?;
        let item = match (found.unlocked.first(), found.locked.first()) {
            (Some(item), _) => item,
            (None, Some(item)) => {
                item.unlock()?;
                item
            }
            (None, None) => return Ok(None),
        };
        let secret = item.get_secret()?;
        Ok(Some(String::from_utf8(secret)?))
    }

    fn save_secret_service(&self, server_id: &str, kind: ServerType, secret: &str) -> Result<()> {
        let ss = SecretService::connect(EncryptionType::Dh)?;
        let collection = ss.get_default_collection().or_else(|_| ss.get_any_collection())?;
        collection.ensure_unlocked()?;
        let label = format!("eduVPN - {server_id} - OAuth Tokens");
        collection
            .create_item(&label, Self::attributes(server_id, kind), secret.as_bytes(), true, "text/plain")
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    // Same format as the Python client's InsecureFileKeyring: {"v1": {"<server>,<category>": "<secret>"}}
    fn file_key(server_id: &str, kind: ServerType) -> String {
        format!("{server_id},{}", kind.keyring_label())
    }

    fn read_file(&self) -> serde_json::Map<String, Value> {
        std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("v1").and_then(|v| v.as_object()).cloned())
            .unwrap_or_default()
    }

    fn load_file(&self, server_id: &str, kind: ServerType) -> Option<String> {
        self.read_file()
            .get(&Self::file_key(server_id, kind))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    fn save_file(&self, server_id: &str, kind: ServerType, secret: Option<&str>) {
        let mut map = self.read_file();
        let key = Self::file_key(server_id, kind);
        match secret {
            Some(s) => {
                map.insert(key, Value::String(s.to_owned()));
            }
            None => {
                if map.remove(&key).is_none() {
                    return;
                }
            }
        }
        let content = json!({ "v1": map }).to_string();
        if let Err(e) = write_private(&self.file, &content) {
            log::error!("writing {}: {e}", self.file.display());
        }
    }
}

/// Writes a file readable only by the current user.
pub fn write_private(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(content.as_bytes())
}
