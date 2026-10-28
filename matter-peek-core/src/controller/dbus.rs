//! The system D-Bus, where the daemons the controller may use live: Avahi and
//! systemd-resolved for mDNS, BlueZ for Bluetooth.

use rs_matter::error::{Error, ErrorCode};
use rs_matter::utils::zbus::fdo::DBusProxy;
use rs_matter::utils::zbus::names::BusName;
use rs_matter::utils::zbus::Connection;

/// A connection to the system bus.
pub(crate) async fn system_bus() -> Result<Connection, Error> {
    Connection::system()
        .await
        .map_err(|_| ErrorCode::NoNetworkInterface.into())
}

/// Whether `name` has an owner on `bus`: its daemon runs. This starts no
/// daemon, unlike a call to it (D-Bus activation), which would start one that
/// is installed but stopped, maybe on purpose.
pub(crate) async fn has_owner(bus: &Connection, name: &'static str) -> bool {
    let Ok(dbus) = DBusProxy::new(bus).await else {
        return false;
    };
    let Ok(name) = BusName::try_from(name) else {
        return false;
    };

    dbus.name_has_owner(name).await.unwrap_or(false)
}
