//! NetworkManager integration over D-Bus.

use std::collections::HashMap;
use std::net::{IpAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ipnet::IpNet;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const ACTIVE_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";

pub const IFACE_NAME: &str = "eduVPN";
const CONNECTION_ID: &str = "eduVPN";
const DEFAULT_FWMARK: u32 = 51860;
const TABLE_MAIN: u32 = 254;

/// NMActiveConnectionState
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveState {
    Unknown,
    Activating,
    Activated,
    Deactivating,
    Deactivated,
}

impl From<u32> for ActiveState {
    fn from(v: u32) -> Self {
        match v {
            1 => Self::Activating,
            2 => Self::Activated,
            3 => Self::Deactivating,
            4 => Self::Deactivated,
            _ => Self::Unknown,
        }
    }
}

type Section = HashMap<&'static str, Value<'static>>;
type ConnSettings = HashMap<&'static str, Section>;

#[derive(Clone)]
pub struct Nm {
    conn: Connection,
}

/// Parameters needed to route WireGuard through ProxyGuard (WireGuard over TCP).
pub struct ProxyRouting {
    pub listen_port: u16,
    pub source_port: u16,
    pub peer_ips: Vec<String>,
    pub peer_port: u16,
}

/// Result of creating a WireGuard connection.
pub struct WgSetup {
    pub gateway_subnet: Option<String>,
    pub listen_port: u16,
}

impl Nm {
    pub fn new() -> Result<Self> {
        Ok(Self { conn: Connection::system().context("connecting to the system D-Bus")? })
    }

    fn proxy(&self, path: &str, iface: &str) -> Result<Proxy<'static>> {
        Ok(Proxy::new(&self.conn, NM, path.to_owned(), iface.to_owned())?)
    }

    fn prop<T: TryFrom<OwnedValue>>(&self, path: &str, iface: &str, name: &str) -> Result<T>
    where
        T::Error: Into<zbus::Error>,
    {
        Ok(self.proxy(path, iface)?.get_property::<T>(name)?)
    }

    /// Global NetworkManager state (NMState). 70 = connected with internet.
    pub fn global_state(&self) -> u32 {
        self.prop(NM_PATH, NM, "State").unwrap_or(0)
    }

    pub fn connection_path(&self, uuid: &str) -> Option<OwnedObjectPath> {
        self.proxy(SETTINGS_PATH, "org.freedesktop.NetworkManager.Settings")
            .ok()?
            .call("GetConnectionByUuid", &(uuid,))
            .ok()
    }

    pub fn active_connections(&self) -> Vec<OwnedObjectPath> {
        self.prop(NM_PATH, NM, "ActiveConnections").unwrap_or_default()
    }

    /// Finds the active connection of the given connection uuid.
    pub fn active_by_uuid(&self, uuid: &str) -> Option<OwnedObjectPath> {
        self.active_connections().into_iter().find(|p| {
            self.prop::<String>(p.as_str(), ACTIVE_IFACE, "Uuid").is_ok_and(|u| u == uuid)
        })
    }

    pub fn active_state(&self, active: &str) -> ActiveState {
        self.prop::<u32>(active, ACTIVE_IFACE, "State").map(ActiveState::from).unwrap_or(ActiveState::Unknown)
    }

    pub fn delete_connection(&self, uuid: &str) -> Result<()> {
        if let Some(path) = self.connection_path(uuid) {
            self.proxy(path.as_str(), "org.freedesktop.NetworkManager.Settings.Connection")?
                .call_method("Delete", &())?;
        }
        Ok(())
    }

    /// Activates a connection and waits until it is up.
    pub fn activate(&self, uuid: &str, cancel: &AtomicBool) -> Result<OwnedObjectPath> {
        let path = self.connection_path(uuid).context("VPN connection not found in NetworkManager")?;
        let root = OwnedObjectPath::try_from("/").unwrap();
        let active: OwnedObjectPath =
            self.proxy(NM_PATH, NM)?.call("ActivateConnection", &(&path, &root, &root))?;
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = self.deactivate(active.as_str());
                bail!("cancelled");
            }
            match self.active_state(active.as_str()) {
                ActiveState::Activated => return Ok(active),
                ActiveState::Deactivated | ActiveState::Deactivating => {
                    bail!("NetworkManager could not establish the VPN connection")
                }
                // The object disappears when activation fails.
                ActiveState::Unknown if self.active_by_uuid(uuid).is_none() => {
                    bail!("NetworkManager could not establish the VPN connection")
                }
                _ => {}
            }
            if Instant::now() > deadline {
                let _ = self.deactivate(active.as_str());
                bail!("timed out waiting for the VPN connection");
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    pub fn deactivate(&self, active: &str) -> Result<()> {
        let path = OwnedObjectPath::try_from(active)?;
        self.proxy(NM_PATH, NM)?.call_method("DeactivateConnection", &(&path,))?;
        Ok(())
    }

    /// Deactivates (if active) and removes the connection with this uuid.
    pub fn disconnect(&self, uuid: &str) -> Result<()> {
        if let Some(active) = self.active_by_uuid(uuid) {
            self.deactivate(active.as_str())?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.active_by_uuid(uuid).is_some() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        self.delete_connection(uuid)
    }

    fn add_unsaved(&self, settings: ConnSettings) -> Result<()> {
        let _: OwnedObjectPath = self
            .proxy(SETTINGS_PATH, "org.freedesktop.NetworkManager.Settings")?
            .call("AddConnectionUnsaved", &(settings,))
            .context("adding the connection to NetworkManager")?;
        Ok(())
    }

    /// Creates the WireGuard connection. Returns details needed for failover.
    pub fn add_wireguard(
        &self,
        uuid: &str,
        config: &str,
        default_gateway: bool,
        allow_lan: bool,
        proxy: Option<&ProxyRouting>,
    ) -> Result<WgSetup> {
        let ini = parse_ini(config);
        let iface = ini.get("Interface").context("WireGuard config has no [Interface]")?;
        let peer = ini.get("Peer").context("WireGuard config has no [Peer]")?;

        let fwmark: u32 = env_num("EDUVPN_WG_FWMARK").unwrap_or(DEFAULT_FWMARK);
        let mut listen_port: u16 = env_num("EDUVPN_WG_LISTEN_PORT").unwrap_or(0);
        if let Some(p) = proxy
            && listen_port == 0
        {
            // NM does not report the port it picks, so choose one ourselves.
            listen_port = free_udp_port_except(p.listen_port)?;
        }

        let lan4: Vec<IpNet> = if allow_lan {
            ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"].iter().map(|s| s.parse().unwrap()).collect()
        } else {
            vec![]
        };
        let lan6: Vec<IpNet> = if allow_lan { vec!["fc00::/7".parse().unwrap()] } else { vec![] };

        let mut addrs4 = Vec::new();
        let mut addrs6 = Vec::new();
        let mut exclude4 = Vec::new();
        let mut exclude6 = Vec::new();
        let mut first4 = None;
        let mut first6 = None;
        for a in split_list(iface.get("Address").map(String::as_str).unwrap_or("")) {
            let net: IpNet = a.parse().with_context(|| format!("invalid address {a}"))?;
            let first = if net.addr().is_ipv4() { &mut first4 } else { &mut first6 };
            first.get_or_insert_with(|| net.to_string());
            let (addrs, lans, excl) = match net {
                IpNet::V4(_) => (&mut addrs4, &lan4, &mut exclude4),
                IpNet::V6(_) => (&mut addrs6, &lan6, &mut exclude6),
            };
            addrs.push(address_data(net.addr(), net.prefix_len()));
            // Traffic to our own VPN range must not be caught by the LAN exceptions.
            if lans.iter().any(|l| l.contains(&net.addr())) {
                excl.push(net);
            }
        }

        let mut dns4: Vec<u32> = Vec::new();
        let mut dns6: Vec<Vec<u8>> = Vec::new();
        let mut search: Vec<String> = Vec::new();
        for d in split_list(iface.get("DNS").map(String::as_str).unwrap_or("")) {
            match d.parse::<IpAddr>() {
                Ok(IpAddr::V4(v4)) => dns4.push(u32::from_ne_bytes(v4.octets())),
                Ok(IpAddr::V6(v6)) => dns6.push(v6.octets().to_vec()),
                Err(_) => search.push(d.to_owned()),
            }
        }

        // Rule priorities: [fwmark rule, proxy rule, lan rule, exclude rule]
        let prios: [u32; 4] = match (proxy.is_some(), allow_lan) {
            (true, true) => [4, 3, 2, 1],
            (true, false) => [2, 1, 0, 0],
            (false, true) => [3, 0, 2, 1],
            (false, false) => [1, 0, 0, 0],
        };

        let build_ip = |v6: bool, addrs: Vec<HashMap<&'static str, Value<'static>>>, excl: &[IpNet], lans: &[IpNet]| {
            let family: i32 = if v6 { 10 } else { 2 };
            let host_len: u8 = if v6 { 128 } else { 32 };
            let mut rules: Vec<HashMap<&'static str, Value<'static>>> = Vec::new();
            rules.push(HashMap::from([
                ("family", Value::from(family)),
                ("priority", Value::from(prios[0])),
                ("invert", Value::from(true)),
                ("fwmark", Value::from(fwmark)),
                ("fwmask", Value::from(0xffff_ffffu32)),
                ("table", Value::from(fwmark)),
            ]));
            if let Some(p) = proxy {
                for ip in &p.peer_ips {
                    let Ok(addr) = ip.parse::<IpAddr>() else { continue };
                    if addr.is_ipv6() != v6 {
                        continue;
                    }
                    rules.push(HashMap::from([
                        ("family", Value::from(family)),
                        ("priority", Value::from(prios[1])),
                        ("to", Value::from(addr.to_string())),
                        ("to-len", Value::from(host_len)),
                        ("sport-start", Value::from(p.source_port)),
                        ("sport-end", Value::from(p.source_port)),
                        ("dport-start", Value::from(p.peer_port)),
                        ("dport-end", Value::from(p.peer_port)),
                        ("ipproto", Value::from(6u8)),
                        ("table", Value::from(TABLE_MAIN)),
                    ]));
                }
            }
            if allow_lan {
                for net in excl {
                    rules.push(HashMap::from([
                        ("family", Value::from(family)),
                        ("priority", Value::from(prios[3])),
                        ("to", Value::from(net.addr().to_string())),
                        ("to-len", Value::from(net.prefix_len())),
                        ("table", Value::from(fwmark)),
                        ("suppress-prefixlength", Value::from(0i32)),
                    ]));
                }
                for lan in lans {
                    rules.push(HashMap::from([
                        ("family", Value::from(family)),
                        ("priority", Value::from(prios[2])),
                        ("to", Value::from(lan.network().to_string())),
                        ("to-len", Value::from(lan.prefix_len())),
                        ("table", Value::from(TABLE_MAIN)),
                        ("suppress-prefixlength", Value::from(0i32)),
                    ]));
                }
            }
            let mut s: Section = HashMap::from([
                ("method", Value::from("manual")),
                ("address-data", Value::from(addrs)),
                ("route-table", Value::from(fwmark)),
                ("routing-rules", Value::from(rules)),
            ]);
            let mut search = search.clone();
            if default_gateway {
                // Avoid DNS leaks: send all queries to the VPN DNS servers.
                s.insert("dns-priority", Value::from(i32::MIN));
                search.insert(0, "~.".into());
            }
            if !search.is_empty() {
                s.insert("dns-search", Value::from(search));
            }
            if v6 {
                if !dns6.is_empty() {
                    s.insert("dns", Value::from(dns6.clone()));
                }
            } else if !dns4.is_empty() {
                s.insert("dns", Value::from(dns4.clone()));
            }
            s
        };

        let mut wg_peer: HashMap<&'static str, Value<'static>> = HashMap::new();
        let endpoint = match proxy {
            Some(p) => format!("127.0.0.1:{}", p.listen_port),
            None => peer.get("Endpoint").cloned().context("WireGuard config has no Endpoint")?,
        };
        wg_peer.insert("endpoint", Value::from(endpoint));
        wg_peer.insert(
            "public-key",
            Value::from(peer.get("PublicKey").cloned().context("WireGuard config has no PublicKey")?),
        );
        let allowed: Vec<String> =
            split_list(peer.get("AllowedIPs").map(String::as_str).unwrap_or("")).map(str::to_owned).collect();
        wg_peer.insert("allowed-ips", Value::from(allowed));
        if let Some(psk) = peer.get("PresharedKey") {
            wg_peer.insert("preshared-key", Value::from(psk.clone()));
            wg_peer.insert("preshared-key-flags", Value::from(0u32));
        }
        if let Some(ka) = peer.get("PersistentKeepalive").and_then(|k| k.parse::<u32>().ok())
            && ka > 0
        {
            wg_peer.insert("persistent-keepalive", Value::from(ka));
        }

        let gateway_subnet = first4.or(first6);
        let mtu = iface.get("MTU").and_then(|m| m.parse::<u32>().ok());
        let mut wireguard: Section = HashMap::from([
            (
                "private-key",
                Value::from(iface.get("PrivateKey").cloned().context("WireGuard config has no PrivateKey")?),
            ),
            ("fwmark", Value::from(fwmark)),
            ("listen-port", Value::from(u32::from(listen_port))),
            ("ip4-auto-default-route", Value::from(0i32)),
            ("ip6-auto-default-route", Value::from(0i32)),
            ("peers", Value::from(vec![wg_peer])),
        ]);
        if let Some(m) = mtu {
            wireguard.insert("mtu", Value::from(m));
        }

        let mut settings: ConnSettings = HashMap::new();
        settings.insert("connection", connection_section(uuid, "wireguard", Some(IFACE_NAME)));
        settings.insert("wireguard", wireguard);
        if !addrs4.is_empty() {
            settings.insert("ipv4", build_ip(false, addrs4, &exclude4, &lan4));
        }
        if !addrs6.is_empty() {
            settings.insert("ipv6", build_ip(true, addrs6, &exclude6, &lan6));
        } else {
            settings.insert("ipv6", HashMap::from([("method", Value::from("disabled"))]));
        }
        self.add_unsaved(settings)?;
        Ok(WgSetup { gateway_subnet, listen_port })
    }

    /// Creates an OpenVPN connection using the NetworkManager-openvpn importer. Returns the new uuid.
    pub fn add_openvpn(&self, config: &str, default_gateway: bool, dns_search: &[String]) -> Result<String> {
        let dir = std::env::temp_dir().join(format!("eduvpn-rs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)?;
        let file = dir.join(format!("{CONNECTION_ID}.ovpn"));
        crate::keyring::write_private(&file, config)?;
        let out = std::process::Command::new("nmcli")
            .args(["--terse", "connection", "import", "type", "openvpn", "file"])
            .arg(&file)
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        let out = out.context("running nmcli (is NetworkManager installed?)")?;
        if !out.status.success() {
            bail!(
                "importing the OpenVPN config failed (is networkmanager-openvpn installed?): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let uuid = stdout
            .split(['(', ')'])
            .nth(1)
            .filter(|u| uuid::Uuid::parse_str(u).is_ok())
            .ok_or_else(|| anyhow!("unexpected nmcli output: {stdout}"))?
            .to_owned();

        let mut search: Vec<String> = Vec::new();
        if default_gateway {
            search.push("~.".into());
        }
        search.extend(dns_search.iter().cloned());
        let never_default = if default_gateway { "no" } else { "yes" };
        let mut args: Vec<String> = vec![
            "connection".into(),
            "modify".into(),
            uuid.clone(),
            "connection.autoconnect".into(),
            "no".into(),
            "connection.permissions".into(),
            format!("user:{}", user_name()),
            "ipv4.never-default".into(),
            never_default.into(),
            "ipv6.never-default".into(),
            never_default.into(),
        ];
        if default_gateway {
            for p in ["ipv4.dns-priority", "ipv6.dns-priority"] {
                args.extend([p.to_owned(), i32::MIN.to_string()]);
            }
        }
        if !search.is_empty() {
            for p in ["ipv4.dns-search", "ipv6.dns-search"] {
                args.extend([p.to_owned(), search.join(",")]);
            }
        }
        let out = std::process::Command::new("nmcli").args(&args).output()?;
        if !out.status.success() {
            let _ = self.delete_connection(&uuid);
            bail!("configuring the OpenVPN connection failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(uuid)
    }

    fn ip4_config(&self, active: &str) -> Option<OwnedObjectPath> {
        self.prop::<OwnedObjectPath>(active, ACTIVE_IFACE, "Ip4Config").ok().filter(|p| p.as_str() != "/")
    }

    fn first_address(&self, config: &str, iface: &str) -> Option<String> {
        let data: Vec<HashMap<String, OwnedValue>> = self.prop(config, iface, "AddressData").ok()?;
        let addr = data.first()?.get("address")?;
        String::try_from(addr.try_clone().ok()?).ok()
    }

    pub fn ipv4_address(&self, active: &str) -> Option<String> {
        self.first_address(self.ip4_config(active)?.as_str(), "org.freedesktop.NetworkManager.IP4Config")
    }

    pub fn ipv6_address(&self, active: &str) -> Option<String> {
        let cfg = self.prop::<OwnedObjectPath>(active, ACTIVE_IFACE, "Ip6Config").ok()?;
        let data: Vec<HashMap<String, OwnedValue>> =
            self.prop(cfg.as_str(), "org.freedesktop.NetworkManager.IP6Config", "AddressData").ok()?;
        data.iter()
            .filter_map(|d| String::try_from(d.get("address")?.try_clone().ok()?).ok())
            .find(|a| !a.starts_with("fe80"))
    }

    pub fn ipv4_gateway(&self, active: &str) -> Option<String> {
        let cfg = self.ip4_config(active)?;
        self.prop::<String>(cfg.as_str(), "org.freedesktop.NetworkManager.IP4Config", "Gateway")
            .ok()
            .filter(|g| !g.is_empty())
    }

    /// The kernel interface carrying the tunnel. For OpenVPN NM reports the tun device as a
    /// separate active connection, which is found by matching the IP address.
    pub fn tunnel_interface(&self, active: &str) -> Option<String> {
        let ty: String = self.prop(active, ACTIVE_IFACE, "Type").ok()?;
        let mut target = OwnedObjectPath::try_from(active).ok()?;
        if ty == "vpn" {
            let ip = self.ipv4_address(active)?;
            target = self
                .active_connections()
                .into_iter()
                .filter(|p| p.as_str() != active)
                .find(|p| self.ipv4_address(p.as_str()).as_deref() == Some(ip.as_str()))?;
        }
        let devices: Vec<OwnedObjectPath> = self.prop(target.as_str(), ACTIVE_IFACE, "Devices").ok()?;
        self.prop(devices.first()?.as_str(), "org.freedesktop.NetworkManager.Device", "Interface").ok()
    }

    /// Subscribes to state changes of all active connections. Calls `f(path, state)`; runs forever.
    pub fn watch_active_states(&self, mut f: impl FnMut(&str, ActiveState)) -> Result<()> {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(NM)?
            .interface(ACTIVE_IFACE)?
            .member("StateChanged")?
            .build();
        let it = MessageIterator::for_match_rule(rule, &self.conn, Some(64))?;
        for msg in it {
            let Ok(msg) = msg else { continue };
            let header = msg.header();
            let Some(path) = header.path() else { continue };
            if let Ok((state, _reason)) = msg.body().deserialize::<(u32, u32)>() {
                f(path.as_str(), ActiveState::from(state));
            }
        }
        Ok(())
    }
}

fn connection_section(uuid: &str, ty: &str, iface: Option<&str>) -> Section {
    let mut s: Section = HashMap::from([
        ("id", Value::from(CONNECTION_ID)),
        ("uuid", Value::from(uuid.to_owned())),
        ("type", Value::from(ty.to_owned())),
        ("autoconnect", Value::from(false)),
        ("permissions", Value::from(vec![format!("user:{}:", user_name())])),
    ]);
    if let Some(i) = iface {
        s.insert("interface-name", Value::from(i.to_owned()));
    }
    s
}

fn address_data(addr: IpAddr, prefix: u8) -> HashMap<&'static str, Value<'static>> {
    HashMap::from([("address", Value::from(addr.to_string())), ("prefix", Value::from(u32::from(prefix)))])
}

fn split_list(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty())
}

fn env_num<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.parse().ok()
}

fn parse_ini(s: &str) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut section = String::new();
    for line in s.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_owned();
        } else if let Some((k, v)) = line.split_once('=') {
            out.entry(section.clone()).or_default().insert(k.trim().to_owned(), v.trim().to_owned());
        }
    }
    out
}

pub fn wireguard_proxy_endpoint(config: &str) -> Option<String> {
    parse_ini(config).get("Peer")?.get("ProxyEndpoint").cloned()
}

pub fn user_name() -> String {
    std::env::var("USER").unwrap_or_else(|_| {
        std::process::Command::new("id")
            .arg("-un")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    })
}

pub fn free_udp_port() -> Result<u16> {
    Ok(UdpSocket::bind("0.0.0.0:0")?.local_addr()?.port())
}

pub fn free_tcp_port() -> Result<u16> {
    Ok(TcpListener::bind("0.0.0.0:0")?.local_addr()?.port())
}

fn free_udp_port_except(exclude: u16) -> Result<u16> {
    for _ in 0..10 {
        let p = free_udp_port()?;
        if p != exclude {
            return Ok(p);
        }
    }
    bail!("no free UDP port found")
}

pub fn rx_bytes_path(iface: &str) -> PathBuf {
    PathBuf::from("/sys/class/net").join(iface).join("statistics/rx_bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    const WG: &str = "[Interface]\nPrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=\n\
        Address = 10.43.43.2/24, fd43:43::2/64\nDNS = 9.9.9.9, 2620:fe::fe, example.org\nMTU = 1392\n\n\
        [Peer]\nPublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\nAllowedIPs = 0.0.0.0/0, ::/0\n\
        Endpoint = 192.0.2.1:51820\nPersistentKeepalive = 25\n";

    /// Needs a running NetworkManager. The connection is only added, never activated.
    #[test]
    #[ignore]
    fn add_wireguard_connection() {
        let nm = Nm::new().unwrap();
        for proxy in [None, Some(ProxyRouting { listen_port: 40000, source_port: 40001, peer_ips: vec!["192.0.2.1".into()], peer_port: 443 })] {
            let uuid = uuid::Uuid::new_v4().to_string();
            let res = nm.add_wireguard(&uuid, WG, true, true, proxy.as_ref());
            let path = nm.connection_path(&uuid);
            if path.is_some() {
                let out = std::process::Command::new("nmcli").args(["-f", "ipv4.routing-rules,ipv6.routing-rules,ipv4.dns,ipv4.dns-search,wireguard.peer-routes", "connection", "show", &uuid]).output().unwrap();
                println!("{}", String::from_utf8_lossy(&out.stdout));
            }
            nm.delete_connection(&uuid).unwrap();
            let setup = res.unwrap();
            assert!(path.is_some());
            assert_eq!(setup.gateway_subnet.as_deref(), Some("10.43.43.2/24"));
        }
    }

    /// Needs NetworkManager-openvpn. The connection is only imported, never activated.
    #[test]
    #[ignore]
    fn add_openvpn_connection() {
        let ovpn = "client\ndev tun\nproto udp\nremote 192.0.2.1 1194\nremote-cert-tls server\n\
            <ca>\n-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIUFake\n-----END CERTIFICATE-----\n</ca>\n\
            <cert>\n-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIUFake\n-----END CERTIFICATE-----\n</cert>\n\
            <key>\n-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49Fake\n-----END PRIVATE KEY-----\n</key>\n\
            script-security 0\n";
        let nm = Nm::new().unwrap();
        let uuid = nm.add_openvpn(ovpn, true, &["example.org".into()]).unwrap();
        let out = std::process::Command::new("nmcli")
            .args(["-f", "connection.permissions,connection.autoconnect,ipv4.never-default,ipv4.dns-priority,ipv4.dns-search", "connection", "show", &uuid])
            .output()
            .unwrap();
        println!("{}", String::from_utf8_lossy(&out.stdout));
        nm.delete_connection(&uuid).unwrap();
    }
}
