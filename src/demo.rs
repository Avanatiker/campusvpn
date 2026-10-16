//! Fake data for screenshots (`cargo run --features demo`). Nothing is connected.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::backend::{Ev, EvSender, Phase, Status};
use crate::types::{Expiry, Profile, Profiles, Protocol, Server, ServerType};

fn server(
    kind: ServerType,
    id: &str,
    name: &str,
    profiles: &[(&str, &str)],
    current: &str,
) -> Server {
    let map: HashMap<String, Profile> = profiles
        .iter()
        .map(|(id, name)| {
            let display_name = HashMap::from([("en".to_owned(), (*name).to_owned())]);
            (
                (*id).to_owned(),
                Profile {
                    display_name,
                    priority: 0,
                },
            )
        })
        .collect();
    Server {
        kind,
        id: id.to_owned(),
        name: name.to_owned(),
        profiles: Profiles {
            map,
            current: current.to_owned(),
        },
        country_code: String::new(),
        locations: Vec::new(),
    }
}

pub fn run(ev: EvSender) {
    let uni = server(
        ServerType::InstituteAccess,
        "https://vpn.example.edu/",
        "Example University",
        &[("full", "Full tunnel"), ("split", "Campus only")],
        "full",
    );
    let own = server(
        ServerType::Custom,
        "https://vpn.example.org/",
        "vpn.example.org",
        &[],
        "",
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let status = Status {
        phase: Phase::Connected,
        server: Some(uni.clone()),
        protocol: Some(Protocol::WireGuard),
        expiry: Some(Expiry {
            end_time: now + 6 * 86400 + 5 * 3600,
        }),
        // Documentation address range (RFC 5737), not a real IP.
        ipv4: Some("192.0.2.42".into()),
        ipv6: None,
    };
    let _ = ev.try_send(Ev::Servers(vec![uni, own]));
    let _ = ev.try_send(Ev::Status(Box::new(status)));
}
