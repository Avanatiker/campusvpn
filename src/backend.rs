//! Connection logic. Runs on its own thread and talks to the UI through channels.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::common::{self, State};
use crate::keyring::Keyring;
use crate::nm::{self, ActiveState, Nm, ProxyRouting};
use crate::settings::{self, LastServer, Settings};
use crate::types::{self, Candidate, Expiry, Protocol, Server, ServerType};

#[derive(Debug)]
pub enum Cmd {
    Connect {
        kind: ServerType,
        id: String,
    },
    /// Connect to the last used server, or disconnect when connected.
    Toggle,
    Disconnect,
    AddServer(Candidate),
    RemoveServer {
        kind: ServerType,
        id: String,
    },
    SetProfile(String),
    SetLocation(String),
    Renew,
    /// The VPN went down without us asking (e.g. disconnected from the system network menu).
    ExternalDown(String),
    Quit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub phase: Phase,
    pub server: Option<Server>,
    pub protocol: Option<Protocol>,
    pub expiry: Option<Expiry>,
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
}

#[derive(Debug)]
pub enum Ev {
    Fatal(String),
    Status(Box<Status>),
    Servers(Vec<Server>),
    OAuth(String),
    OAuthDone,
    AskProfile {
        cookie: usize,
        profiles: Vec<(String, String)>,
    },
    AskLocation {
        cookie: usize,
        locations: Vec<String>,
    },
    Error(String),
    Info(String),
    QuitDone,
}

pub type EvSender = async_channel::Sender<Ev>;

static CANCEL: AtomicBool = AtomicBool::new(false);

/// Handle of the running ProxyGuard tunnel. eduvpn-common frees the handle when the tunnel
/// stops, and using a freed handle aborts the process, so this is cleared (under the lock)
/// before the tunnel is stopped and checked (under the lock) before every restart.
static PROXY: Mutex<Option<usize>> = Mutex::new(None);

/// Aborts the running connect / login. Safe to call from any thread.
pub fn cancel() {
    CANCEL.store(true, Ordering::SeqCst);
    common::cancel_all();
}

fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

struct GoCallbacks {
    ev: EvSender,
    keyring: Keyring,
}

impl common::Callbacks for GoCallbacks {
    fn on_transition(&self, old: State, new: State, data: &str) {
        match new {
            State::OAuthStarted => {
                // The UI opens the browser.
                let _ = self.ev.try_send(Ev::OAuth(data.to_owned()));
            }
            State::AskProfile => {
                match serde_json::from_str::<types::RequiredAsk<types::Profiles>>(data) {
                    Ok(ask) => {
                        let _ = self.ev.try_send(Ev::AskProfile {
                            cookie: ask.cookie,
                            profiles: ask.data.sorted(),
                        });
                    }
                    Err(e) => log::error!("bad profile request: {e}"),
                }
            }
            State::AskLocation => {
                match serde_json::from_str::<types::RequiredAsk<Vec<String>>>(data) {
                    Ok(ask) => {
                        let _ = self.ev.try_send(Ev::AskLocation {
                            cookie: ask.cookie,
                            locations: ask.data,
                        });
                    }
                    Err(e) => log::error!("bad location request: {e}"),
                }
            }
            _ if old == State::OAuthStarted => {
                let _ = self.ev.try_send(Ev::OAuthDone);
            }
            _ => {}
        }
    }

    fn load_tokens(&self, server_id: &str, kind: ServerType) -> Option<String> {
        self.keyring.load(server_id, kind)
    }

    fn save_tokens(&self, server_id: &str, kind: ServerType, tokens: &str) {
        self.keyring.save(server_id, kind, tokens)
    }
}

/// State shared with the NetworkManager watcher thread.
#[derive(Default)]
struct Shared {
    /// D-Bus path of our active VPN connection, cleared before we disconnect on purpose.
    active: Option<String>,
    /// NetworkManager uuid of our VPN connection, set before it is activated.
    uuid: Option<String>,
}

struct Backend {
    nm: Nm,
    ev: EvSender,
    settings: Arc<Mutex<Settings>>,
    status: Status,
    shared: Arc<Mutex<Shared>>,
    keyring: Keyring,
    uuid_file: PathBuf,
    /// First address of the WireGuard interface, used to derive the gateway for failover.
    wg_subnet: Option<String>,
}

pub fn spawn(settings: Arc<Mutex<Settings>>, ev: EvSender, tx: Sender<Cmd>, rx: Receiver<Cmd>) {
    std::thread::Builder::new()
        .name("backend".into())
        .spawn(move || {
            let mut b = match Backend::start(settings, ev.clone(), tx) {
                Ok(b) => b,
                Err(e) => {
                    let _ = ev.send_blocking(Ev::Fatal(format!("{e:#}")));
                    return;
                }
            };
            b.startup();
            for cmd in rx {
                let quit = matches!(cmd, Cmd::Quit);
                b.handle(cmd);
                if quit {
                    break;
                }
            }
        })
        .expect("spawning backend thread");
}

impl Backend {
    fn start(settings: Arc<Mutex<Settings>>, ev: EvSender, tx: Sender<Cmd>) -> Result<Self> {
        let nm = Nm::new()?;
        let dir = settings::common_dir();
        let lib = common::init(
            &dir,
            env!("CARGO_PKG_VERSION"),
            Box::new(GoCallbacks {
                ev: ev.clone(),
                keyring: Keyring::new(dir.clone()),
            }),
        )?;
        log::info!("loaded {}", lib.display());

        let shared = Arc::new(Mutex::new(Shared::default()));
        spawn_watcher(nm.clone(), shared.clone(), tx);

        Ok(Self {
            nm,
            ev,
            settings,
            status: Status::default(),
            shared,
            keyring: Keyring::new(dir.clone()),
            uuid_file: dir.join("uuid"),
            wg_subnet: None,
        })
    }

    fn emit(&self, ev: Ev) {
        let _ = self.ev.send_blocking(ev);
    }

    fn publish(&self) {
        self.emit(Ev::Status(Box::new(self.status.clone())));
    }

    fn set_phase(&mut self, phase: Phase) {
        self.status.phase = phase;
        if phase != Phase::Connected {
            self.status.ipv4 = None;
            self.status.ipv6 = None;
        }
        self.publish();
    }

    fn refresh_servers(&self) {
        match common::server_list() {
            Ok(list) => self.emit(Ev::Servers(list)),
            Err(e) => log::error!("server list: {e:#}"),
        }
    }

    fn uuid(&self) -> Option<String> {
        std::fs::read_to_string(&self.uuid_file)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    fn set_uuid(&self, uuid: &str) {
        if let Err(e) = std::fs::write(&self.uuid_file, uuid) {
            log::error!("saving connection uuid: {e}");
        }
    }

    fn last_server(&self) -> Option<LastServer> {
        self.settings.lock().unwrap().last_server.clone()
    }

    fn startup(&mut self) {
        self.refresh_servers();
        self.status.server = common::current_server().or_else(|| {
            let last = self.last_server()?;
            common::server_list()
                .ok()?
                .into_iter()
                .find(|s| s.kind == last.kind && s.id == last.id)
        });

        // Still connected from a previous run?
        if let Some(uuid) = self.uuid() {
            match self.nm.active_by_uuid(&uuid) {
                // WireGuard over TCP needs ProxyGuard, which died with the previous process.
                // Such a connection carries no traffic, so replace it.
                Some(_) if self.nm.uses_local_endpoint(&uuid) => {
                    log::info!("previous TCP connection cannot be resumed, reconnecting");
                    common::ensure_state(State::Connected);
                    self.shared.lock().unwrap().uuid = Some(uuid.clone());
                    self.teardown();
                    if let Some(last) = self.last_server() {
                        self.publish();
                        self.wait_for_network();
                        return self.connect(last.kind, &last.id, true);
                    }
                }
                Some(active) if self.nm.active_state(active.as_str()) == ActiveState::Activated => {
                    log::info!("VPN already connected");
                    common::ensure_state(State::Connected);
                    {
                        let mut sh = self.shared.lock().unwrap();
                        sh.active = Some(active.to_string());
                        sh.uuid = Some(uuid.clone());
                    }
                    self.fill_connected_details(active.as_str());
                    self.set_phase(Phase::Connected);
                    return;
                }
                _ => {
                    let _ = self.nm.delete_connection(&uuid);
                }
            }
        }
        self.publish();

        let autoconnect = self.settings.lock().unwrap().autoconnect;
        if let (true, Some(last)) = (autoconnect, self.last_server()) {
            self.wait_for_network();
            log::info!("auto-connecting to {}", last.id);
            self.connect(last.kind, &last.id, true);
        }
    }

    /// At login the network may not be up yet; give it a moment before auto-connecting.
    fn wait_for_network(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.nm.global_state() < 70 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        log::debug!("command {cmd:?}");
        match cmd {
            Cmd::Connect { kind, id } => self.connect(kind, &id, false),
            Cmd::Toggle => match self.status.phase {
                Phase::Connected | Phase::Connecting => self.disconnect(),
                _ => match self.last_server() {
                    Some(last) => self.connect(last.kind, &last.id, false),
                    None => match common::server_list()
                        .ok()
                        .and_then(|l| l.into_iter().next())
                    {
                        Some(s) => self.connect(s.kind, &s.id, false),
                        None => self.emit(Ev::Info("Add a server first".into())),
                    },
                },
            },
            Cmd::Disconnect => self.disconnect(),
            Cmd::AddServer(c) => self.add_server(c),
            Cmd::RemoveServer { kind, id } => self.remove_server(kind, &id),
            Cmd::SetProfile(p) => self.change_and_reconnect(|| common::set_profile(&p)),
            Cmd::SetLocation(cc) => {
                let Some(org) = self.status.server.as_ref().map(|s| s.id.clone()) else {
                    return;
                };
                self.change_and_reconnect(|| common::set_secure_location(&org, &cc));
            }
            Cmd::Renew => self.renew(),
            Cmd::ExternalDown(path) => self.external_down(&path),
            Cmd::Quit => {
                // A ProxyGuard (WireGuard over TCP) tunnel lives in this process and dies with it.
                if PROXY.lock().unwrap().is_some() {
                    self.disconnect();
                }
                common::deregister();
                self.emit(Ev::QuitDone);
            }
        }
    }

    fn report(&self, what: &str, e: anyhow::Error) {
        if cancelled() {
            log::info!("{what}: cancelled");
        } else {
            log::error!("{what}: {e:#}");
            self.emit(Ev::Error(format!("{e:#}")));
        }
    }

    fn add_server(&mut self, c: Candidate) {
        CANCEL.store(false, Ordering::SeqCst);
        common::ensure_state(State::Main);
        let res = common::add_server(c.kind, &c.id);
        self.emit(Ev::OAuthDone);
        self.refresh_servers();
        match res {
            Ok(()) => {
                let id = match c.kind {
                    ServerType::SecureInternet => c.id.clone(),
                    _ => normalize_url(&c.id),
                };
                self.connect(c.kind, &id, false);
            }
            Err(e) => self.report("adding server", e),
        }
    }

    fn remove_server(&mut self, kind: ServerType, id: &str) {
        let exists =
            common::server_list().is_ok_and(|l| l.iter().any(|s| s.kind == kind && s.id == id));
        if !exists {
            return self.refresh_servers();
        }
        let is_current = self
            .status
            .server
            .as_ref()
            .is_some_and(|s| s.kind == kind && s.id == id);
        if is_current && matches!(self.status.phase, Phase::Connected | Phase::Connecting) {
            self.disconnect();
        }
        common::ensure_state(State::Main);
        if let Err(e) = common::remove_server(kind, id) {
            self.report("removing server", e);
        }
        self.keyring.clear(id, kind);
        {
            let mut s = self.settings.lock().unwrap();
            if s.last_server
                .as_ref()
                .is_some_and(|l| l.kind == kind && l.id == id)
            {
                s.last_server = None;
                s.save();
            }
        }
        if is_current {
            self.status.server = None;
            self.publish();
        }
        self.refresh_servers();
    }

    fn change_and_reconnect(&mut self, change: impl FnOnce() -> Result<()>) {
        let was_connected = self.status.phase == Phase::Connected;
        if was_connected {
            self.disconnect();
        }
        if let Err(e) = change() {
            self.report("changing server settings", e);
        }
        self.status.server = common::current_server().or(self.status.server.take());
        self.publish();
        if was_connected && let Some(s) = self.status.server.clone() {
            self.connect(s.kind, &s.id, false);
        }
    }

    fn renew(&mut self) {
        let Some(server) = self.status.server.clone() else {
            return;
        };
        if matches!(self.status.phase, Phase::Connected | Phase::Connecting) {
            self.disconnect();
        }
        CANCEL.store(false, Ordering::SeqCst);
        if let Err(e) = common::renew_session() {
            self.emit(Ev::OAuthDone);
            return self.report("renewing session", e);
        }
        self.connect(server.kind, &server.id, false);
    }

    fn connect(&mut self, kind: ServerType, id: &str, startup: bool) {
        if matches!(self.status.phase, Phase::Connected | Phase::Connecting) {
            self.disconnect();
        }
        CANCEL.store(false, Ordering::SeqCst);
        if let Some(s) = common::server_list()
            .ok()
            .and_then(|l| l.into_iter().find(|s| s.kind == kind && s.id == id))
        {
            self.status.server = Some(s);
        }
        self.set_phase(Phase::Connecting);

        // UDP being blocked is a property of the network, so remember it per server and network.
        let udp_key = self
            .nm
            .primary_connection_uuid()
            .map(|net| format!("{id}|{net}"));
        let prefer_tcp = {
            let s = self.settings.lock().unwrap();
            s.prefer_tcp
                || udp_key.as_ref().is_some_and(|k| s.is_udp_blocked(k))
                || std::env::var("EDUVPN_PREFER_TCP").is_ok_and(|v| v == "1")
        };
        if prefer_tcp {
            log::info!("using TCP for this server and network");
        }
        let res = self.try_connect(kind, id, prefer_tcp, startup, udp_key);
        self.emit(Ev::OAuthDone);
        match res {
            Ok(()) => {
                let mut s = self.settings.lock().unwrap();
                s.last_server = Some(LastServer {
                    kind,
                    id: id.to_owned(),
                });
                s.save();
            }
            Err(e) => {
                self.teardown();
                self.set_phase(Phase::Disconnected);
                if startup {
                    log::warn!("auto-connect failed: {e:#}");
                    self.emit(Ev::Info(format!("Auto-connect failed: {e:#}")));
                } else {
                    self.report("connecting", e);
                }
            }
        }
        self.refresh_servers();
    }

    fn try_connect(
        &mut self,
        kind: ServerType,
        id: &str,
        prefer_tcp: bool,
        startup: bool,
        udp_key: Option<String>,
    ) -> Result<()> {
        // GetConfig may only start from these states.
        if !(common::in_state(State::Main)
            || common::in_state(State::Disconnected)
            || common::in_state(State::GotConfig))
        {
            common::ensure_state(State::Main);
        }
        let cfg = common::get_config(kind, id, prefer_tcp, startup)?;
        if cancelled() {
            bail!("cancelled");
        }
        self.status.server = common::current_server().or(self.status.server.take());
        self.publish();
        self.bring_up(&cfg)?;

        let used_tcp = prefer_tcp || cfg.protocol == Protocol::WireGuardTcp;
        if cfg.should_failover && !used_tcp && self.failover_dropped() {
            log::info!("connection does not pass traffic, reconnecting over TCP");
            self.emit(Ev::Info(
                "UDP seems blocked on this network, switching to TCP".into(),
            ));
            if let Some(key) = udp_key {
                let mut s = self.settings.lock().unwrap();
                s.mark_udp_blocked(key);
                s.save();
            }
            self.teardown();
            self.set_phase(Phase::Connecting);
            let cfg = common::get_config(kind, id, true, startup)?;
            self.bring_up(&cfg)?;
        }
        Ok(())
    }

    /// Creates the NetworkManager connection for `cfg` and activates it.
    fn bring_up(&mut self, cfg: &types::VpnConfig) -> Result<()> {
        common::ensure_state(State::Connecting);
        if let Some(old) = self.uuid() {
            let _ = self.nm.delete_connection(&old);
        }
        let allow_lan = self.settings.lock().unwrap().allow_lan;

        let uuid = match cfg.protocol {
            Protocol::OpenVpn => {
                self.nm
                    .add_openvpn(&cfg.config, cfg.default_gateway, &cfg.dns_search_domains)?
            }
            Protocol::WireGuard | Protocol::WireGuardTcp => {
                let uuid = uuid::Uuid::new_v4().to_string();
                let mut proxy = None;
                let mut routing = None;
                if cfg.protocol == Protocol::WireGuardTcp {
                    let endpoint = nm::wireguard_proxy_endpoint(&cfg.config)
                        .context("config has no ProxyEndpoint")?;
                    let listen_port = nm::free_udp_port()?;
                    let source_port = nm::free_tcp_port()?;
                    let pg = common::ProxyGuard::new(listen_port, source_port, &endpoint)?;
                    routing = Some(ProxyRouting {
                        listen_port,
                        source_port,
                        peer_ips: pg.peer_ips()?,
                        peer_port: if endpoint.starts_with("http://") {
                            80
                        } else {
                            443
                        },
                    });
                    proxy = Some(pg);
                }
                let setup = self.nm.add_wireguard(
                    &uuid,
                    &cfg.config,
                    cfg.default_gateway,
                    allow_lan,
                    routing.as_ref(),
                )?;
                self.wg_subnet = setup.gateway_subnet;
                if let Some(pg) = proxy {
                    let handle = pg.handle();
                    *PROXY.lock().unwrap() = Some(handle);
                    let port = setup.listen_port;
                    std::thread::spawn(move || {
                        if let Err(e) = pg.tunnel(port) {
                            log::info!("ProxyGuard tunnel ended: {e:#}");
                        }
                        let mut p = PROXY.lock().unwrap();
                        if *p == Some(handle) {
                            *p = None;
                        }
                    });
                }
                uuid
            }
            Protocol::Unknown => bail!("server returned an unknown VPN protocol"),
        };
        self.set_uuid(&uuid);
        self.shared.lock().unwrap().uuid = Some(uuid.clone());

        let active = self.nm.activate(&uuid, &CANCEL)?;
        self.shared.lock().unwrap().active = Some(active.to_string());
        common::ensure_state(State::Connected);
        self.status.protocol = Some(cfg.protocol);
        self.fill_connected_details(active.as_str());
        self.set_phase(Phase::Connected);
        Ok(())
    }

    fn fill_connected_details(&mut self, active: &str) {
        self.status.expiry = common::expiry();
        self.status.ipv4 = self.nm.ipv4_address(active);
        self.status.ipv6 = self.nm.ipv6_address(active);
        if self.status.protocol.is_none() {
            self.status.protocol = match self.nm.tunnel_interface(active) {
                Some(i) if i == nm::IFACE_NAME => Some(Protocol::WireGuard),
                Some(_) => Some(Protocol::OpenVpn),
                None => None,
            };
        }
    }

    /// Runs eduvpn-common's reachability test. Returns true if the tunnel passes no traffic.
    fn failover_dropped(&self) -> bool {
        let Some(active) = self.shared.lock().unwrap().active.clone() else {
            return false;
        };
        let Some(iface) = self.nm.tunnel_interface(&active) else {
            log::warn!("failover: tunnel interface not found");
            return false;
        };
        let wireguard = iface == nm::IFACE_NAME;
        let gateway = if wireguard {
            self.wg_subnet
                .as_deref()
                .and_then(|s| common::calculate_gateway(s).ok())
        } else {
            self.nm.ipv4_gateway(&active)
        };
        let Some(gateway) = gateway else {
            log::warn!("failover: no gateway");
            return false;
        };
        let mtu = std::fs::read_to_string(format!("/sys/class/net/{iface}/mtu"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1000);
        std::thread::sleep(Duration::from_secs(1));
        match common::start_failover(&gateway, mtu, nm::rx_bytes_path(&iface)) {
            Ok(dropped) => dropped,
            Err(e) => {
                log::warn!("failover: {e:#}");
                false
            }
        }
    }

    /// Takes the VPN down and tells the server.
    fn teardown(&mut self) {
        {
            self.shared.lock().unwrap().active = None;
            *PROXY.lock().unwrap() = None;
        }
        if common::in_state(State::Connected) || common::in_state(State::Connecting) {
            common::ensure_state(State::Disconnecting);
        }
        if let Some(uuid) = self.uuid()
            && let Err(e) = self.nm.disconnect(&uuid)
        {
            log::warn!("removing VPN connection: {e:#}");
        }
        // Stops a running ProxyGuard tunnel.
        common::cancel_all();
        if common::in_state(State::Disconnecting) || common::in_state(State::GotConfig) {
            for attempt in 0..2 {
                match common::cleanup() {
                    Ok(()) => break,
                    Err(e) => log::debug!("cleanup attempt {attempt}: {e:#}"),
                }
            }
        }
        if common::in_state(State::Disconnecting) {
            common::ensure_state(State::Disconnected);
        }
        self.status.protocol = None;
        self.status.expiry = None;
    }

    fn disconnect(&mut self) {
        if self.status.phase == Phase::Disconnected
            && self
                .uuid()
                .and_then(|u| self.nm.active_by_uuid(&u))
                .is_none()
        {
            return;
        }
        self.set_phase(Phase::Disconnecting);
        self.teardown();
        self.set_phase(Phase::Disconnected);
    }

    fn external_down(&mut self, path: &str) {
        // The event may be stale: we might have reconnected since it was sent.
        let ours = self.shared.lock().unwrap().active.as_deref() == Some(path);
        if !ours || self.status.phase != Phase::Connected {
            return;
        }
        log::info!("VPN connection went down");
        self.teardown();
        self.set_phase(Phase::Disconnected);
        self.emit(Ev::Info("The VPN connection was closed".into()));
    }
}

fn spawn_watcher(nm: Nm, shared: Arc<Mutex<Shared>>, tx: Sender<Cmd>) {
    std::thread::Builder::new()
        .name("nm-watcher".into())
        .spawn(move || {
            let res = nm.watch_active_states(|path, state| {
                let (ours, our_uuid) = {
                    let sh = shared.lock().unwrap();
                    (sh.active.as_deref() == Some(path), sh.uuid.clone())
                };
                if ours {
                    if state == ActiveState::Deactivated {
                        let _ = tx.send(Cmd::ExternalDown(path.to_owned()));
                    }
                } else if state == ActiveState::Activated
                    && let Some(handle) = *PROXY.lock().unwrap()
                    // Our own VPN coming up is not a network change.
                    && nm.active_uuid(path).is_some_and(|u| Some(u) != our_uuid)
                {
                    // The underlying network changed, ProxyGuard has to reconnect.
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(1));
                        // Holding the lock keeps teardown from stopping the tunnel meanwhile.
                        // Restart returns as soon as the running tunnel picks it up.
                        let p = PROXY.lock().unwrap();
                        if *p == Some(handle) {
                            common::ProxyGuard::restart(handle);
                        }
                    });
                }
            });
            if let Err(e) = res {
                log::error!("watching NetworkManager: {e:#}");
            }
        })
        .expect("spawning watcher thread");
}

fn normalize_url(s: &str) -> String {
    let mut s = s.trim().to_owned();
    if !s.starts_with("http://") && !s.starts_with("https://") {
        s.insert_str(0, "https://");
    }
    if !s.ends_with('/') {
        s.push('/');
    }
    s
}

/// Searches discovery for servers and organisations matching `query`.
pub fn search(query: &str, has_secure_internet: bool) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = common::disco_servers(query)
        .into_iter()
        .filter(|s| s.server_type == "institute_access")
        .map(|s| Candidate {
            kind: ServerType::InstituteAccess,
            name: match types::tr(&s.display_name) {
                n if n.is_empty() => s.base_url.clone(),
                n => n,
            },
            id: s.base_url,
        })
        .collect();
    if !has_secure_internet {
        out.extend(
            common::disco_organizations(query)
                .into_iter()
                .map(|o| Candidate {
                    kind: ServerType::SecureInternet,
                    name: types::tr(&o.display_name),
                    id: o.org_id,
                }),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_server_urls() {
        assert_eq!(normalize_url("vpn.example.org"), "https://vpn.example.org/");
        assert_eq!(
            normalize_url(" https://vpn.example.org/ "),
            "https://vpn.example.org/"
        );
        assert_eq!(
            normalize_url("http://vpn.example.org"),
            "http://vpn.example.org/"
        );
    }
}
