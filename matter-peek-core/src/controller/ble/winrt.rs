//! The GATT Central on Windows: rs-matter's, over its Bluetooth LE API (WinRT).

use alloc::string::String;

pub(super) use rs_matter::transport::network::btp::win32::{run_central, scan};

use rs_matter::transport::network::btp::win32::central_unavailable;

/// Why the central cannot run now, if it cannot: there is no Bluetooth LE
/// adapter that can connect to devices, or it is off.
pub(super) async fn unavailable() -> Option<String> {
    central_unavailable().await.map(Into::into)
}
