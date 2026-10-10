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
use zbus::names::InterfaceName;
use zbus::object_server::{DispatchResult, Interface, SignalContext};
use zbus::{fdo, Connection, ObjectServer};
use zvariant::OwnedValue;

use super::manager::ManagerInterface;
use super::mount_obj::MountObject;
use super::scope_obj::ScopeObject;
use super::service_obj::ServiceObject;
use super::slice_obj::SliceObject;
use super::socket_obj::SocketObject;
use super::unit_obj::UnitObject;
use super::BridgeContext;

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
    ) -> Option<fdo::Result<HashMap<String, OwnedValue>>> {
        let kind = self.kind();
        match iface_name {
            "org.freedesktop.systemd1.Unit" => {
                let obj = UnitObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(obj.get_all().await)
            }
            "org.freedesktop.systemd1.Service" if kind == "service" => {
                let obj = ServiceObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(obj.get_all().await)
            }
            "org.freedesktop.systemd1.Socket" if kind == "socket" => {
                let obj = SocketObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(obj.get_all().await)
            }
            "org.freedesktop.systemd1.Mount" if kind == "mount" => {
                let obj = MountObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(obj.get_all().await)
            }
            "org.freedesktop.systemd1.Slice" if kind == "slice" => {
                let obj = SliceObject {
                    unit_name: self.unit_name.clone(),
                };
                Some(obj.get_all().await)
            }
            "org.freedesktop.systemd1.Scope" if kind == "scope" => {
                let obj = ScopeObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(obj.get_all().await)
            }
            "org.freedesktop.DBus.Properties"
            | "org.freedesktop.DBus.Peer"
            | "org.freedesktop.DBus.Introspectable"
            | "org.freedesktop.DBus.ObjectManager" => Some(Ok(HashMap::new())),
            _ => None,
        }
    }

    async fn all_props(&self) -> HashMap<String, OwnedValue> {
        let mut result = HashMap::new();
        let unit_obj = UnitObject {
            ctx: self.ctx.clone(),
            unit_name: self.unit_name.clone(),
        };
        if let Ok(props) = unit_obj.get_all().await {
            result.extend(props);
        }
        match self.kind().as_str() {
            "service" => {
                let obj = ServiceObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                if let Ok(props) = obj.get_all().await {
                    result.extend(props);
                }
            }
            "socket" => {
                let obj = SocketObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                if let Ok(props) = obj.get_all().await {
                    result.extend(props);
                }
            }
            "mount" => {
                let obj = MountObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                if let Ok(props) = obj.get_all().await {
                    result.extend(props);
                }
            }
            _ => {}
        }
        result
    }

    async fn handle_get_all(
        &self,
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
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
            let props = self.all_props().await;
            connection.reply(msg, &props).await?;
        } else {
            match self.props_for_interface(&iface_name).await {
                Some(Ok(props)) => {
                    connection.reply(msg, &props).await?;
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
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
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
                obj.get(&prop_name).await
            }
            "org.freedesktop.systemd1.Service" if kind == "service" => {
                let obj = ServiceObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                obj.get(&prop_name).await
            }
            "org.freedesktop.systemd1.Socket" if kind == "socket" => {
                let obj = SocketObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                obj.get(&prop_name).await
            }
            "org.freedesktop.systemd1.Mount" if kind == "mount" => {
                let obj = MountObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                obj.get(&prop_name).await
            }
            "org.freedesktop.systemd1.Slice" if kind == "slice" => {
                let obj = SliceObject {
                    unit_name: self.unit_name.clone(),
                };
                obj.get(&prop_name).await
            }
            "org.freedesktop.systemd1.Scope" if kind == "scope" => {
                let obj = ScopeObject {
                    ctx: self.ctx.clone(),
                    unit_name: self.unit_name.clone(),
                };
                obj.get(&prop_name).await
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
                let v: zvariant::Value<'_> = v.into();
                connection.reply(msg, &v).await?;
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

    async fn get(&self, _property_name: &str) -> Option<fdo::Result<OwnedValue>> {
        None
    }

    async fn get_all(&self) -> fdo::Result<HashMap<String, OwnedValue>> {
        Ok(HashMap::new())
    }

    async fn set_mut(
        &mut self,
        _property_name: &str,
        _value: &zvariant::Value<'_>,
        _ctxt: &SignalContext<'_>,
    ) -> Option<fdo::Result<()>> {
        None
    }

    fn call<'call>(
        &'call self,
        _server: &'call ObjectServer,
        connection: &'call Connection,
        msg: &'call zbus::message::Message,
        name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        match name.as_str() {
            "GetAll" => DispatchResult::Async(Box::pin(async move {
                self.handle_get_all(connection, msg).await
            })),
            "Get" => DispatchResult::Async(Box::pin(
                async move { self.handle_get(connection, msg).await },
            )),
            "Set" => DispatchResult::Async(Box::pin(
                async move { self.handle_set(connection, msg).await },
            )),
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

/// Custom `org.freedesktop.DBus.Properties` for the manager object.
pub struct ManagerProperties {
    pub ctx: Arc<BridgeContext>,
}

impl ManagerProperties {
    async fn manager_iface_props(&self) -> fdo::Result<HashMap<String, OwnedValue>> {
        let obj = ManagerInterface::new(self.ctx.clone());
        obj.get_all().await
    }

    async fn handle_get_all(
        &self,
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
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
            "" | "org.freedesktop.systemd1.Manager" => match self.manager_iface_props().await {
                Ok(props) => {
                    connection.reply(msg, &props).await?;
                }
                Err(e) => {
                    connection.reply_dbus_error(&msg.header(), e).await?;
                }
            },
            "org.freedesktop.DBus.Properties"
            | "org.freedesktop.DBus.Peer"
            | "org.freedesktop.DBus.Introspectable"
            | "org.freedesktop.DBus.ObjectManager" => {
                let empty: HashMap<String, OwnedValue> = HashMap::new();
                connection.reply(msg, &empty).await?;
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
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
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
        match obj.get(&prop_name).await {
            Some(Ok(v)) => {
                let v: zvariant::Value<'_> = v.into();
                connection.reply(msg, &v).await?;
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

    async fn get(&self, _property_name: &str) -> Option<fdo::Result<OwnedValue>> {
        None
    }

    async fn get_all(&self) -> fdo::Result<HashMap<String, OwnedValue>> {
        Ok(HashMap::new())
    }

    async fn set_mut(
        &mut self,
        _property_name: &str,
        _value: &zvariant::Value<'_>,
        _ctxt: &SignalContext<'_>,
    ) -> Option<fdo::Result<()>> {
        None
    }

    fn call<'call>(
        &'call self,
        _server: &'call ObjectServer,
        connection: &'call Connection,
        msg: &'call zbus::message::Message,
        name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        match name.as_str() {
            "GetAll" => DispatchResult::Async(Box::pin(async move {
                self.handle_get_all(connection, msg).await
            })),
            "Get" => DispatchResult::Async(Box::pin(
                async move { self.handle_get(connection, msg).await },
            )),
            "Set" => DispatchResult::Async(Box::pin(
                async move { self.handle_set(connection, msg).await },
            )),
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