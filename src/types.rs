//! JSON types exchanged with the eduvpn-common Go library.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub type Translated = HashMap<String, String>;

/// Pick the best translation for the current locale, falling back to English.
pub fn tr(map: &Translated) -> String {
    let lang = std::env::var("LANG").unwrap_or_default();
    let lang = lang.split(['_', '.']).next().unwrap_or("");
    map.get(lang)
        .or_else(|| map.get("en"))
        .or_else(|| map.values().next())
        .cloned()
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "i32", from = "i32")]
pub enum ServerType {
    Unknown,
    InstituteAccess,
    SecureInternet,
    Custom,
}

impl From<i32> for ServerType {
    fn from(v: i32) -> Self {
        match v {
            1 => Self::InstituteAccess,
            2 => Self::SecureInternet,
            3 => Self::Custom,
            _ => Self::Unknown,
        }
    }
}

impl From<ServerType> for i32 {
    fn from(t: ServerType) -> i32 {
        match t {
            ServerType::Unknown => 0,
            ServerType::InstituteAccess => 1,
            ServerType::SecureInternet => 2,
            ServerType::Custom => 3,
        }
    }
}

impl ServerType {
    /// Label used by the Python client as keyring attribute, kept for compatibility.
    pub fn keyring_label(self) -> &'static str {
        match self {
            Self::InstituteAccess => "Institute Access Server",
            Self::SecureInternet => "Secure Internet Server",
            Self::Custom => "Custom Server",
            Self::Unknown => "Unknown Server",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Self::InstituteAccess => "Institute Access",
            Self::SecureInternet => "Secure Internet",
            Self::Custom => "Own server",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub display_name: Translated,
    #[serde(default)]
    pub priority: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Profiles {
    #[serde(default)]
    pub map: HashMap<String, Profile>,
    #[serde(default)]
    pub current: String,
}

impl Profiles {
    /// Profiles sorted by priority (descending), then name.
    pub fn sorted(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self
            .map
            .iter()
            .map(|(id, p)| (p.priority, tr(&p.display_name), id.clone()))
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        v.into_iter().map(|(_, name, id)| (id, name)).collect()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct RawServer {
    #[serde(default)]
    pub display_name: Translated,
    pub identifier: String,
    #[serde(default)]
    pub profiles: Profiles,
    #[serde(default)]
    pub country_code: String,
    #[serde(default)]
    pub locations: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ServerList {
    #[serde(default)]
    pub institute_access_servers: Vec<RawServer>,
    pub secure_internet_server: Option<RawServer>,
    #[serde(default)]
    pub custom_servers: Vec<RawServer>,
}

#[derive(Debug, Deserialize)]
pub struct CurrentServer {
    pub institute_access_server: Option<RawServer>,
    pub secure_internet_server: Option<RawServer>,
    pub custom_server: Option<RawServer>,
    pub server_type: ServerType,
}

impl CurrentServer {
    pub fn into_server(self) -> Option<Server> {
        let raw = match self.server_type {
            ServerType::InstituteAccess => self.institute_access_server,
            ServerType::SecureInternet => self.secure_internet_server,
            ServerType::Custom => self.custom_server,
            ServerType::Unknown => None,
        }?;
        Some(Server::from_raw(raw, self.server_type))
    }
}

/// A configured server, flattened for the UI.
#[derive(Clone, Debug)]
pub struct Server {
    pub kind: ServerType,
    pub id: String,
    pub name: String,
    pub profiles: Profiles,
    pub country_code: String,
    pub locations: Vec<String>,
}

impl Server {
    pub fn from_raw(raw: RawServer, kind: ServerType) -> Self {
        let name = match tr(&raw.display_name) {
            n if !n.is_empty() => n,
            _ => raw.identifier.clone(),
        };
        Self {
            kind,
            id: raw.identifier,
            name,
            profiles: raw.profiles,
            country_code: raw.country_code,
            locations: raw.locations,
        }
    }

    pub fn current_profile_name(&self) -> Option<String> {
        self.profiles.map.get(&self.profiles.current).map(|p| tr(&p.display_name))
    }
}

impl ServerList {
    pub fn into_servers(self) -> Vec<Server> {
        let mut out = Vec::new();
        out.extend(
            self.institute_access_servers
                .into_iter()
                .map(|s| Server::from_raw(s, ServerType::InstituteAccess)),
        );
        out.extend(self.secure_internet_server.map(|s| Server::from_raw(s, ServerType::SecureInternet)));
        out.extend(self.custom_servers.into_iter().map(|s| Server::from_raw(s, ServerType::Custom)));
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(from = "i32")]
pub enum Protocol {
    Unknown,
    OpenVpn,
    WireGuard,
    WireGuardTcp,
}

impl From<i32> for Protocol {
    fn from(v: i32) -> Self {
        match v {
            1 => Self::OpenVpn,
            2 => Self::WireGuard,
            3 => Self::WireGuardTcp,
            _ => Self::Unknown,
        }
    }
}

impl Protocol {
    pub fn display(self) -> &'static str {
        match self {
            Self::OpenVpn => "OpenVPN",
            Self::WireGuard => "WireGuard",
            Self::WireGuardTcp => "WireGuard (TCP)",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct VpnConfig {
    pub config: String,
    pub protocol: Protocol,
    #[serde(default)]
    pub default_gateway: bool,
    #[serde(default)]
    pub dns_search_domains: Vec<String>,
    #[serde(default)]
    pub should_failover: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Expiry {
    #[serde(default)]
    pub end_time: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

/// Data for transitions that need a reply (ASK_PROFILE / ASK_LOCATION).
#[derive(Debug, Deserialize)]
pub struct RequiredAsk<T> {
    pub cookie: usize,
    pub data: T,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DiscoServer {
    pub base_url: String,
    #[serde(default)]
    pub display_name: Translated,
    pub server_type: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct DiscoServers {
    #[serde(default)]
    pub server_list: Vec<DiscoServer>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DiscoOrganization {
    pub org_id: String,
    #[serde(default)]
    pub display_name: Translated,
}

#[derive(Debug, Default, Deserialize)]
pub struct DiscoOrganizations {
    #[serde(default)]
    pub organization_list: Vec<DiscoOrganization>,
}

/// A server that can be added, from discovery or typed by the user.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub kind: ServerType,
    pub id: String,
    pub name: String,
}
