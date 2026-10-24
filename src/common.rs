//! Bindings to the eduvpn-common Go library (libeduvpn_common.so).
//!
//! The library handles discovery, OAuth, token refresh and fetching VPN configs.
//! It is loaded at runtime, the same way the official Python client does.

use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;
use serde::Deserialize;

use crate::types::{self, ServerType};

pub const CLIENT_ID: &str = "org.eduvpn.app.linux";
pub const LIB_MAJOR: &str = "5.";

#[repr(C)]
struct DataError {
    data: *mut c_char,
    error: *mut c_char,
}

#[repr(C)]
struct IntError {
    value: c_int,
    error: *mut c_char,
}

#[repr(C)]
struct HandleError {
    handle: usize,
    error: *mut c_char,
}

type StateCb = extern "C" fn(c_int, c_int, *const c_char) -> c_int;
type TokenGetter = extern "C" fn(*const c_char, c_int, *mut c_char, usize);
type TokenSetter = extern "C" fn(*const c_char, c_int, *const c_char);
type ReadRxBytes = extern "C" fn() -> i64;
type ProxySetup = extern "C" fn(c_int);

type StrFn = unsafe extern "C" fn() -> *mut c_char;
type DataFn = unsafe extern "C" fn() -> DataError;

struct Lib {
    _lib: Library,
    register:
        unsafe extern "C" fn(*const c_char, *const c_char, *const c_char, StateCb) -> *mut c_char,
    deregister: StrFn,
    expiry_times: DataFn,
    add_server: unsafe extern "C" fn(usize, c_int, *const c_char, *const i64) -> *mut c_char,
    remove_server: unsafe extern "C" fn(c_int, *const c_char) -> *mut c_char,
    current_server: DataFn,
    server_list: DataFn,
    get_config: unsafe extern "C" fn(usize, c_int, *const c_char, c_int, c_int) -> DataError,
    set_profile_id: unsafe extern "C" fn(*const c_char) -> *mut c_char,
    set_secure_location: unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char,
    disco_servers: unsafe extern "C" fn(usize, c_int, *const c_char) -> DataError,
    disco_organizations: unsafe extern "C" fn(usize, c_int, *const c_char) -> DataError,
    cleanup: unsafe extern "C" fn(usize) -> *mut c_char,
    renew_session: unsafe extern "C" fn(usize) -> *mut c_char,
    start_failover: unsafe extern "C" fn(usize, *const c_char, c_int, ReadRxBytes) -> IntError,
    new_proxyguard:
        unsafe extern "C" fn(usize, c_int, c_int, *const c_char, ProxySetup) -> HandleError,
    proxyguard_restart: unsafe extern "C" fn(usize) -> *mut c_char,
    proxyguard_tunnel: unsafe extern "C" fn(usize, usize, c_int) -> *mut c_char,
    proxyguard_peer_ips: unsafe extern "C" fn(usize) -> DataError,
    set_state: unsafe extern "C" fn(c_int) -> *mut c_char,
    in_state: unsafe extern "C" fn(c_int) -> IntError,
    free_string: unsafe extern "C" fn(*mut c_char),
    set_token_handler: unsafe extern "C" fn(TokenGetter, TokenSetter) -> *mut c_char,
    calculate_gateway: unsafe extern "C" fn(*const c_char) -> DataError,
    cookie_new: unsafe extern "C" fn() -> usize,
    cookie_reply: unsafe extern "C" fn(usize, *const c_char) -> *mut c_char,
    cookie_delete: unsafe extern "C" fn(usize) -> *mut c_char,
    cookie_cancel: unsafe extern "C" fn(usize) -> *mut c_char,
}

/// The FSM states of eduvpn-common (client/fsm.go).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum State {
    Deregistered = 0,
    Main = 1,
    AddingServer = 2,
    OAuthStarted = 3,
    GettingConfig = 4,
    AskLocation = 5,
    AskProfile = 6,
    GotConfig = 7,
    Connecting = 8,
    Connected = 9,
    Disconnecting = 10,
    Disconnected = 11,
}

impl State {
    fn from_i32(v: i32) -> Option<Self> {
        use State::*;
        Some(match v {
            0 => Deregistered,
            1 => Main,
            2 => AddingServer,
            3 => OAuthStarted,
            4 => GettingConfig,
            5 => AskLocation,
            6 => AskProfile,
            7 => GotConfig,
            8 => Connecting,
            9 => Connected,
            10 => Disconnecting,
            11 => Disconnected,
            _ => return None,
        })
    }
}

/// Callbacks from the Go library into the application.
pub trait Callbacks: Send + Sync {
    fn on_transition(&self, old: State, new: State, data: &str);
    fn load_tokens(&self, server_id: &str, kind: ServerType) -> Option<String>;
    fn save_tokens(&self, server_id: &str, kind: ServerType, tokens: &str);
}

static LIB: OnceLock<Lib> = OnceLock::new();
static CALLBACKS: OnceLock<Box<dyn Callbacks>> = OnceLock::new();
static RX_BYTES_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
static JAR: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

#[derive(Deserialize)]
struct GoError {
    message: types::Translated,
}

fn lib() -> &'static Lib {
    LIB.get().expect("eduvpn-common not loaded")
}

/// Takes ownership of a Go allocated string.
fn take(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { (lib().free_string)(ptr) };
    Some(s)
}

fn go_error(ptr: *mut c_char) -> Result<()> {
    match take(ptr) {
        None => Ok(()),
        Some(s) if s.is_empty() => Ok(()),
        Some(s) => {
            let msg = serde_json::from_str::<GoError>(&s)
                .map(|e| types::tr(&e.message))
                .unwrap_or(s);
            Err(anyhow!(msg))
        }
    }
}

fn go_data(de: DataError) -> Result<String> {
    let data = take(de.data);
    go_error(de.error)?;
    Ok(data.unwrap_or_default())
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

/// Runs `f` with a fresh cookie that can be cancelled via [`cancel_all`].
///
/// Cookies are cgo handles: using one after `CookieDelete` makes Go panic and abort the
/// process. Every use outside the owning call therefore goes through the JAR lock, and
/// the cookie is removed from the JAR and deleted under that same lock.
fn with_cookie<T>(f: impl FnOnce(usize) -> T) -> T {
    let c = unsafe { (lib().cookie_new)() };
    JAR.lock().unwrap().get_or_insert_default().insert(c);
    let out = f(c);
    let mut jar = JAR.lock().unwrap();
    jar.get_or_insert_default().remove(&c);
    let _ = go_error(unsafe { (lib().cookie_delete)(c) });
    out
}

/// Cancels every running operation that holds a cookie (OAuth waits, tunnels, ...).
pub fn cancel_all() {
    let Some(lib) = LIB.get() else { return };
    // CookieCancel does not block, so holding the lock keeps the cookies alive meanwhile.
    let jar = JAR.lock().unwrap();
    for &c in jar.iter().flatten() {
        let _ = go_error(unsafe { (lib.cookie_cancel)(c) });
    }
}

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        dirs.push(dir.to_path_buf());
        dirs.push(dir.join("../lib/campusvpn"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".local/lib/campusvpn"));
    }
    for d in [
        "/usr/lib/campusvpn",
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/usr/lib/x86_64-linux-gnu",
    ] {
        dirs.push(d.into());
    }
    // The python-eduvpn-common package ships the library inside site-packages.
    for base in ["/usr/lib", "/usr/lib64", "/usr/local/lib"] {
        if let Ok(rd) = std::fs::read_dir(base) {
            for e in rd.flatten() {
                let name = e.file_name();
                if name.to_string_lossy().starts_with("python3") {
                    for sub in ["site-packages", "dist-packages"] {
                        dirs.push(e.path().join(sub).join("eduvpn_common/lib"));
                    }
                }
            }
        }
    }
    dirs
}

fn find_library() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("EDUVPN_COMMON_LIB") {
        return Some(p.into());
    }
    for dir in candidate_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().unwrap_or_default().to_string_lossy();
                n.starts_with(&format!("libeduvpn_common-{LIB_MAJOR}")) && n.ends_with(".so")
            })
            .collect();
        found.sort();
        if let Some(p) = found.pop() {
            return Some(p);
        }
    }
    None
}

macro_rules! sym {
    ($lib:expr, $name:literal) => {
        *unsafe { $lib.get($name) }
            .with_context(|| format!("missing symbol {}", stringify!($name)))?
    };
}

/// Loads the library and registers this client with it.
pub fn init(config_dir: &Path, version: &str, callbacks: Box<dyn Callbacks>) -> Result<PathBuf> {
    let path = find_library().context(
        "libeduvpn_common 5.x not found. Install python-eduvpn-common or set EDUVPN_COMMON_LIB",
    )?;
    let l =
        unsafe { Library::new(&path) }.with_context(|| format!("loading {}", path.display()))?;
    let loaded = Lib {
        register: sym!(l, b"Register"),
        deregister: sym!(l, b"Deregister"),
        expiry_times: sym!(l, b"ExpiryTimes"),
        add_server: sym!(l, b"AddServer"),
        remove_server: sym!(l, b"RemoveServer"),
        current_server: sym!(l, b"CurrentServer"),
        server_list: sym!(l, b"ServerList"),
        get_config: sym!(l, b"GetConfig"),
        set_profile_id: sym!(l, b"SetProfileID"),
        set_secure_location: sym!(l, b"SetSecureLocation"),
        disco_servers: sym!(l, b"DiscoServers"),
        disco_organizations: sym!(l, b"DiscoOrganizations"),
        cleanup: sym!(l, b"Cleanup"),
        renew_session: sym!(l, b"RenewSession"),
        start_failover: sym!(l, b"StartFailover"),
        new_proxyguard: sym!(l, b"NewProxyguard"),
        proxyguard_restart: sym!(l, b"ProxyguardRestart"),
        proxyguard_tunnel: sym!(l, b"ProxyguardTunnel"),
        proxyguard_peer_ips: sym!(l, b"ProxyguardPeerIPs"),
        set_state: sym!(l, b"SetState"),
        in_state: sym!(l, b"InState"),
        free_string: sym!(l, b"FreeString"),
        set_token_handler: sym!(l, b"SetTokenHandler"),
        calculate_gateway: sym!(l, b"CalculateGateway"),
        cookie_new: sym!(l, b"CookieNew"),
        cookie_reply: sym!(l, b"CookieReply"),
        cookie_delete: sym!(l, b"CookieDelete"),
        cookie_cancel: sym!(l, b"CookieCancel"),
        _lib: l,
    };
    if LIB.set(loaded).is_err() {
        bail!("eduvpn-common already initialised");
    }
    if CALLBACKS.set(callbacks).is_err() {
        bail!("callbacks already set");
    }

    // Same mode as eduvpn-common and the official client: the directory holds VPN state.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(config_dir)?;
    let name = cstr(CLIENT_ID);
    let ver = cstr(version);
    let dir = cstr(&config_dir.to_string_lossy());
    go_error(unsafe { (lib().register)(name.as_ptr(), ver.as_ptr(), dir.as_ptr(), state_cb) })?;
    go_error(unsafe { (lib().set_token_handler)(token_getter, token_setter) })?;
    Ok(path)
}

pub fn deregister() {
    if let Some(lib) = LIB.get() {
        let _ = go_error(unsafe { (lib.deregister)() });
    }
}

extern "C" fn state_cb(old: c_int, new: c_int, data: *const c_char) -> c_int {
    let data = if data.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(data) }
            .to_string_lossy()
            .into_owned()
    };
    if let (Some(cb), Some(old), Some(new)) =
        (CALLBACKS.get(), State::from_i32(old), State::from_i32(new))
    {
        cb.on_transition(old, new, &data);
    }
    // We handle every transition: OAuth, profile and location requests are all answered.
    1
}

extern "C" fn token_getter(server_id: *const c_char, kind: c_int, out: *mut c_char, len: usize) {
    if out.is_null() || len == 0 {
        return;
    }
    unsafe { *out = 0 };
    let Some(cb) = CALLBACKS.get() else { return };
    let id = unsafe { CStr::from_ptr(server_id) }.to_string_lossy();
    let Some(tokens) = cb.load_tokens(&id, ServerType::from(kind)) else {
        return;
    };
    let bytes = tokens.as_bytes();
    if bytes.len() + 1 > len {
        log::warn!("tokens do not fit in the buffer given by eduvpn-common");
        return;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len());
        *out.add(bytes.len()) = 0;
    }
}

extern "C" fn token_setter(server_id: *const c_char, kind: c_int, tokens: *const c_char) {
    let Some(cb) = CALLBACKS.get() else { return };
    let id = unsafe { CStr::from_ptr(server_id) }.to_string_lossy();
    let tokens = unsafe { CStr::from_ptr(tokens) }.to_string_lossy();
    cb.save_tokens(&id, ServerType::from(kind), &tokens);
}

extern "C" fn read_rx_bytes() -> i64 {
    let path = RX_BYTES_FILE.lock().unwrap().clone();
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(-1)
}

extern "C" fn proxy_setup(fd: c_int) {
    log::debug!("ProxyGuard socket set up, fd {fd}");
}

pub fn server_list() -> Result<Vec<types::Server>> {
    let json = go_data(unsafe { (lib().server_list)() })?;
    Ok(serde_json::from_str::<types::ServerList>(&json)?.into_servers())
}

pub fn current_server() -> Option<types::Server> {
    let json = go_data(unsafe { (lib().current_server)() }).ok()?;
    serde_json::from_str::<types::CurrentServer>(&json)
        .ok()?
        .into_server()
}

pub fn expiry() -> Option<types::Expiry> {
    let json = go_data(unsafe { (lib().expiry_times)() }).ok()?;
    serde_json::from_str(&json).ok()
}

/// Adds a server. Blocks until the user has finished logging in through the browser.
pub fn add_server(kind: ServerType, id: &str) -> Result<()> {
    let id = cstr(id);
    with_cookie(|c| {
        go_error(unsafe { (lib().add_server)(c, kind.into(), id.as_ptr(), std::ptr::null()) })
    })
}

pub fn remove_server(kind: ServerType, id: &str) -> Result<()> {
    let id = cstr(id);
    go_error(unsafe { (lib().remove_server)(kind.into(), id.as_ptr()) })
}

/// Fetches a VPN config. With `startup` set no interaction (OAuth/profile) is triggered.
pub fn get_config(
    kind: ServerType,
    id: &str,
    prefer_tcp: bool,
    startup: bool,
) -> Result<types::VpnConfig> {
    let id = cstr(id);
    let json = with_cookie(|c| {
        go_data(unsafe {
            (lib().get_config)(
                c,
                kind.into(),
                id.as_ptr(),
                prefer_tcp as c_int,
                startup as c_int,
            )
        })
    })?;
    Ok(serde_json::from_str(&json)?)
}

pub fn set_profile(profile: &str) -> Result<()> {
    let p = cstr(profile);
    go_error(unsafe { (lib().set_profile_id)(p.as_ptr()) })
}

pub fn set_secure_location(org_id: &str, country: &str) -> Result<()> {
    let (o, c) = (cstr(org_id), cstr(country));
    go_error(unsafe { (lib().set_secure_location)(o.as_ptr(), c.as_ptr()) })
}

pub fn disco_servers(search: &str) -> Vec<types::DiscoServer> {
    let s = cstr(search);
    // The library returns cached data together with an error when offline, so the error is not fatal.
    let de = with_cookie(|c| unsafe { (lib().disco_servers)(c, 0, s.as_ptr()) });
    let data = take(de.data).unwrap_or_default();
    if let Err(e) = go_error(de.error) {
        log::warn!("server discovery: {e}");
    }
    serde_json::from_str::<types::DiscoServers>(&data)
        .map(|d| d.server_list)
        .unwrap_or_default()
}

pub fn disco_organizations(search: &str) -> Vec<types::DiscoOrganization> {
    let s = cstr(search);
    let de = with_cookie(|c| unsafe { (lib().disco_organizations)(c, 0, s.as_ptr()) });
    let data = take(de.data).unwrap_or_default();
    if let Err(e) = go_error(de.error) {
        log::warn!("organization discovery: {e}");
    }
    serde_json::from_str::<types::DiscoOrganizations>(&data)
        .map(|d| d.organization_list)
        .unwrap_or_default()
}

/// Tells the server the session is over (`/disconnect`). Must be called after disconnecting.
pub fn cleanup() -> Result<()> {
    with_cookie(|c| go_error(unsafe { (lib().cleanup)(c) }))
}

/// Forgets the OAuth tokens; the next config fetch triggers a new login.
pub fn renew_session() -> Result<()> {
    with_cookie(|c| go_error(unsafe { (lib().renew_session)(c) }))
}

/// Checks if traffic still flows through the tunnel. Returns true if the connection dropped.
pub fn start_failover(gateway: &str, mtu: u32, rx_bytes_file: PathBuf) -> Result<bool> {
    *RX_BYTES_FILE.lock().unwrap() = Some(rx_bytes_file);
    let gw = cstr(gateway);
    let res = with_cookie(|c| unsafe {
        (lib().start_failover)(c, gw.as_ptr(), mtu as c_int, read_rx_bytes)
    });
    go_error(res.error)?;
    Ok(res.value != 0)
}

pub struct ProxyGuard {
    handle: usize,
}

impl ProxyGuard {
    pub fn new(listen_port: u16, source_port: u16, peer: &str) -> Result<Self> {
        let p = cstr(peer);
        let res = with_cookie(|c| unsafe {
            (lib().new_proxyguard)(
                c,
                listen_port.into(),
                source_port.into(),
                p.as_ptr(),
                proxy_setup,
            )
        });
        go_error(res.error)?;
        Ok(Self { handle: res.handle })
    }

    pub fn peer_ips(&self) -> Result<Vec<String>> {
        let json = go_data(unsafe { (lib().proxyguard_peer_ips)(self.handle) })?;
        Ok(serde_json::from_str(&json)?)
    }

    /// Runs the tunnel until cancelled with [`cancel_all`]. Blocks.
    pub fn tunnel(&self, wg_listen_port: u16) -> Result<()> {
        with_cookie(|c| {
            go_error(unsafe { (lib().proxyguard_tunnel)(c, self.handle, wg_listen_port.into()) })
        })
    }

    pub fn restart(handle: usize) {
        if let Err(e) = go_error(unsafe { (lib().proxyguard_restart)(handle) }) {
            log::warn!("restarting ProxyGuard: {e}");
        }
    }

    pub fn handle(&self) -> usize {
        self.handle
    }
}

pub fn set_state(state: State) -> Result<()> {
    go_error(unsafe { (lib().set_state)(state as c_int) })
}

pub fn in_state(state: State) -> bool {
    let res = unsafe { (lib().in_state)(state as c_int) };
    go_error(res.error).is_ok() && res.value != 0
}

/// Moves the FSM to `state` if it is not already there, ignoring invalid transitions.
pub fn ensure_state(state: State) {
    if !in_state(state)
        && let Err(e) = set_state(state)
    {
        log::debug!("state transition to {state:?}: {e}");
    }
}

pub fn calculate_gateway(subnet: &str) -> Result<String> {
    let s = cstr(subnet);
    go_data(unsafe { (lib().calculate_gateway)(s.as_ptr()) })
}

/// Answers an ASK_PROFILE / ASK_LOCATION request.
///
/// Does nothing if the request already ended (e.g. it was cancelled meanwhile).
pub fn cookie_reply(cookie: usize, data: &str) -> Result<()> {
    let d = cstr(data);
    // The lock is held across the call so the cookie cannot be deleted under us. The
    // reply only blocks until the waiting GetConfig receives it, and that wait can only
    // end through this cookie (cancel/delete), which also needs the lock.
    let jar = JAR.lock().unwrap();
    if !jar.as_ref().is_some_and(|j| j.contains(&cookie)) {
        bail!("the request is no longer active");
    }
    go_error(unsafe { (lib().cookie_reply)(cookie, d.as_ptr()) })
}

/// Aborts an ASK_PROFILE / ASK_LOCATION request.
pub fn cookie_cancel(cookie: usize) {
    let jar = JAR.lock().unwrap();
    if jar.as_ref().is_some_and(|j| j.contains(&cookie)) {
        let _ = go_error(unsafe { (lib().cookie_cancel)(cookie) });
    }
}
