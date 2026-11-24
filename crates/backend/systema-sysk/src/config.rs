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
    pub receive_buffer: String,
    pub socket_mode: String,
    pub service: String,
    pub directory_mode: String,
    /// `PassCredentials=`: enable `SO_PASSCRED` on every listener, so the
    /// kernel attaches `SCM_CREDENTIALS` to received messages.
    pub pass_credentials: bool,
    /// `PassSecurity=`: enable `SO_PASSSEC` on every listener.
    pub pass_security: bool,
    /// `Symlinks=`: compatibility symlinks to create for the socket.
    pub symlinks: Vec<String>,
    /// `XAttrEntryPoint=`: `NAME=VALUE` attributes set on the filesystem
    /// socket inode.
    pub xattr_entrypoint: Vec<String>,
    /// `XAttrListen=`: `NAME=VALUE` attributes set on the listening socket.
    pub xattr_listen: Vec<String>,
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
            receive_buffer: get_string(map, "ReceiveBuffer").unwrap_or_default(),
            socket_mode: get_string(map, "SocketMode").unwrap_or_default(),
            service: get_string(map, "Service").unwrap_or_default(),
            directory_mode: get_string(map, "DirectoryMode").unwrap_or_default(),
            pass_credentials: get_bool(map, "PassCredentials"),
            pass_security: get_bool(map, "PassSecurity"),
            symlinks: get_strings(map, "Symlinks"),
            xattr_entrypoint: get_strings(map, "XAttrEntryPoint"),
            xattr_listen: get_strings(map, "XAttrListen"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pass_credentials_and_pass_security() {
        let cfg = UnitConfig {
            unit_name: "systemd-udevd-kernel.socket".to_string(),
            config_json: br#"{"ListenNetlink":["kobject-uevent 1"],"PassCredentials":true,"PassSecurity":true}"#
                .to_vec(),
            ..Default::default()
        };
        let config = SocketConfig::from_unit_config(&cfg).unwrap();
        assert!(config.pass_credentials);
        assert!(config.pass_security);
        assert_eq!(config.listen[0].netlink, "kobject-uevent 1");
    }

    #[test]
    fn parses_receive_buffer() {
        let cfg = UnitConfig {
            unit_name: "systemd-udevd-kernel.socket".to_string(),
            config_json: br#"{"ListenNetlink":["kobject-uevent 1"],"ReceiveBuffer":"128M"}"#
                .to_vec(),
            ..Default::default()
        };
        let config = SocketConfig::from_unit_config(&cfg).unwrap();
        assert_eq!(config.receive_buffer, "128M");
    }
}
