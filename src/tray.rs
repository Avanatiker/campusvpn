//! System tray icon (StatusNotifierItem), works natively on KDE and with the AppIndicator
//! extension on GNOME.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{Icon, MenuItem, ToolTip};

use crate::backend::{self, Cmd, Phase, Status};
use crate::types::Server;
use crate::ui::UiMsg;

/// Whether a StatusNotifierWatcher (the panel's tray) currently shows our icon.
pub static TRAY_SHOWN: AtomicBool = AtomicBool::new(true);

pub struct VpnTray {
    pub status: Status,
    pub servers: Vec<Server>,
    pub cmd: Sender<Cmd>,
    pub ui: async_channel::Sender<UiMsg>,
    icons: [Vec<Icon>; 3],
}

impl VpnTray {
    pub fn new(cmd: Sender<Cmd>, ui: async_channel::Sender<UiMsg>) -> Self {
        let icons = [IconKind::Off, IconKind::Busy, IconKind::On]
            .map(|k| [22, 32, 48, 64].map(|s| render(k, s)).to_vec());
        Self {
            status: Status::default(),
            servers: Vec::new(),
            cmd,
            ui,
            icons,
        }
    }

    fn status_text(&self) -> String {
        let name = self.status.server.as_ref().map(|s| s.name.as_str());
        match (self.status.phase, name) {
            (Phase::Connected, Some(n)) => format!("Connected to {n}"),
            (Phase::Connected, None) => "Connected".into(),
            (Phase::Connecting, _) => "Connecting…".into(),
            (Phase::Disconnecting, _) => "Disconnecting…".into(),
            (Phase::Disconnected, _) => "Not connected".into(),
        }
    }
}

impl ksni::Tray for VpnTray {
    fn watcher_online(&self) {
        TRAY_SHOWN.store(true, Ordering::SeqCst);
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        log::info!("system tray not available: {reason:?}");
        TRAY_SHOWN.store(false, Ordering::SeqCst);
        // Keep the service: the panel may (re)start later, e.g. during login.
        true
    }

    fn id(&self) -> String {
        "eduvpn-rs".into()
    }

    fn title(&self) -> String {
        "eduVPN".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Communications
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let idx = match self.status.phase {
            Phase::Connected => 2,
            Phase::Connecting | Phase::Disconnecting => 1,
            Phase::Disconnected => 0,
        };
        self.icons[idx].clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "eduVPN".into(),
            description: self.status_text(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.ui.try_send(UiMsg::Toggle);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: self.status_text(),
                enabled: false,
                ..Default::default()
            }
            .into(),
        ];
        let (label, icon) = match self.status.phase {
            Phase::Connected => ("Disconnect", "network-disconnect"),
            Phase::Connecting => ("Cancel", "process-stop"),
            Phase::Disconnecting => ("Disconnecting…", "network-disconnect"),
            Phase::Disconnected => ("Connect", "network-connect"),
        };
        items.push(
            StandardItem {
                label: label.into(),
                icon_name: icon.into(),
                enabled: self.status.phase != Phase::Disconnecting && !self.servers.is_empty(),
                activate: Box::new(|t: &mut Self| {
                    if t.status.phase == Phase::Connecting {
                        backend::cancel();
                        let _ = t.cmd.send(Cmd::Disconnect);
                    } else {
                        let _ = t.cmd.send(Cmd::Toggle);
                    }
                }),
                ..Default::default()
            }
            .into(),
        );
        if self.servers.len() > 1 {
            let current = self.status.server.as_ref().map(|s| (s.kind, s.id.clone()));
            let submenu = self
                .servers
                .iter()
                .map(|s| {
                    let (kind, id) = (s.kind, s.id.clone());
                    CheckmarkItem {
                        label: s.name.replace('_', "__"),
                        checked: current.as_ref() == Some(&(s.kind, s.id.clone())),
                        activate: Box::new(move |t: &mut Self| {
                            let _ = t.cmd.send(Cmd::Connect {
                                kind,
                                id: id.clone(),
                            });
                        }),
                        ..Default::default()
                    }
                    .into()
                })
                .collect();
            items.push(
                SubMenu {
                    label: "Servers".into(),
                    icon_name: "network-server".into(),
                    submenu,
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Open eduVPN".into(),
                icon_name: "window-new".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.ui.try_send(UiMsg::Show);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.ui.try_send(UiMsg::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

#[derive(Clone, Copy, PartialEq)]
enum IconKind {
    Off,
    Busy,
    On,
}

/// Draws the tray icon: a shield, filled green with a check when connected,
/// amber with dots while connecting and as a light outline when disconnected.
fn render(kind: IconKind, size: i32) -> Icon {
    const SS: i32 = 4;
    let n = size as f32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let color: [f32; 3] = match kind {
        IconKind::On => [0x2e as f32, 0xc2 as f32, 0x7e as f32],
        IconKind::Busy => [0xf5 as f32, 0xa6 as f32, 0x23 as f32],
        IconKind::Off => [0xd8 as f32, 0xd8 as f32, 0xd8 as f32],
    };
    for py in 0..size {
        for px in 0..size {
            let (mut fill, mut white) = (0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / n;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / n;
                    let d = shield_depth(x, y);
                    let in_shape = match kind {
                        IconKind::Off => (0.0..0.075).contains(&d),
                        _ => d >= 0.0,
                    };
                    if !in_shape {
                        continue;
                    }
                    let mark = match kind {
                        IconKind::On => {
                            seg_dist(x, y, (0.33, 0.50), (0.45, 0.62)).min(seg_dist(
                                x,
                                y,
                                (0.45, 0.62),
                                (0.68, 0.36),
                            )) < 0.05
                        }
                        IconKind::Busy => [0.33, 0.5, 0.67]
                            .iter()
                            .any(|cx| (x - cx).powi(2) + (y - 0.49).powi(2) < 0.055f32.powi(2)),
                        IconKind::Off => false,
                    };
                    if mark { white += 1.0 } else { fill += 1.0 }
                }
            }
            let total = (SS * SS) as f32;
            let a = (fill + white) / total;
            let px_color = |c: f32| {
                if a > 0.0 {
                    ((c * fill + 255.0 * white) / (fill + white)) as u8
                } else {
                    0
                }
            };
            data.extend_from_slice(&[
                (a * 255.0) as u8,
                px_color(color[0]),
                px_color(color[1]),
                px_color(color[2]),
            ]);
        }
    }
    Icon {
        width: size,
        height: size,
        data,
    }
}

/// Distance inside the shield outline (negative = outside).
fn shield_depth(x: f32, y: f32) -> f32 {
    let (top, mid, bottom, half) = (0.08, 0.48, 0.94, 0.38);
    if y < top || y > bottom {
        return -1.0;
    }
    let t = ((y - mid) / (bottom - mid)).max(0.0);
    // Rounded sides that narrow into a point at the bottom.
    let hw = if y <= mid {
        half
    } else {
        half * (1.0 - t * t).max(0.0).sqrt() * (1.0 - 0.45 * t)
    };
    let side = hw - (x - 0.5).abs();
    if side < 0.0 {
        return -1.0;
    }
    // Vertical distance to the curved bottom, approximated.
    let below = if y > mid { side * 0.9 } else { side };
    below.min(y - top)
}

fn seg_dist(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((x - a.0 - t * dx).powi(2) + (y - a.1 - t * dy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes the tray icons as PNGs to $ICON_DUMP_DIR for visual inspection.
    #[test]
    #[ignore]
    fn dump_icons() {
        use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
        let dir = std::env::var("ICON_DUMP_DIR").unwrap();
        for (name, kind) in [
            ("off", IconKind::Off),
            ("busy", IconKind::Busy),
            ("on", IconKind::On),
        ] {
            let icon = render(kind, 64);
            // ARGB -> RGBA
            let rgba: Vec<u8> = icon
                .data
                .chunks(4)
                .flat_map(|p| [p[1], p[2], p[3], p[0]])
                .collect();
            let pb = Pixbuf::from_mut_slice(rgba, Colorspace::Rgb, true, 8, 64, 64, 64 * 4);
            pb.savev(format!("{dir}/tray-{name}.png"), "png", &[])
                .unwrap();
        }
    }
}
