//! Main window (GTK4 + libadwaita).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::backend::{self, Cmd, Ev, Phase, Status};
use crate::common;
use crate::settings::{self, Settings};
use crate::tray::{TRAY_SHOWN, VpnTray};
use crate::types::{Candidate, Server, ServerType};

/// Messages from the tray to the UI thread.
pub enum UiMsg {
    Show,
    Toggle,
    Quit,
}

const CSS: &str = r#"
.status-badge {
    border-radius: 9999px;
    min-width: 120px;
    min-height: 120px;
    background-color: alpha(currentColor, 0.08);
    color: alpha(@window_fg_color, 0.45);
    transition: all 250ms ease-out;
}
.status-badge.connected {
    background-color: alpha(@success_color, 0.16);
    color: @success_color;
}
.status-badge.connecting {
    background-color: alpha(@warning_color, 0.16);
    color: @warning_color;
}
.main-button {
    min-width: 200px;
    min-height: 44px;
    font-weight: bold;
}
"#;

struct Ui {
    app: adw::Application,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    nav: adw::NavigationView,
    home_stack: gtk::Stack,
    badge: gtk::Box,
    badge_icon: gtk::Image,
    title: gtk::Label,
    subtitle: gtk::Label,
    details: gtk::Label,
    main_button: gtk::Button,
    servers_group: adw::PreferencesGroup,
    server_rows: RefCell<Vec<gtk::Widget>>,
    conn_group: adw::PreferencesGroup,
    conn_rows: RefCell<Vec<gtk::Widget>>,
    status: RefCell<Status>,
    servers: RefCell<Vec<Server>>,
    settings: Arc<Mutex<Settings>>,
    cmd: Sender<Cmd>,
    tray: RefCell<Option<ksni::blocking::Handle<VpnTray>>>,
    /// Dialogs waiting on eduvpn-common, with a flag set when they are closed by us
    /// rather than dismissed by the user (which would cancel the operation).
    oauth_dialog: RefCell<Option<(adw::AlertDialog, Rc<Cell<bool>>)>>,
    ask_dialog: RefCell<Option<(adw::Dialog, Rc<Cell<bool>>)>>,
    quitting: Cell<bool>,
}

pub fn run(start_hidden: bool) -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(settings::APP_ID)
        .build();
    let ui: Rc<RefCell<Option<Rc<Ui>>>> = Rc::default();
    app.connect_activate(move |app| {
        if let Some(ui) = ui.borrow().as_ref() {
            // Launched again while running: bring the window up.
            ui.show();
            return;
        }
        let built = build(app);
        if !start_hidden || built.tray.borrow().is_none() {
            built.show();
        } else {
            // Starting hidden only makes sense with a tray; give the panel time to come up.
            let weak = Rc::downgrade(&built);
            glib::timeout_add_seconds_local_once(15, move || {
                if let Some(ui) = weak.upgrade()
                    && !ui.tray_shown()
                {
                    ui.show();
                }
            });
        }
        // Quit cleanly on logout / Ctrl+C, so a TCP tunnel is not left behind dead.
        for signal in [
            15, /* SIGTERM */
            2,  /* SIGINT */
            1,  /* SIGHUP */
        ] {
            let weak = Rc::downgrade(&built);
            glib_unix::unix_signal_add_local(signal, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.quit();
                }
                glib::ControlFlow::Continue
            });
        }
        *ui.borrow_mut() = Some(built);
    });
    // GTK must not see our own command line flags.
    app.run_with_args(&std::env::args().take(1).collect::<Vec<_>>())
}

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../data/icons/", $name, ".svg")))),*]
    };
}

const ICONS: &[(&str, &str)] = icons![
    "io.github.avanatiker.campusvpn",
    "campusvpn-shield-on-symbolic",
    "campusvpn-shield-busy-symbolic",
    "campusvpn-shield-off-symbolic",
    "campusvpn-institute-symbolic",
    "campusvpn-globe-symbolic",
    "campusvpn-server-symbolic",
];

/// Makes the bundled icons available regardless of the system icon theme.
fn install_icons(display: &gdk::Display) {
    let dir = settings::cache_home().join("campusvpn").join("icons");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("creating icon cache: {e}");
        return;
    }
    for (name, svg) in ICONS {
        let _ = std::fs::write(dir.join(format!("{name}.svg")), svg);
    }
    gtk::IconTheme::for_display(display).add_search_path(&dir);
}

fn build(app: &adw::Application) -> Rc<Ui> {
    let display = gdk::Display::default().expect("no display");
    install_icons(&display);
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    gtk::Window::set_default_icon_name(settings::APP_ID);
    // Keep running while the window is hidden in the tray.
    std::mem::forget(app.hold());

    let settings = Arc::new(Mutex::new(Settings::load()));
    let (ev_tx, ev_rx) = async_channel::unbounded::<Ev>();
    let (ui_tx, ui_rx) = async_channel::unbounded::<UiMsg>();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();

    // ---- Home page ----
    let badge_icon = gtk::Image::builder()
        .icon_name("campusvpn-shield-off-symbolic")
        .pixel_size(52)
        .build();
    let badge = gtk::Box::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["status-badge"])
        .build();
    badge_icon.set_hexpand(true);
    badge_icon.set_vexpand(true);
    badge.append(&badge_icon);
    let title = gtk::Label::builder()
        .label("Not connected")
        .css_classes(["title-1"])
        .build();
    let subtitle = gtk::Label::builder()
        .css_classes(["dim-label"])
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    let details = gtk::Label::builder()
        .css_classes(["caption", "dim-label"])
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    let main_button = gtk::Button::builder()
        .label("Connect")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action", "main-button"])
        .build();

    let hero = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .build();
    hero.append(&badge);
    let spacer = gtk::Box::builder().height_request(8).build();
    hero.append(&spacer);
    hero.append(&title);
    hero.append(&subtitle);
    hero.append(&details);

    let conn_group = adw::PreferencesGroup::new();
    let servers_group = adw::PreferencesGroup::builder().title("Servers").build();
    let add_small = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Add server")
        .build();
    servers_group.set_header_suffix(Some(&add_small));

    let home_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(16)
        .margin_end(16)
        .build();
    home_box.append(&hero);
    home_box.append(&main_button);
    home_box.append(&conn_group);
    home_box.append(&servers_group);
    let home_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(
            &adw::Clamp::builder()
                .maximum_size(480)
                .child(&home_box)
                .build(),
        )
        .vexpand(true)
        .build();

    let add_first = gtk::Button::builder()
        .label("Add server")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action", "main-button"])
        .build();
    let empty = adw::StatusPage::builder()
        .icon_name("io.github.avanatiker.campusvpn")
        .title("Welcome to CampusVPN")
        .description(
            "Find your institution or enter the address of your VPN server to get started.",
        )
        .child(&add_first)
        .build();
    let loading = adw::StatusPage::builder()
        .title("Starting…")
        .child(&gtk::Spinner::builder().spinning(true).build())
        .build();

    let home_stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    home_stack.add_named(&loading, Some("loading"));
    home_stack.add_named(&home_scroll, Some("home"));
    home_stack.add_named(&empty, Some("empty"));

    let menu = gio::Menu::new();
    menu.append(Some("Renew Session"), Some("app.renew"));
    menu.append(Some("Settings"), Some("app.settings"));
    menu.append(Some("About CampusVPN"), Some("app.about"));
    menu.append(Some("Quit"), Some("app.quit"));
    let header = adw::HeaderBar::new();
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .primary(true)
            .build(),
    );
    let home_view = adw::ToolbarView::new();
    home_view.add_top_bar(&header);
    home_view.set_content(Some(&home_stack));
    let home_page = adw::NavigationPage::builder()
        .title("CampusVPN")
        .tag("home")
        .child(&home_view)
        .build();

    let nav = adw::NavigationView::new();
    nav.add(&home_page);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&nav));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("CampusVPN")
        .default_width(440)
        .default_height(700)
        .width_request(360)
        .height_request(400)
        .content(&toasts)
        .build();

    // ---- Tray ----
    let tray = {
        use ksni::blocking::TrayMethods;
        // At login the panel may start after us; register as soon as it appears.
        match VpnTray::new(cmd_tx.clone(), ui_tx.clone())
            .assume_sni_available(true)
            .spawn()
        {
            Ok(h) => Some(h),
            Err(e) => {
                log::warn!("system tray not available: {e}");
                None
            }
        }
    };

    let ui = Rc::new(Ui {
        app: app.clone(),
        window,
        toasts,
        nav,
        home_stack,
        badge,
        badge_icon,
        title,
        subtitle,
        details,
        main_button,
        servers_group,
        server_rows: RefCell::default(),
        conn_group,
        conn_rows: RefCell::default(),
        status: RefCell::default(),
        servers: RefCell::default(),
        settings: settings.clone(),
        cmd: cmd_tx.clone(),
        tray: RefCell::new(tray),
        oauth_dialog: RefCell::default(),
        ask_dialog: RefCell::default(),
        quitting: Cell::new(false),
    });

    // ---- Signals ----
    let weak = Rc::downgrade(&ui);
    ui.main_button.connect_clicked(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        let phase = ui.status.borrow().phase;
        match phase {
            Phase::Connecting => {
                backend::cancel();
                ui.send(Cmd::Disconnect);
            }
            Phase::Connected => ui.send(Cmd::Disconnect),
            Phase::Disconnected => match ui.status.borrow().server.clone() {
                Some(s) => ui.send(Cmd::Connect {
                    kind: s.kind,
                    id: s.id,
                }),
                None => ui.send(Cmd::Toggle),
            },
            Phase::Disconnecting => {}
        }
    });
    for b in [&add_small, &add_first] {
        let weak = Rc::downgrade(&ui);
        b.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.open_add_page();
            }
        });
    }
    let weak = Rc::downgrade(&ui);
    ui.window.connect_close_request(move |w| {
        let Some(ui) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if ui.quitting.get() {
            return glib::Propagation::Proceed;
        }
        if ui.settings.lock().unwrap().close_to_tray && ui.tray_shown() {
            w.set_visible(false);
        } else {
            ui.quit();
        }
        glib::Propagation::Stop
    });

    type Action = fn(&Rc<Ui>);
    let actions: [(&str, Action); 4] = [
        ("settings", |ui| ui.open_settings()),
        ("about", |ui| ui.open_about()),
        ("renew", |ui| ui.send(Cmd::Renew)),
        ("quit", |ui| ui.quit()),
    ];
    for (name, f) in actions {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(&ui);
        action.connect_activate(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                f(&ui);
            }
        });
        app.add_action(&action);
    }
    app.set_accels_for_action("app.quit", &["<Control>q"]);

    let weak = Rc::downgrade(&ui);
    glib::spawn_future_local(async move {
        while let Ok(ev) = ev_rx.recv().await {
            let Some(ui) = weak.upgrade() else { break };
            ui.on_event(ev);
        }
    });
    let weak = Rc::downgrade(&ui);
    glib::spawn_future_local(async move {
        while let Ok(msg) = ui_rx.recv().await {
            let Some(ui) = weak.upgrade() else { break };
            match msg {
                UiMsg::Show => ui.show(),
                UiMsg::Toggle if ui.window.is_visible() && ui.window.is_active() => {
                    ui.window.set_visible(false)
                }
                UiMsg::Toggle => ui.show(),
                UiMsg::Quit => ui.quit(),
            }
        }
    });
    // Keep the remaining session time fresh.
    let weak = Rc::downgrade(&ui);
    glib::timeout_add_seconds_local(30, move || match weak.upgrade() {
        Some(ui) => {
            ui.render_status();
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });

    #[cfg(not(feature = "demo"))]
    backend::spawn(settings, ev_tx, cmd_tx, cmd_rx);
    #[cfg(feature = "demo")]
    {
        let _ = (settings, cmd_tx, cmd_rx);
        crate::demo::run(ev_tx);
    }
    ui
}

impl Ui {
    fn tray_shown(&self) -> bool {
        self.tray.borrow().is_some() && TRAY_SHOWN.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmd.send(cmd);
    }

    fn show(&self) {
        self.window.set_visible(true);
        self.window.present();
    }

    fn quit(&self) {
        if self.quitting.replace(true) {
            return;
        }
        backend::cancel();
        self.send(Cmd::Quit);
        if let Some(tray) = self.tray.borrow_mut().take() {
            tray.shutdown();
        }
        // In case the backend is stuck, do not hang around.
        let app = self.app.clone();
        glib::timeout_add_seconds_local_once(3, move || app.quit());
    }

    fn toast(&self, msg: &str) {
        if self.window.is_visible() {
            self.toasts.add_toast(
                adw::Toast::builder()
                    .title(glib::markup_escape_text(msg))
                    .timeout(5)
                    .build(),
            );
        } else {
            let n = gio::Notification::new("CampusVPN");
            n.set_body(Some(msg));
            self.app.send_notification(Some("status"), &n);
        }
    }

    fn update_tray(&self) {
        if let Some(tray) = self.tray.borrow().as_ref() {
            let status = self.status.borrow().clone();
            let servers = self.servers.borrow().clone();
            tray.update(move |t| {
                t.status = status;
                t.servers = servers;
            });
        }
    }

    fn on_event(self: &Rc<Self>, ev: Ev) {
        match ev {
            Ev::Fatal(msg) => {
                self.show();
                let d = adw::AlertDialog::new(Some("CampusVPN could not start"), Some(&msg));
                d.add_response("quit", "Quit");
                let app = self.app.clone();
                d.connect_response(None, move |_, _| app.quit());
                d.present(Some(&self.window));
            }
            Ev::Status(s) => {
                *self.status.borrow_mut() = *s;
                self.render_status();
                self.render_connection_options();
                self.render_servers();
                self.update_tray();
            }
            Ev::Servers(list) => {
                *self.servers.borrow_mut() = list;
                self.render_servers();
                self.render_status();
                self.render_connection_options();
                self.update_tray();
            }
            Ev::OAuth(url) => self.show_oauth(&url),
            Ev::OAuthDone => self.close_pending_dialogs(),
            Ev::AskProfile { cookie, profiles } => self.ask(
                "Choose a profile",
                cookie,
                profiles
                    .into_iter()
                    .map(|(id, name)| (id, name, None))
                    .collect(),
            ),
            Ev::AskLocation { cookie, locations } => self.ask(
                "Choose a location",
                cookie,
                locations
                    .into_iter()
                    .map(|cc| (cc.clone(), country_name(&cc), Some(flag(&cc))))
                    .collect(),
            ),
            Ev::Error(msg) => self.toast(&msg),
            Ev::Info(msg) => self.toast(&msg),
            Ev::QuitDone => self.app.quit(),
        }
    }

    fn render_status(&self) {
        let st = self.status.borrow();
        let has_servers = !self.servers.borrow().is_empty();
        // The "loading" page stays until the backend reports for the first time.
        self.home_stack
            .set_visible_child_name(if has_servers { "home" } else { "empty" });

        for c in ["connected", "connecting"] {
            self.badge.remove_css_class(c);
        }
        let b = &self.main_button;
        for c in ["suggested-action", "destructive-action"] {
            b.remove_css_class(c);
        }
        b.set_sensitive(true);
        let (title, icon) = match st.phase {
            Phase::Connected => {
                self.badge.add_css_class("connected");
                b.set_label("Disconnect");
                b.add_css_class("destructive-action");
                ("Connected", "campusvpn-shield-on-symbolic")
            }
            Phase::Connecting => {
                self.badge.add_css_class("connecting");
                b.set_label("Cancel");
                ("Connecting…", "campusvpn-shield-busy-symbolic")
            }
            Phase::Disconnecting => {
                self.badge.add_css_class("connecting");
                b.set_label("Disconnecting…");
                b.set_sensitive(false);
                ("Disconnecting…", "campusvpn-shield-busy-symbolic")
            }
            Phase::Disconnected => {
                b.set_label("Connect");
                b.add_css_class("suggested-action");
                b.set_sensitive(has_servers);
                ("Not connected", "campusvpn-shield-off-symbolic")
            }
        };
        self.title.set_label(title);
        self.badge_icon.set_icon_name(Some(icon));

        let sub = match &st.server {
            Some(s) => {
                let mut parts = vec![s.name.clone()];
                if s.kind == ServerType::SecureInternet && !s.country_code.is_empty() {
                    parts.push(format!(
                        "{} {}",
                        flag(&s.country_code),
                        country_name(&s.country_code)
                    ));
                }
                if s.profiles.map.len() > 1
                    && let Some(p) = s.current_profile_name()
                {
                    parts.push(p);
                }
                parts.join(" · ")
            }
            None => String::new(),
        };
        self.subtitle.set_label(&sub);
        self.subtitle.set_visible(!sub.is_empty());

        let mut det = Vec::new();
        if st.phase == Phase::Connected {
            if let Some(p) = st.protocol {
                det.push(p.display().to_owned());
            }
            if let Some(ip) = st.ipv4.as_ref().or(st.ipv6.as_ref()) {
                det.push(ip.clone());
            }
            if let Some(e) = &st.expiry
                && e.end_time > 0
            {
                det.push(remaining(e.end_time));
            }
        }
        self.details.set_label(&det.join("  ·  "));
        self.details.set_visible(!det.is_empty());
    }

    fn render_servers(self: &Rc<Self>) {
        for w in self.server_rows.borrow_mut().drain(..) {
            self.servers_group.remove(&w);
        }
        let st = self.status.borrow();
        let current = st.server.as_ref().map(|s| (s.kind, s.id.clone()));
        let connected = st.phase == Phase::Connected;
        drop(st);
        for s in self.servers.borrow().iter() {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&s.name))
                .subtitle(s.kind.display())
                .activatable(true)
                .build();
            let icon = match s.kind {
                ServerType::SecureInternet => "campusvpn-globe-symbolic",
                ServerType::InstituteAccess => "campusvpn-institute-symbolic",
                _ => "campusvpn-server-symbolic",
            };
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            if current.as_ref() == Some(&(s.kind, s.id.clone())) {
                let mark = gtk::Image::from_icon_name(if connected {
                    "emblem-ok-symbolic"
                } else {
                    "object-select-symbolic"
                });
                if connected {
                    mark.add_css_class("success");
                }
                row.add_suffix(&mark);
            }
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .tooltip_text("Remove server")
                .build();
            let (kind, id, name) = (s.kind, s.id.clone(), s.name.clone());
            let weak = Rc::downgrade(self);
            remove.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.confirm_remove(kind, id.clone(), &name);
                }
            });
            row.add_suffix(&remove);
            let (kind, id) = (s.kind, s.id.clone());
            let weak = Rc::downgrade(self);
            row.connect_activated(move |_| {
                if let Some(ui) = weak.upgrade() {
                    let st = ui.status.borrow().clone();
                    let is_current = st
                        .server
                        .as_ref()
                        .is_some_and(|c| c.kind == kind && c.id == id);
                    if !(is_current && matches!(st.phase, Phase::Connected | Phase::Connecting)) {
                        ui.send(Cmd::Connect {
                            kind,
                            id: id.clone(),
                        });
                    }
                }
            });
            self.servers_group.add(&row);
            self.server_rows.borrow_mut().push(row.upcast());
        }
    }

    /// Profile and location pickers for the current server.
    fn render_connection_options(&self) {
        for w in self.conn_rows.borrow_mut().drain(..) {
            self.conn_group.remove(&w);
        }
        let st = self.status.borrow();
        let busy = matches!(st.phase, Phase::Connecting | Phase::Disconnecting);
        let Some(server) = st.server.clone() else {
            self.conn_group.set_visible(false);
            return;
        };
        drop(st);

        let profiles = server.profiles.sorted();
        if profiles.len() > 1 {
            let names: Vec<&str> = profiles.iter().map(|(_, n)| n.as_str()).collect();
            let row = adw::ComboRow::builder()
                .title("Profile")
                .model(&gtk::StringList::new(&names))
                .sensitive(!busy)
                .build();
            if let Some(i) = profiles
                .iter()
                .position(|(id, _)| *id == server.profiles.current)
            {
                row.set_selected(i as u32);
            }
            let cmd = self.cmd.clone();
            row.connect_selected_notify(move |r| {
                if let Some((id, _)) = profiles.get(r.selected() as usize)
                    && *id != server.profiles.current
                {
                    let _ = cmd.send(Cmd::SetProfile(id.clone()));
                }
            });
            self.conn_group.add(&row);
            self.conn_rows.borrow_mut().push(row.upcast());
        }

        let server = self.status.borrow().server.clone().unwrap();
        if server.kind == ServerType::SecureInternet && server.locations.len() > 1 {
            let mut locs = server.locations.clone();
            locs.sort_by_key(|c| country_name(c));
            let names: Vec<String> = locs
                .iter()
                .map(|c| format!("{} {}", flag(c), country_name(c)))
                .collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let row = adw::ComboRow::builder()
                .title("Location")
                .model(&gtk::StringList::new(&names))
                .sensitive(!busy)
                .build();
            if let Some(i) = locs
                .iter()
                .position(|c| c.eq_ignore_ascii_case(&server.country_code))
            {
                row.set_selected(i as u32);
            }
            let cmd = self.cmd.clone();
            let current = server.country_code.clone();
            row.connect_selected_notify(move |r| {
                if let Some(cc) = locs.get(r.selected() as usize)
                    && !cc.eq_ignore_ascii_case(&current)
                {
                    let _ = cmd.send(Cmd::SetLocation(cc.clone()));
                }
            });
            self.conn_group.add(&row);
            self.conn_rows.borrow_mut().push(row.upcast());
        }
        self.conn_group
            .set_visible(!self.conn_rows.borrow().is_empty());
    }

    fn confirm_remove(self: &Rc<Self>, kind: ServerType, id: String, name: &str) {
        let d = adw::AlertDialog::new(
            Some("Remove server?"),
            Some(&format!(
                "{name} will be removed and you will have to log in again to use it."
            )),
        );
        d.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
        d.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        d.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        d.connect_response(Some("remove"), move |_, _| {
            let Some(ui) = weak.upgrade() else { return };
            // Don't make the user wait for a connection attempt to this server to finish.
            let st = ui.status.borrow().clone();
            if st.phase == Phase::Connecting
                && st
                    .server
                    .as_ref()
                    .is_some_and(|s| s.kind == kind && s.id == id)
            {
                backend::cancel();
            }
            ui.servers
                .borrow_mut()
                .retain(|s| !(s.kind == kind && s.id == id));
            ui.render_servers();
            ui.render_status();
            ui.send(Cmd::RemoveServer {
                kind,
                id: id.clone(),
            });
        });
        d.present(Some(&self.window));
    }

    /// Closes the login / choice dialogs once the operation they belong to has ended.
    fn close_pending_dialogs(&self) {
        // Take first: closing emits signals that borrow these cells again.
        let oauth = self.oauth_dialog.borrow_mut().take();
        if let Some((d, settled)) = oauth {
            settled.set(true);
            d.force_close();
        }
        let ask = self.ask_dialog.borrow_mut().take();
        if let Some((d, settled)) = ask {
            settled.set(true);
            d.force_close();
        }
    }

    fn show_oauth(self: &Rc<Self>, url: &str) {
        self.show();
        gtk::UriLauncher::new(url).launch(Some(&self.window), gio::Cancellable::NONE, |res| {
            if let Err(e) = res {
                log::error!("opening the browser: {e}");
            }
        });
        if self.oauth_dialog.borrow().is_some() {
            return;
        }
        let settled = Rc::new(Cell::new(false));
        let d = adw::AlertDialog::new(
            Some("Log in with your browser"),
            Some(
                "A browser window was opened. Log in there and approve the access request; this dialog closes by itself.",
            ),
        );
        d.add_responses(&[("cancel", "Cancel"), ("copy", "Copy Link")]);
        d.set_close_response("cancel");
        let url = url.to_owned();
        let (weak, flag) = (Rc::downgrade(self), settled.clone());
        d.connect_response(None, move |d, resp| {
            let Some(ui) = weak.upgrade() else { return };
            if flag.get() {
                return;
            }
            if resp == "copy" {
                ui.window.clipboard().set_text(&url);
                // Keep waiting for the login.
                d.present(Some(&ui.window));
            } else {
                flag.set(true);
                ui.oauth_dialog.borrow_mut().take();
                backend::cancel();
            }
        });
        d.present(Some(&self.window));
        *self.oauth_dialog.borrow_mut() = Some((d, settled));
    }

    /// Lets the user pick a profile or location requested by eduvpn-common.
    fn ask(
        self: &Rc<Self>,
        title: &str,
        cookie: usize,
        choices: Vec<(String, String, Option<String>)>,
    ) {
        self.show();
        let old = self.ask_dialog.borrow_mut().take();
        if let Some((old, settled)) = old {
            settled.set(true);
            old.force_close();
        }
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let answered = Rc::new(Cell::new(false));
        let dialog = adw::Dialog::builder()
            .title(title)
            .content_width(380)
            .build();
        for (id, name, prefix) in choices {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&name))
                .activatable(true)
                .build();
            if let Some(p) = prefix {
                row.add_prefix(&gtk::Label::new(Some(&p)));
            }
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let (dialog, answered) = (dialog.clone(), answered.clone());
            row.connect_activated(move |_| {
                answered.set(true);
                if let Err(e) = common::cookie_reply(cookie, &id) {
                    log::error!("reply: {e:#}");
                }
                dialog.close();
            });
            list.append(&row);
        }
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(16)
            .margin_end(16)
            .build();
        content.append(&list);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(520)
                .child(&content)
                .build(),
        ));
        dialog.set_child(Some(&view));
        let (weak, flag) = (Rc::downgrade(self), answered.clone());
        dialog.connect_closed(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.ask_dialog.borrow_mut().take();
            }
            if !flag.get() {
                // Dismissed by the user: give up on connecting.
                common::cookie_cancel(cookie);
                backend::cancel();
            }
        });
        dialog.present(Some(&self.window));
        *self.ask_dialog.borrow_mut() = Some((dialog, answered));
    }

    fn open_add_page(self: &Rc<Self>) {
        if self.nav.find_page("add").is_some() {
            return;
        }
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Institution, organisation or server address")
            .hexpand(true)
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let hint = gtk::Label::builder()
            .label("Search for your institution, or type the address of your own VPN server.")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .margin_top(24)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(16)
            .margin_bottom(24)
            .margin_start(16)
            .margin_end(16)
            .build();
        content.append(&search);
        content.append(&hint);
        content.append(&list);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(
                &adw::Clamp::builder()
                    .maximum_size(560)
                    .child(&content)
                    .build(),
            )
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&scroll));
        let page = adw::NavigationPage::builder()
            .title("Add Server")
            .tag("add")
            .child(&view)
            .build();

        let generation = Rc::new(Cell::new(0u64));
        let weak = Rc::downgrade(self);
        search.connect_search_changed(move |entry| {
            let Some(ui) = weak.upgrade() else { return };
            let query = entry.text().trim().to_owned();
            let gen_now = generation.get() + 1;
            generation.set(gen_now);
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            hint.set_visible(query.is_empty());
            list.set_visible(!query.is_empty());
            if query.is_empty() {
                return;
            }
            if looks_like_address(&query) {
                let address = if query.contains("://") {
                    query.clone()
                } else {
                    format!("https://{query}")
                };
                ui.add_candidate_row(
                    &list,
                    Candidate {
                        kind: ServerType::Custom,
                        name: address.clone(),
                        id: address,
                    },
                    "Own server",
                );
            }
            let has_si = ui
                .servers
                .borrow()
                .iter()
                .any(|s| s.kind == ServerType::SecureInternet);
            let (list, generation, weak) = (list.clone(), generation.clone(), Rc::downgrade(&ui));
            glib::spawn_future_local(async move {
                let found = gio::spawn_blocking(move || backend::search(&query, has_si))
                    .await
                    .unwrap_or_default();
                let Some(ui) = weak.upgrade() else { return };
                if generation.get() != gen_now {
                    return;
                }
                for c in found.into_iter().take(60) {
                    let label = c.kind.display();
                    ui.add_candidate_row(&list, c, label);
                }
            });
        });
        self.nav.push(&page);
        search.grab_focus();
    }

    fn add_candidate_row(self: &Rc<Self>, list: &gtk::ListBox, c: Candidate, subtitle: &str) {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&c.name))
            .subtitle(subtitle)
            .activatable(true)
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
        let weak = Rc::downgrade(self);
        row.connect_activated(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.nav.pop_to_tag("home");
                ui.send(Cmd::AddServer(c.clone()));
            }
        });
        list.append(&row);
    }

    fn open_settings(self: &Rc<Self>) {
        let s = self.settings.lock().unwrap().clone();
        let has_tray = self.tray_shown();
        let page = adw::PreferencesPage::new();

        let startup = adw::PreferencesGroup::builder().title("Startup").build();
        let autostart = adw::SwitchRow::builder()
            .title("Start on login")
            .subtitle("Launch CampusVPN in the background when you log in")
            .active(settings::autostart_enabled())
            .build();
        let window = self.window.clone();
        let toasts = self.toasts.clone();
        autostart.connect_active_notify(move |r| {
            if let Err(e) = settings::set_autostart(r.is_active()) {
                toasts.add_toast(adw::Toast::new(&format!("Could not change autostart: {e}")));
                let _ = &window;
            }
        });
        startup.add(&autostart);
        startup.add(&self.switch(
            "Start minimised to tray",
            "Only show the tray icon when started on login",
            s.start_minimized && has_tray,
            has_tray,
            |s, v| s.start_minimized = v,
        ));
        startup.add(&self.switch(
            "Connect automatically",
            "Connect to the last used server on start",
            s.autoconnect,
            true,
            |s, v| s.autoconnect = v,
        ));

        let window_group = adw::PreferencesGroup::builder().title("Window").build();
        let tray_sub = if has_tray {
            "Closing the window keeps CampusVPN running in the system tray"
        } else {
            "No system tray found on this desktop"
        };
        window_group.add(&self.switch(
            "Keep running in tray",
            tray_sub,
            s.close_to_tray && has_tray,
            has_tray,
            |s, v| s.close_to_tray = v,
        ));

        let conn = adw::PreferencesGroup::builder()
            .title("Connection")
            .description("Changes apply on the next connect")
            .build();
        conn.add(&self.switch(
            "Allow local network access",
            "Reach printers and other devices on your LAN while connected (WireGuard)",
            s.allow_lan,
            true,
            |s, v| s.allow_lan = v,
        ));
        conn.add(&self.switch(
            "Always use TCP",
            "Networks where UDP is blocked are detected and remembered automatically",
            s.prefer_tcp,
            true,
            |s, v| s.prefer_tcp = v,
        ));

        page.add(&startup);
        page.add(&window_group);
        page.add(&conn);
        let dialog = adw::PreferencesDialog::new();
        dialog.add(&page);
        dialog.present(Some(&self.window));
    }

    fn switch(
        &self,
        title: &str,
        subtitle: &str,
        active: bool,
        sensitive: bool,
        set: fn(&mut Settings, bool),
    ) -> adw::SwitchRow {
        let row = adw::SwitchRow::builder()
            .title(title)
            .subtitle(subtitle)
            .active(active)
            .sensitive(sensitive)
            .build();
        let settings = self.settings.clone();
        row.connect_active_notify(move |r| {
            let mut s = settings.lock().unwrap();
            set(&mut s, r.is_active());
            s.save();
        });
        row
    }

    fn open_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("CampusVPN")
            .application_icon(settings::APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .comments("A lightweight Linux VPN client for eduVPN and Let's Connect! servers, written in Rust.\nNot affiliated with eduVPN, GÉANT or SURF. Uses the eduvpn-common library.")
            .website("https://github.com/Avanatiker/campusvpn")
            .license_type(gtk::License::Gpl30)
            .build();
        about.present(Some(&self.window));
    }
}

fn looks_like_address(q: &str) -> bool {
    !q.contains(' ') && (q.contains('.') || q.contains("://"))
}

fn remaining(end: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = end - now;
    if secs <= 0 {
        return "session expired".into();
    }
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("valid for {d} d {h} h")
    } else if h > 0 {
        format!("valid for {h} h {m} min")
    } else {
        format!("valid for {m} min")
    }
}

/// Flag emoji from an ISO country code.
fn flag(cc: &str) -> String {
    cc.to_ascii_uppercase()
        .chars()
        .filter(char::is_ascii_uppercase)
        .filter_map(|c| char::from_u32(0x1F1E6 + (c as u32 - 'A' as u32)))
        .collect()
}

fn country_name(cc: &str) -> String {
    let name = match cc.to_ascii_uppercase().as_str() {
        "AL" => "Albania",
        "AM" => "Armenia",
        "AT" => "Austria",
        "AU" => "Australia",
        "BE" => "Belgium",
        "BG" => "Bulgaria",
        "BR" => "Brazil",
        "CA" => "Canada",
        "CH" => "Switzerland",
        "CL" => "Chile",
        "CY" => "Cyprus",
        "CZ" => "Czechia",
        "DE" => "Germany",
        "DK" => "Denmark",
        "EE" => "Estonia",
        "ES" => "Spain",
        "FI" => "Finland",
        "FR" => "France",
        "GB" | "UK" => "United Kingdom",
        "GE" => "Georgia",
        "GR" => "Greece",
        "HR" => "Croatia",
        "HU" => "Hungary",
        "IE" => "Ireland",
        "IL" => "Israel",
        "IN" => "India",
        "IS" => "Iceland",
        "IT" => "Italy",
        "JP" => "Japan",
        "KE" => "Kenya",
        "KR" => "South Korea",
        "LT" => "Lithuania",
        "LU" => "Luxembourg",
        "LV" => "Latvia",
        "MA" => "Morocco",
        "MD" => "Moldova",
        "ME" => "Montenegro",
        "MK" => "North Macedonia",
        "MT" => "Malta",
        "MX" => "Mexico",
        "NG" => "Nigeria",
        "NL" => "Netherlands",
        "NO" => "Norway",
        "NZ" => "New Zealand",
        "PL" => "Poland",
        "PT" => "Portugal",
        "RO" => "Romania",
        "RS" => "Serbia",
        "SE" => "Sweden",
        "SG" => "Singapore",
        "SI" => "Slovenia",
        "SK" => "Slovakia",
        "TR" => "Türkiye",
        "UA" => "Ukraine",
        "UG" => "Uganda",
        "US" => "United States",
        "ZA" => "South Africa",
        "TZ" => "Tanzania",
        "AR" => "Argentina",
        "CO" => "Colombia",
        "PE" => "Peru",
        "EC" => "Ecuador",
        "UY" => "Uruguay",
        "TH" => "Thailand",
        "MY" => "Malaysia",
        "ID" => "Indonesia",
        _ => return cc.to_ascii_uppercase(),
    };
    name.to_owned()
}
