//! Custom `org.freedesktop.DBus.Properties` interfaces (bridge).
//!
//! zbus's default `Properties` implementation rejects an empty interface
//! name in `GetAll`, but systemd accepts `GetAll("")` to return every
//! property across all interfaces on an object.  We replace the default
//! implementation on the manager path and every per-unit object path with
//! wrappers that parse the argument as a plain `String` and handle the
//! empty-string case like systemd.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use async_trait::async_trait;
use zbus::message::Header;
use zbus::names::InterfaceName;
use zbus::object_server::{DispatchResult, Interface, SignalEmitter};
use zbus::OwnedVariant as OwnedValue;
use zbus::{fdo, BoxDBusError, Connection, ObjectServer};

use super::manager::ManagerInterface;
use super::mount_obj::MountObject;
use super::scope_obj::ScopeObject;
use super::service_obj::ServiceObject;
use super::slice_obj::SliceObject;
use super::socket_obj::SocketObject;
use super::unit_obj::UnitObject;
use super::BridgeContext;

/// Object path each custom `Properties` interface answers on.
trait PropertiesPath {
    fn object_path(&self) -> String;
}

impl PropertiesPath for Properties {
    fn object_path(&self) -> String {
        format!(
            "/org/freedesktop/systemd1/unit/{}",
            super::manager::encode_unit_path(&self.unit_name)
        )
    }
}

impl PropertiesPath for ManagerProperties {
    fn object_path(&self) -> String {
        "/org/freedesktop/systemd1".to_string()
    }
}

/// Render a zbus error as the boxed D-Bus error the `Interface` trait expects.
fn box_dbus_error(err: zbus::Error) -> BoxDBusError {
    Box::new(fdo::Error::Failed(err.to_string()))
}

/// Fallback `org.freedesktop.DBus.Properties` for unit paths without an
/// object yet.
///
/// `systemctl status <unit>` builds the object path client-side and calls
/// `Properties.GetAll` without a prior `GetUnit`, so a unit whose object has
/// not been registered (a template instance such as `modprobe@ntfs.service`,
/// or a unit that appeared after startup) would otherwise get
/// `UnknownObject`.  One instance serves every path below
/// `/org/freedesktop/systemd1/unit`; the unit name comes from the message
/// path.
pub struct FallbackProperties {
    pub ctx: Arc<BridgeContext>,
}

impl FallbackProperties {
    /// Resolve `unit_name`, materialize it through System A when needed, and
    /// register its exact object for subsequent calls.
    async fn load(&self, unit_name: &str) -> Result<String, BoxDBusError> {
        let canonical = super::resolve_canonical(&self.ctx, unit_name).await;
        let loaded = self
            .ctx
            .mirror
            .read()
            .get(&canonical)
            .map(|snapshot| snapshot.load_state == "loaded")
            .unwrap_or(false);
        if loaded {
            super::ensure_unit_object(&self.ctx, &canonical).await;
            return Ok(canonical);
        }
        match systema_sysw_common::calls::load_unit_snapshot(&self.ctx.client, &canonical).await {
            Ok((load, snapshot)) => {
                self.ctx.mirror.write().upsert(snapshot);
                super::ensure_unit_object(&self.ctx, &load.name).await;
                Ok(load.name)
            }
            Err(_) => Err(Box::new(fdo::Error::UnknownObject(sysa::l10n::fmt(
                sysa::l10n::t_("Unit {name} is not loaded."),
                &[("name", &canonical.to_string())],
            )))),
        }
    }
}

/// Split a unit object path into `(unit_name, path)`.
fn unit_from_message(msg: &zbus::message::Message) -> Option<(String, String)> {
    let header = msg.header();
    let path = header.path()?.to_string();
    let encoded = path.strip_prefix("/org/freedesktop/systemd1/unit/")?;
    let name = super::manager::decode_unit_path(encoded)?;
    if name.is_empty() {
        return None;
    }
    Some((name, path))
}

/// Replacement for `zbus::fdo::Properties` on per-unit object paths.
pub struct Properties {
    pub ctx: Arc<BridgeContext>,
    pub unit_name: String,
}

impl Properties {
    fn kind(&self) -> String {
        self.ctx
            .mirror
            .read()
            .get(&self.unit_name)
            .map(|s| s.kind.clone())
            .unwrap_or_default()
    }

    async fn props_for_interface(
        &self,
        iface_name: &str,
        server: &ObjectServer,
        connection: &Connection,
        emitter: &SignalEmitter<'_>,
    ) -> Option<fdo::Result<HashMap<String, OwnedValue>>> {
        let kind = self.kind();
        match iface_name {
            "org.freedesktop.systemd1.Unit" => {
                let obj = UnitObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(Interface::get_all(&obj, server, connection, None, emitter).await)
            }
            "org.freedesktop.systemd1.Service" if kind == "service" => {
                let obj = ServiceObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(Interface::get_all(&obj, server, connection, None, emitter).await)
            }
            "org.freedesktop.systemd1.Socket" if kind == "socket" => {
                let obj = SocketObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(Interface::get_all(&obj, server, connection, None, emitter).await)
            }
            "org.freedesktop.systemd1.Mount" if kind == "mount" => {
                let obj = MountObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(Interface::get_all(&obj, server, connection, None, emitter).await)
            }
            "org.freedesktop.systemd1.Slice" if kind == "slice" => {
                let obj = SliceObject {
                    unit_name: self.unit_name.clone(),
                };
                Some(Interface::get_all(&obj, server, connection, None, emitter).await)
            }
            "org.freedesktop.systemd1.Scope" if kind == "scope" => {
                let obj = ScopeObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(Interface::get_all(&obj, server, connection, None, emitter).await)
            }
            "org.freedesktop.DBus.Properties"
            | "org.freedesktop.DBus.Peer"
            | "org.freedesktop.DBus.Introspectable"
            | "org.freedesktop.DBus.ObjectManager" => Some(Ok(HashMap::new())),
            _ => None,
        }
    }

    async fn all_props(
        &self,
        server: &ObjectServer,
        connection: &Connection,
        emitter: &SignalEmitter<'_>,
    ) -> HashMap<String, OwnedValue> {
        let mut result = HashMap::new();
        let unit_obj = UnitObject {
            ctx: self.ctx.clone(),
            unit_name: self.unit_name.clone(),
        };
        if let Ok(props) = Interface::get_all(&unit_obj, server, connection, None, emitter).await {
            result.extend(props);
        }
        match self.kind().as_str() {
            "service" => {
                let obj = ServiceObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                if let Ok(props) = Interface::get_all(&obj, server, connection, None, emitter).await
                {
                    result.extend(props);
                }
            }
            "socket" => {
                let obj = SocketObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                if let Ok(props) = Interface::get_all(&obj, server, connection, None, emitter).await
                {
                    result.extend(props);
                }
            }
            "mount" => {
                let obj = MountObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                if let Ok(props) = Interface::get_all(&obj, server, connection, None, emitter).await
                {
                    result.extend(props);
                }
            }
            _ => {}
        }
        result
    }

    async fn handle_get_all(
        &self,
        server: &ObjectServer,
        connection: &Connection,
        msg: &zbus::message::Message,
        emitter: &SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let header = msg.header();
        let body = msg.body();
        let (iface_name,): (String,) = match body.deserialize() {
            Ok(r) => r,
            Err(e) => {
                let err = fdo::Error::InvalidArgs(sysa::l10n::fmt(
                    sysa::l10n::t_("Bad arguments: {e}."),
                    &[("e", &e.to_string())],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
                return Ok(());
            }
        };
        if iface_name.is_empty() {
            let props = self.all_props(server, connection, emitter).await;
            connection.reply(&header, &props).await?;
        } else {
            match self
                .props_for_interface(&iface_name, server, connection, emitter)
                .await
            {
                Some(Ok(props)) => {
                    connection.reply(&header, &props).await?;
                }
                Some(Err(e)) => {
                    connection.reply_dbus_error(&msg.header(), e).await?;
                }
                None => {
                    let err = fdo::Error::UnknownInterface(sysa::l10n::fmt(
                        sysa::l10n::t_("Unknown interface '{iface_name}'."),
                        &[("iface_name", &iface_name.to_string())],
                    ));
                    connection.reply_dbus_error(&msg.header(), err).await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_get(
        &self,
        server: &ObjectServer,
        connection: &Connection,
        msg: &zbus::message::Message,
        emitter: &SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let header = msg.header();
        let body = msg.body();
        let (iface_name, prop_name): (String, String) = match body.deserialize() {
            Ok(r) => r,
            Err(e) => {
                let err = fdo::Error::InvalidArgs(sysa::l10n::fmt(
                    sysa::l10n::t_("Bad arguments: {e}."),
                    &[("e", &e.to_string())],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
                return Ok(());
            }
        };
        let kind = self.kind();
        let value = match iface_name.as_str() {
            "org.freedesktop.systemd1.Unit" => {
                let obj = UnitObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Interface::get(&obj, &prop_name, server, connection, None, emitter).await
            }
            "org.freedesktop.systemd1.Service" if kind == "service" => {
                let obj = ServiceObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Interface::get(&obj, &prop_name, server, connection, None, emitter).await
            }
            "org.freedesktop.systemd1.Socket" if kind == "socket" => {
                let obj = SocketObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Interface::get(&obj, &prop_name, server, connection, None, emitter).await
            }
            "org.freedesktop.systemd1.Mount" if kind == "mount" => {
                let obj = MountObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Interface::get(&obj, &prop_name, server, connection, None, emitter).await
            }
            "org.freedesktop.systemd1.Slice" if kind == "slice" => {
                let obj = SliceObject {
                    unit_name: self.unit_name.clone(),
                };
                Interface::get(&obj, &prop_name, server, connection, None, emitter).await
            }
            "org.freedesktop.systemd1.Scope" if kind == "scope" => {
                let obj = ScopeObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Interface::get(&obj, &prop_name, server, connection, None, emitter).await
            }
            "org.freedesktop.DBus.Properties"
            | "org.freedesktop.DBus.Peer"
            | "org.freedesktop.DBus.Introspectable"
            | "org.freedesktop.DBus.ObjectManager" => None,
            _ => {
                let err = fdo::Error::UnknownInterface(sysa::l10n::fmt(
                    sysa::l10n::t_("Unknown interface '{iface_name}'."),
                    &[("iface_name", &iface_name.to_string())],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
                return Ok(());
            }
        };

        match value {
            Some(Ok(v)) => {
                let v: zbus::Variant<'_> = v.into();
                connection.reply(&header, &v).await?;
            }
            Some(Err(e)) => {
                connection.reply_dbus_error(&msg.header(), e).await?;
            }
            None => {
                let err = fdo::Error::UnknownProperty(sysa::l10n::fmt(
                    sysa::l10n::t_("Unknown property '{prop_name}'."),
                    &[("prop_name", &prop_name.to_string())],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
            }
        }
        Ok(())
    }

    async fn handle_set(
        &self,
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
        let err =
            fdo::Error::PropertyReadOnly(sysa::l10n::t_("Properties are read-only").to_string());
        connection.reply_dbus_error(&msg.header(), err).await?;
        Ok(())
    }
}

#[async_trait]
impl Interface for Properties {
    fn name() -> InterfaceName<'static>
    where
        Self: Sized,
    {
        InterfaceName::from_static_str_unchecked("org.freedesktop.DBus.Properties")
    }

    async fn get(
        &self,
        _property_name: &str,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> Option<Result<OwnedValue, BoxDBusError>> {
        None
    }

    async fn get_all(
        &self,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> fdo::Result<HashMap<String, OwnedValue>> {
        Ok(HashMap::new())
    }

    async fn set_mut(
        &mut self,
        _property_name: &str,
        _value: &zbus::Variant<'_>,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> Option<Result<(), BoxDBusError>> {
        None
    }

    fn call<'call>(
        &'call self,
        server: &'call ObjectServer,
        connection: &'call Connection,
        msg: &'call zbus::message::Message,
        name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        let Ok(emitter) = SignalEmitter::new(connection, self.object_path()) else {
            return DispatchResult::NotFound;
        };
        match name.as_str() {
            "GetAll" => {
                let emitter = emitter.clone();
                DispatchResult::Async(Box::pin(async move {
                    self.handle_get_all(server, connection, msg, &emitter)
                        .await
                        .map_err(box_dbus_error)
                }))
            }
            "Get" => {
                let emitter = emitter.clone();
                DispatchResult::Async(Box::pin(async move {
                    self.handle_get(server, connection, msg, &emitter)
                        .await
                        .map_err(box_dbus_error)
                }))
            }
            "Set" => DispatchResult::Async(Box::pin(async move {
                self.handle_set(connection, msg)
                    .await
                    .map_err(box_dbus_error)
            })),
            _ => DispatchResult::NotFound,
        }
    }

    fn call_mut<'call>(
        &'call mut self,
        _server: &'call ObjectServer,
        _connection: &'call Connection,
        _msg: &'call zbus::message::Message,
        _name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        DispatchResult::NotFound
    }

    fn introspect_to_writer(&self, writer: &mut dyn Write, level: usize) {
        introspect_properties(writer, level);
    }
}

#[async_trait]
impl Interface for FallbackProperties {
    fn name() -> InterfaceName<'static>
    where
        Self: Sized,
    {
        InterfaceName::from_static_str_unchecked("org.freedesktop.DBus.Properties")
    }

    async fn get(
        &self,
        _property_name: &str,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> Option<Result<OwnedValue, BoxDBusError>> {
        None
    }

    async fn get_all(
        &self,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> fdo::Result<HashMap<String, OwnedValue>> {
        Ok(HashMap::new())
    }

    async fn set_mut(
        &mut self,
        _property_name: &str,
        _value: &zbus::Variant<'_>,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> Option<Result<(), BoxDBusError>> {
        None
    }

    fn call<'call>(
        &'call self,
        server: &'call ObjectServer,
        connection: &'call Connection,
        msg: &'call zbus::message::Message,
        name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        let Some((unit_name, path)) = unit_from_message(msg) else {
            return DispatchResult::NotFound;
        };
        DispatchResult::Async(Box::pin(async move {
            let unit_name = self.load(&unit_name).await?;
            let emitter = SignalEmitter::new(connection, path).map_err(box_dbus_error)?;
            let props = Properties {
                ctx: self.ctx.clone(),
                unit_name,
            };
            match name.as_str() {
                "GetAll" => props
                    .handle_get_all(server, connection, msg, &emitter)
                    .await
                    .map_err(box_dbus_error),
                "Get" => props
                    .handle_get(server, connection, msg, &emitter)
                    .await
                    .map_err(box_dbus_error),
                "Set" => props
                    .handle_set(connection, msg)
                    .await
                    .map_err(box_dbus_error),
                _ => Ok(()),
            }
        }))
    }

    fn call_mut<'call>(
        &'call mut self,
        _server: &'call ObjectServer,
        _connection: &'call Connection,
        _msg: &'call zbus::message::Message,
        _name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        DispatchResult::NotFound
    }

    fn introspect_to_writer(&self, writer: &mut dyn Write, level: usize) {
        introspect_properties(writer, level);
    }
}

/// Placeholder interface that materialises the unit-prefix node, so zbus's
/// built-in `Properties` can be removed from it before the fallback is
/// installed. It stays registered: removing it prunes the node (and with it
/// the fallback), because zbus only counts interfaces, not fallbacks, when it
/// decides whether a node is empty.
pub struct UnitPathMarker;

#[async_trait]
impl Interface for UnitPathMarker {
    fn name() -> InterfaceName<'static>
    where
        Self: Sized,
    {
        InterfaceName::from_static_str_unchecked("org.freedesktop.systemd1.internal.UnitPath")
    }

    async fn get(
        &self,
        _property_name: &str,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> Option<Result<OwnedValue, BoxDBusError>> {
        None
    }

    async fn get_all(
        &self,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> fdo::Result<HashMap<String, OwnedValue>> {
        Ok(HashMap::new())
    }

    async fn set_mut(
        &mut self,
        _property_name: &str,
        _value: &zbus::Variant<'_>,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> Option<Result<(), BoxDBusError>> {
        None
    }

    fn call<'call>(
        &'call self,
        _server: &'call ObjectServer,
        _connection: &'call Connection,
        _msg: &'call zbus::message::Message,
        _name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        DispatchResult::NotFound
    }

    fn call_mut<'call>(
        &'call mut self,
        _server: &'call ObjectServer,
        _connection: &'call Connection,
        _msg: &'call zbus::message::Message,
        _name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        DispatchResult::NotFound
    }

    fn introspect_to_writer(&self, _writer: &mut dyn Write, _level: usize) {}
}

/// Custom `org.freedesktop.DBus.Properties` for the manager object.
pub struct ManagerProperties {
    pub ctx: Arc<BridgeContext>,
}

impl ManagerProperties {
    async fn manager_iface_props(
        &self,
        server: &ObjectServer,
        connection: &Connection,
        emitter: &SignalEmitter<'_>,
    ) -> fdo::Result<HashMap<String, OwnedValue>> {
        let obj = ManagerInterface::new(self.ctx.clone());
        Interface::get_all(&obj, server, connection, None, emitter).await
    }

    async fn handle_get_all(
        &self,
        server: &ObjectServer,
        connection: &Connection,
        msg: &zbus::message::Message,
        emitter: &SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let header = msg.header();
        let body = msg.body();
        let (iface_name,): (String,) = match body.deserialize() {
            Ok(r) => r,
            Err(e) => {
                let err = fdo::Error::InvalidArgs(sysa::l10n::fmt(
                    sysa::l10n::t_("Bad arguments: {e}."),
                    &[("e", &e.to_string())],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
                return Ok(());
            }
        };
        match iface_name.as_str() {
            "" | "org.freedesktop.systemd1.Manager" => {
                match self.manager_iface_props(server, connection, emitter).await {
                    Ok(props) => {
                        connection.reply(&header, &props).await?;
                    }
                    Err(e) => {
                        connection.reply_dbus_error(&msg.header(), e).await?;
                    }
                }
            }
            "org.freedesktop.DBus.Properties"
            | "org.freedesktop.DBus.Peer"
            | "org.freedesktop.DBus.Introspectable"
            | "org.freedesktop.DBus.ObjectManager" => {
                let empty: HashMap<String, OwnedValue> = HashMap::new();
                connection.reply(&header, &empty).await?;
            }
            _ => {
                let err = fdo::Error::UnknownInterface(sysa::l10n::fmt(
                    sysa::l10n::t_("Unknown interface '{iface_name}'."),
                    &[("iface_name", &iface_name.to_string())],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
            }
        }
        Ok(())
    }

    async fn handle_get(
        &self,
        server: &ObjectServer,
        connection: &Connection,
        msg: &zbus::message::Message,
        emitter: &SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let header = msg.header();
        let body = msg.body();
        let (iface_name, prop_name): (String, String) = match body.deserialize() {
            Ok(r) => r,
            Err(e) => {
                let err = fdo::Error::InvalidArgs(sysa::l10n::fmt(
                    sysa::l10n::t_("Bad arguments: {e}."),
                    &[("e", &e.to_string())],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
                return Ok(());
            }
        };
        if iface_name != "org.freedesktop.systemd1.Manager" {
            let err = fdo::Error::UnknownInterface(sysa::l10n::fmt(
                sysa::l10n::t_("Unknown interface '{iface_name}'."),
                &[("iface_name", &iface_name.to_string())],
            ));
            connection.reply_dbus_error(&msg.header(), err).await?;
            return Ok(());
        }
        let obj = ManagerInterface::new(self.ctx.clone());
        match Interface::get(&obj, &prop_name, server, connection, None, emitter).await {
            Some(Ok(v)) => {
                let v: zbus::Variant<'_> = v.into();
                connection.reply(&header, &v).await?;
            }
            Some(Err(e)) => {
                connection.reply_dbus_error(&msg.header(), e).await?;
            }
            None => {
                let err = fdo::Error::UnknownProperty(sysa::l10n::fmt(
                    sysa::l10n::t_("Unknown property '{prop_name}'."),
                    &[("prop_name", &prop_name.to_string())],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
            }
        }
        Ok(())
    }

    async fn handle_set(
        &self,
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
        let err =
            fdo::Error::PropertyReadOnly(sysa::l10n::t_("Properties are read-only").to_string());
        connection.reply_dbus_error(&msg.header(), err).await?;
        Ok(())
    }
}

#[async_trait]
impl Interface for ManagerProperties {
    fn name() -> InterfaceName<'static>
    where
        Self: Sized,
    {
        InterfaceName::from_static_str_unchecked("org.freedesktop.DBus.Properties")
    }

    async fn get(
        &self,
        _property_name: &str,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> Option<Result<OwnedValue, BoxDBusError>> {
        None
    }

    async fn get_all(
        &self,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> fdo::Result<HashMap<String, OwnedValue>> {
        Ok(HashMap::new())
    }

    async fn set_mut(
        &mut self,
        _property_name: &str,
        _value: &zbus::Variant<'_>,
        _server: &ObjectServer,
        _connection: &Connection,
        _header: Option<&Header<'_>>,
        _emitter: &SignalEmitter<'_>,
    ) -> Option<Result<(), BoxDBusError>> {
        None
    }

    fn call<'call>(
        &'call self,
        server: &'call ObjectServer,
        connection: &'call Connection,
        msg: &'call zbus::message::Message,
        name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        let Ok(emitter) = SignalEmitter::new(connection, self.object_path()) else {
            return DispatchResult::NotFound;
        };
        match name.as_str() {
            "GetAll" => {
                let emitter = emitter.clone();
                DispatchResult::Async(Box::pin(async move {
                    self.handle_get_all(server, connection, msg, &emitter)
                        .await
                        .map_err(box_dbus_error)
                }))
            }
            "Get" => {
                let emitter = emitter.clone();
                DispatchResult::Async(Box::pin(async move {
                    self.handle_get(server, connection, msg, &emitter)
                        .await
                        .map_err(box_dbus_error)
                }))
            }
            "Set" => DispatchResult::Async(Box::pin(async move {
                self.handle_set(connection, msg)
                    .await
                    .map_err(box_dbus_error)
            })),
            _ => DispatchResult::NotFound,
        }
    }

    fn call_mut<'call>(
        &'call mut self,
        _server: &'call ObjectServer,
        _connection: &'call Connection,
        _msg: &'call zbus::message::Message,
        _name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        DispatchResult::NotFound
    }

    fn introspect_to_writer(&self, writer: &mut dyn Write, level: usize) {
        introspect_properties(writer, level);
    }
}

/// Shared introspection XML for the custom Properties interfaces.
fn introspect_properties(writer: &mut dyn Write, level: usize) {
    writeln!(
        writer,
        "{:indent$}<interface name=\"org.freedesktop.DBus.Properties\">",
        "",
        indent = level
    )
    .unwrap();
    let l = level + 2;
    writeln!(writer, "{:indent$}<method name=\"Get\">", "", indent = l).unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"s\" name=\"interface_name\" direction=\"in\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"s\" name=\"property_name\" direction=\"in\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"v\" name=\"value\" direction=\"out\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(writer, "{:indent$}</method>", "", indent = l).unwrap();
    writeln!(writer, "{:indent$}<method name=\"GetAll\">", "", indent = l).unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"s\" name=\"interface_name\" direction=\"in\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"a{{sv}}\" name=\"properties\" direction=\"out\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(writer, "{:indent$}</method>", "", indent = l).unwrap();
    writeln!(writer, "{:indent$}<method name=\"Set\">", "", indent = l).unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"s\" name=\"interface_name\" direction=\"in\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"s\" name=\"property_name\" direction=\"in\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"v\" name=\"value\" direction=\"in\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(writer, "{:indent$}</method>", "", indent = l).unwrap();
    writeln!(
        writer,
        "{:indent$}<signal name=\"PropertiesChanged\">",
        "",
        indent = l
    )
    .unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"s\" name=\"interface_name\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"a{{sv}}\" name=\"changed_properties\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(
        writer,
        "{:indent$}<arg type=\"as\" name=\"invalidated_properties\"/>",
        "",
        indent = l + 2
    )
    .unwrap();
    writeln!(writer, "{:indent$}</signal>", "", indent = l).unwrap();
    writeln!(writer, "{:indent$}</interface>", "", indent = level).unwrap();
}
