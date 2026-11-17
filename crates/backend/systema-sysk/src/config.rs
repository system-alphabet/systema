//! `[Socket]` directive parsing for the socket worker.

use sysa::proto::UnitConfig;

/// One listen address, normalised from the section's `Listen*=` directives.
#[derive(Debug, Clone, Default)]
pub struct SocketAddress {
    pub stream: String,
    pub datagram: String,
    pub sequential_packet: String,
    pub fifo: String,
    pub netlink: String,
}

/// The directives of a `[Socket]` section.
#[derive(Debug, Clone, Default)]
pub struct SocketConfig {
    pub listen: Vec<SocketAddress>,
    pub accept: bool,
    pub backlog: u32,
    pub socket_mode: String,
    pub service: String,
    pub directory_mode: String,
}

impl SocketConfig {
    pub fn from_unit_config(config: &UnitConfig) -> Option<Self> {
        if config.config_json.is_empty() {
            return None;
        }
        let map = sysa::ir::config::parse_json(&config.config_json).ok()?;
        Some(Self::from_map(&map))
    }

    fn from_map(map: &sysa::ir::ConfigMap) -> Self {
        use sysa::ir::config::{get_bool, get_string, get_strings, get_u32_or};

        let mut listen = Vec::new();
        for address in get_strings(map, "ListenStream") {
            listen.push(SocketAddress {
                stream: address,
                ..Default::default()
            });
        }
        for address in get_strings(map, "ListenDatagram") {
            listen.push(SocketAddress {
                datagram: address,
                ..Default::default()
            });
        }
        for address in get_strings(map, "ListenSequentialPacket") {
            listen.push(SocketAddress {
                sequential_packet: address,
                ..Default::default()
            });
        }
        for address in get_strings(map, "ListenFIFO") {
            listen.push(SocketAddress {
                fifo: address,
                ..Default::default()
            });
        }
        for address in get_strings(map, "ListenNetlink") {
            listen.push(SocketAddress {
                netlink: address,
                ..Default::default()
            });
        }

        SocketConfig {
            listen,
            accept: get_bool(map, "Accept"),
            backlog: get_u32_or(map, "Backlog", 0),
            socket_mode: get_string(map, "SocketMode").unwrap_or_default(),
            service: get_string(map, "Service").unwrap_or_default(),
            directory_mode: get_string(map, "DirectoryMode").unwrap_or_default(),
        }
    }
}
