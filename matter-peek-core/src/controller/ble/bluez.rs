//! The GATT Central on Linux: rs-matter's, over BlueZ on the system D-Bus.

use alloc::string::String;
use alloc::vec::Vec;

use rs_matter::error::Error;
use rs_matter::transport::network::btp::{bluez, AdvData, Btp};
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::BtAddr;
use rs_matter::utils::zbus::fdo::ObjectManagerProxy;
use rs_matter::utils::zbus::zvariant::OwnedValue;

use crate::dbus::{has_owner, system_bus};

/// Report the commissionable devices matching `filter` to `on_found`, until it
/// returns `Some` or the scan times out.
pub(super) async fn scan<F, R>(
    filter: &CommissionableFilter,
    scan_timeout: Option<u16>,
    on_found: F,
) -> Result<R, Error>
where
    F: FnMut(BtAddr, &AdvData) -> Option<R>,
{
    let bus = system_bus().await?;

    bluez::scan(&bus, None, filter, scan_timeout, on_found).await
}

/// Connect to the device at `addr`, found by [`scan`], and carry `btp` over the
/// connection until it ends.
pub(super) async fn run_central(addr: BtAddr, btp: &Btp) -> Result<(), Error> {
    let bus = system_bus().await?;

    bluez::run_central(&bus, None, addr, btp).await
}

/// Why the central cannot run now, if it cannot: BlueZ does not run, it has no
/// Bluetooth adapter, or every adapter is off.
pub(super) async fn unavailable() -> Option<String> {
    let Ok(bus) = system_bus().await else {
        return Some("the system D-Bus, where BlueZ is, cannot be reached".into());
    };
    if !has_owner(&bus, "org.bluez").await {
        return Some("BlueZ is not installed, or does not run".into());
    }

    let objects = match ObjectManagerProxy::new(&bus, "org.bluez", "/").await {
        Ok(manager) => manager.get_managed_objects().await.ok(),
        Err(_) => None,
    };
    let Some(objects) = objects else {
        return Some("BlueZ does not answer".into());
    };

    let adapters: Vec<bool> = objects
        .values()
        .filter_map(|interfaces| interfaces.get("org.bluez.Adapter1"))
        .map(|adapter| {
            adapter
                .get("Powered")
                .and_then(|on| bool::try_from(OwnedValue::clone(on)).ok())
                .unwrap_or(false)
        })
        .collect();

    if adapters.is_empty() {
        Some("there is no Bluetooth adapter".into())
    } else if !adapters.contains(&true) {
        Some("the Bluetooth adapter is off".into())
    } else {
        None
    }
}
