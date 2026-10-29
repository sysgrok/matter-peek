//! The GATT Central on Windows, over its Bluetooth LE API (WinRT): the same
//! [`scan`] and [`run_central`] as rs-matter's BlueZ ones, on the default
//! adapter.
//!
//! WinRT runs event handlers on its own threads; channels bring what they get
//! to the controller's thread.

use core::future::IntoFuture;

use alloc::string::String;
use alloc::vec::Vec;

use async_channel::Receiver;

use embassy_futures::select::{select, select3, Either};
use embassy_time::{Duration, Instant, Timer};

use log::{debug, info, warn};

use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::btp::{
    AdvData, Btp, C1_CHARACTERISTIC_UUID, C2_CHARACTERISTIC_UUID, MATTER_BLE_SERVICE_UUID,
};
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::BtAddr;
use rs_matter::utils::select::Coalesce;

use windows::core::{Interface, GUID};
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher,
    BluetoothLEScanningMode,
};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattClientCharacteristicConfigurationDescriptorValue,
    GattCommunicationStatus, GattDeviceService, GattSession, GattValueChangedEventArgs,
    GattWriteOption,
};
use windows::Devices::Bluetooth::{BluetoothAdapter, BluetoothCacheMode, BluetoothLEDevice};
use windows::Devices::Radios::RadioState;
use windows::Foundation::{IClosable, TypedEventHandler};
use windows::Storage::Streams::{DataReader, DataWriter, IBuffer};

/// How long [`scan`] scans for a matching device when not told.
const DEFAULT_SCAN_TIMEOUT_SECS: u16 = 60;

/// Advertisements waiting for the scan; more are dropped, as devices advertise
/// again soon.
const ADVERTISEMENTS_QUEUED: usize = 16;

/// How many times the first GATT request, which connects, is tried, and the
/// wait between tries.
const CONNECT_ATTEMPTS: u8 = 3;
const CONNECT_RETRY_DELAY_MS: u64 = 500;

/// The ATT MTU before the peers exchange theirs.
const DEFAULT_ATT_MTU: u16 = 23;

/// How long to wait for Windows to exchange a larger ATT MTU after connecting,
/// and how often to look.
const MTU_WAIT_MS: u64 = 1000;
const MTU_POLL_MS: u64 = 50;

/// A WinRT object, closed when dropped: Windows keeps the connection while
/// the device, its GATT session or its services are open.
struct Closing(Option<IClosable>);

impl Closing {
    fn new<T: Interface>(object: &T) -> Self {
        Self(object.cast().ok())
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        if let Some(object) = &self.0 {
            let _ = object.Close();
        }
    }
}

/// The scan's handler on the watcher; dropping it ends the scan.
struct Watching<'a> {
    watcher: &'a BluetoothLEAdvertisementWatcher,
    token: i64,
}

impl Drop for Watching<'_> {
    fn drop(&mut self) {
        let _ = self.watcher.Stop();
        let _ = self.watcher.RemoveReceived(self.token);
    }
}

/// The indication handler on C2, removed when dropped.
struct Listening<'a> {
    c2: &'a GattCharacteristic,
    token: i64,
}

impl Drop for Listening<'_> {
    fn drop(&mut self) {
        let _ = self.c2.RemoveValueChanged(self.token);
    }
}

/// Report the commissionable devices matching `filter` to `on_found`, each
/// once, until it returns `Some` or the scan times out (then
/// `NoNetworkInterface`).
pub(super) async fn scan<F, R>(
    filter: &CommissionableFilter,
    scan_timeout: Option<u16>,
    mut on_found: F,
) -> Result<R, Error>
where
    F: FnMut(BtAddr, &AdvData) -> Option<R>,
{
    let what = "scan for Bluetooth LE devices";

    let watcher = BluetoothLEAdvertisementWatcher::new().map_err(btp_error(what))?;
    watcher
        .SetScanningMode(BluetoothLEScanningMode::Active)
        .map_err(btp_error(what))?;

    let (sender, advertisements) = async_channel::bounded(ADVERTISEMENTS_QUEUED);
    let handler = TypedEventHandler::<
        BluetoothLEAdvertisementWatcher,
        BluetoothLEAdvertisementReceivedEventArgs,
    >::new(move |_, args| {
        if let Some(found) = args.as_ref().and_then(matter_advertisement) {
            let _ = sender.try_send(found);
        }

        Ok(())
    });

    let _watching = Watching {
        watcher: &watcher,
        token: watcher.Received(&handler).map_err(btp_error(what))?,
    };
    watcher.Start().map_err(btp_error(what))?;

    info!("Scanning for a commissionable Matter device (filter: {filter:?})");

    let mut reported = Vec::new();
    let found = async {
        while let Ok((addr, adv)) = advertisements.recv().await {
            if reported.contains(&addr) || !adv.matches(filter) {
                continue;
            }
            reported.push(addr);

            debug!("Matched commissionable device {addr} (adv: {adv:?})");

            if let Some(result) = on_found(addr, &adv) {
                return Some(result);
            }
        }

        None
    };

    let timeout = Timer::after(Duration::from_secs(
        scan_timeout.unwrap_or(DEFAULT_SCAN_TIMEOUT_SECS).into(),
    ));

    match select(found, timeout).await {
        Either::First(Some(result)) => Ok(result),
        _ => {
            warn!("No commissionable Matter device matching the filter was found within the scan timeout");
            Err(ErrorCode::NoNetworkInterface.into())
        }
    }
}

/// Connect to the device at `addr`, found by [`scan`], and carry `btp` over the
/// connection, as the initiator, until it fails or the peer goes silent.
pub(super) async fn run_central(addr: BtAddr, btp: &Btp) -> Result<(), Error> {
    info!("Connecting to commissionable device {addr}");

    let device = result(BluetoothLEDevice::FromBluetoothAddressAsync(windows_addr(
        addr,
    )))
    .await
    .map_err(btp_error("find the device"))?;
    let _device = Closing::new(&device);

    // Windows connects on the first GATT request, and drops an idle connection
    // unless the session keeps it.
    let session = result(
        device
            .BluetoothDeviceId()
            .and_then(|id| GattSession::FromDeviceIdAsync(&id)),
    )
    .await
    .map_err(btp_error("open a GATT session"))?;
    let _session = Closing::new(&session);
    session
        .SetMaintainConnection(true)
        .map_err(btp_error("keep the connection"))?;

    let service = matter_service(&device).await?;
    let _service = Closing::new(&service);

    let c1 = characteristic(&service, C1_CHARACTERISTIC_UUID, "C1").await?;
    let c2 = characteristic(&service, C2_CHARACTERISTIC_UUID, "C2").await?;

    let gatt_mtu = att_mtu(&session).await;

    debug!("Discovered Matter GATT characteristics C1/C2, ATT MTU: {gatt_mtu:?}");

    // As over BlueZ: the handshake goes to C1 before we subscribe to C2, which
    // is when a Matter SDK peripheral sends its response.
    btp.set_initiator(true);
    let mut buf = [0; 512];
    let len = btp.process_outgoing(gatt_mtu, &mut buf)?;
    if len == 0 {
        return Err(ErrorCode::InvalidState.into());
    }
    write_c1(&c1, &buf[..len]).await?;

    let (sender, indications) = async_channel::unbounded();
    let handler =
        TypedEventHandler::<GattCharacteristic, GattValueChangedEventArgs>::new(move |_, args| {
            let value = args
                .as_ref()
                .and_then(|args| args.CharacteristicValue().ok())
                .and_then(|value| bytes(&value).ok());
            if let Some(value) = value {
                let _ = sender.try_send(value);
            }

            Ok(())
        });
    let _listening = Listening {
        c2: &c2,
        token: c2
            .ValueChanged(&handler)
            .map_err(btp_error("listen to C2"))?,
    };

    info!("C1 handshake acknowledged; subscribing to C2 indications");
    let status = result(c2.WriteClientCharacteristicConfigurationDescriptorAsync(
        GattClientCharacteristicConfigurationDescriptorValue::Indicate,
    ))
    .await
    .map_err(btp_error("subscribe to C2"))?;
    check(status, "subscribe to C2")?;
    info!("C2 indication subscription completed; waiting for BTP response");

    select3(
        wait_timeout(btp),
        process_c2_indications(btp, addr, gatt_mtu, &indications),
        process_c1_writes(btp, gatt_mtu, &c1, &mut buf),
    )
    .coalesce()
    .await
}

/// Why the central cannot run now, if it cannot: there is no Bluetooth LE
/// adapter that can connect to devices, or it is off.
pub(super) async fn unavailable() -> Option<String> {
    let Ok(adapter) = result(BluetoothAdapter::GetDefaultAsync()).await else {
        return Some("there is no Bluetooth adapter".into());
    };

    if !adapter.IsLowEnergySupported().unwrap_or(false)
        || !adapter.IsCentralRoleSupported().unwrap_or(false)
    {
        return Some("the Bluetooth adapter cannot connect to Bluetooth LE devices".into());
    }

    // An unknown radio state is no reason to refuse.
    let state = result(adapter.GetRadioAsync())
        .await
        .and_then(|radio| radio.State());
    if state.is_ok_and(|state| state != RadioState::On) {
        return Some("the Bluetooth adapter is off".into());
    }

    None
}

/// The Matter commissionable advertisement in `args`, if it has one.
fn matter_advertisement(
    args: &BluetoothLEAdvertisementReceivedEventArgs,
) -> Option<(BtAddr, AdvData)> {
    // The advertisement as sent: a length-type-data record per section.
    let mut raw = Vec::new();
    for section in args.Advertisement().ok()?.DataSections().ok()? {
        let data = bytes(&section.Data().ok()?).ok()?;
        raw.push(u8::try_from(data.len() + 1).ok()?);
        raw.push(section.DataType().ok()?);
        raw.extend_from_slice(&data);
    }

    let adv = AdvData::parse_adv(&raw)?;

    Some((bt_addr(args.BluetoothAddress().ok()?), adv))
}

/// The device's Matter GATT service. Asking for it connects, so a device that
/// is unreachable is asked again a few times.
async fn matter_service(device: &BluetoothLEDevice) -> Result<GattDeviceService, Error> {
    let what = "discover the Matter GATT service";

    let mut attempt = 1;
    let services = loop {
        // Uncached: Windows may remember the services of another device that
        // had this address.
        let services = result(device.GetGattServicesForUuidWithCacheModeAsync(
            GUID::from_u128(MATTER_BLE_SERVICE_UUID),
            BluetoothCacheMode::Uncached,
        ))
        .await
        .map_err(btp_error(what))?;
        let status = services.Status().map_err(btp_error(what))?;

        if status != GattCommunicationStatus::Unreachable || attempt == CONNECT_ATTEMPTS {
            check(status, what)?;
            break services;
        }

        warn!("Connect attempt {attempt}/{CONNECT_ATTEMPTS}: the device is unreachable; retrying");
        attempt += 1;
        Timer::after(Duration::from_millis(CONNECT_RETRY_DELAY_MS)).await;
    };

    services
        .Services()
        .map_err(btp_error(what))?
        .into_iter()
        .next()
        .ok_or_else(|| {
            warn!("The connected device does not expose the Matter GATT service");
            ErrorCode::NoNetworkInterface.into()
        })
}

/// The characteristic `uuid`, called `name`, of the Matter GATT service.
async fn characteristic(
    service: &GattDeviceService,
    uuid: u128,
    name: &str,
) -> Result<GattCharacteristic, Error> {
    let what = "discover the Matter GATT characteristics";

    let characteristics = result(service.GetCharacteristicsForUuidWithCacheModeAsync(
        GUID::from_u128(uuid),
        BluetoothCacheMode::Uncached,
    ))
    .await
    .map_err(btp_error(what))?;
    check(characteristics.Status().map_err(btp_error(what))?, what)?;

    characteristics
        .Characteristics()
        .map_err(btp_error(what))?
        .into_iter()
        .next()
        .ok_or_else(|| {
            warn!("The Matter GATT service is missing the {name} characteristic");
            ErrorCode::NoNetworkInterface.into()
        })
}

/// The ATT MTU. Windows exchanges a larger one on its own after connecting,
/// so this waits a little while it still reads as the default.
async fn att_mtu(session: &GattSession) -> Option<u16> {
    let deadline = Instant::now() + Duration::from_millis(MTU_WAIT_MS);

    loop {
        let mtu = session.MaxPduSize().ok()?;
        if mtu > DEFAULT_ATT_MTU || Instant::now() >= deadline {
            return Some(mtu);
        }

        Timer::after(Duration::from_millis(MTU_POLL_MS)).await;
    }
}

/// End once the peer has sent nothing for the BTP idle timeout. A disconnect
/// shows up sooner, as a failed C1 write.
async fn wait_timeout(btp: &Btp) -> Result<(), Error> {
    btp.wait_timeout().await;
    info!("Timeout while waiting for data from the peer");

    Ok(())
}

/// Feed C2 indications into the BTP session.
async fn process_c2_indications(
    btp: &Btp,
    peer: BtAddr,
    gatt_mtu: Option<u16>,
    indications: &Receiver<Vec<u8>>,
) -> Result<(), Error> {
    while let Ok(value) = indications.recv().await {
        // Never a BTP frame; the BTP layer would fail the session on it.
        if value.is_empty() {
            continue;
        }

        btp.process_incoming(gatt_mtu, peer, &value)
            .inspect_err(|e| {
                warn!(
                    "C2 indication rejected by BTP: {}B, error code {:?}",
                    value.len(),
                    e.code()
                )
            })?;
    }

    Ok(())
}

/// Write the BTP output to C1.
async fn process_c1_writes(
    btp: &Btp,
    gatt_mtu: Option<u16>,
    c1: &GattCharacteristic,
    buf: &mut [u8],
) -> Result<(), Error> {
    loop {
        let len = btp.process_outgoing(gatt_mtu, buf)?;

        if len > 0 {
            write_c1(c1, &buf[..len]).await?;
        } else {
            btp.wait_outgoing().await;
        }
    }
}

/// Write `value` to C1 as a Write Request, as Matter requires; waiting for the
/// response also paces the segments.
async fn write_c1(c1: &GattCharacteristic, value: &[u8]) -> Result<(), Error> {
    let what = "write to C1";

    let value = buffer(value).map_err(btp_error(what))?;
    let status = result(c1.WriteValueWithOptionAsync(&value, GattWriteOption::WriteWithResponse))
        .await
        .map_err(btp_error(what))?;

    check(status, what)
}

/// The result of a WinRT operation, once it completes.
async fn result<O, T>(op: windows::core::Result<O>) -> windows::core::Result<T>
where
    O: IntoFuture<Output = windows::core::Result<T>>,
{
    op?.await
}

/// Fail unless the GATT request succeeded.
fn check(status: GattCommunicationStatus, what: &str) -> Result<(), Error> {
    if status == GattCommunicationStatus::Success {
        Ok(())
    } else {
        warn!("Bluetooth: cannot {what}: GATT status {status:?}");
        Err(ErrorCode::BtpError.into())
    }
}

/// Log a WinRT error with what failed, and turn it into a BTP error, as
/// rs-matter does with BlueZ's.
fn btp_error(what: &str) -> impl FnOnce(windows::core::Error) -> Error + '_ {
    move |e| {
        warn!("Bluetooth: cannot {what}: {e}");
        ErrorCode::BtpError.into()
    }
}

/// The bytes in `buffer`.
fn bytes(buffer: &IBuffer) -> windows::core::Result<Vec<u8>> {
    let mut bytes = alloc::vec![0; buffer.Length()? as usize];
    DataReader::FromBuffer(buffer)?.ReadBytes(&mut bytes)?;

    Ok(bytes)
}

/// `bytes` in a buffer.
fn buffer(bytes: &[u8]) -> windows::core::Result<IBuffer> {
    let writer = DataWriter::new()?;
    writer.WriteBytes(bytes)?;

    writer.DetachBuffer()
}

/// Windows' address (48 bits, most significant byte first) as a `BtAddr`.
fn bt_addr(addr: u64) -> BtAddr {
    let [_, _, bytes @ ..] = addr.to_be_bytes();

    BtAddr(bytes)
}

/// A `BtAddr` as Windows' address.
fn windows_addr(addr: BtAddr) -> u64 {
    let mut bytes = [0; 8];
    bytes[2..].copy_from_slice(&addr.0);

    u64::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_round_trip() {
        let addr = BtAddr([0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]);

        assert_eq!(windows_addr(addr), 0x1234_5678_9ABC);
        assert_eq!(bt_addr(windows_addr(addr)), addr);
    }
}
