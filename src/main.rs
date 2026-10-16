mod backend;
mod common;
mod keyring;
mod nm;
mod settings;
mod tray;
mod types;
mod ui;

const USAGE: &str = "\
Usage: eduvpn-rs [OPTIONS]

Options:
  --hidden      Start in the system tray without showing the window
  --autostart   Used by the login autostart entry (honours the \"start minimised\" setting)
  -h, --help    Show this help
  -V, --version Show the version

Environment:
  EDUVPN_COMMON_LIB  Path to libeduvpn_common-5.x.so
  RUST_LOG           Log level, e.g. RUST_LOG=debug";

fn main() -> gtk::glib::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut hidden = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--hidden" | "--minimized" | "--minimised" => hidden = true,
            "--autostart" => hidden = settings::Settings::load().start_minimized,
            "-h" | "--help" => {
                println!("{USAGE}");
                return gtk::glib::ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("eduvpn-rs {}", env!("CARGO_PKG_VERSION"));
                return gtk::glib::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown option: {other}\n\n{USAGE}");
                return gtk::glib::ExitCode::FAILURE;
            }
        }
    }
    ui::run(hidden)
}
