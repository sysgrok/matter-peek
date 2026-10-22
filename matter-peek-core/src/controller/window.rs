//! A commissioning window on a device on our fabric, so that another
//! controller can commission it too: the enhanced window, with a passcode of
//! our own. The device gets only the PAKE verifier; the passcode reaches the
//! other controller as the manual pairing code or the QR code we show.

extern crate alloc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::num::NonZeroU8;

use rs_matter::crypto::{
    CanonEcPoint, CanonEcScalar, Crypto, CryptoSensitive, EcPoint as _, EcScalar as _, PbKdf as _,
    Rng as _, EC_CANON_POINT_LEN, EC_CANON_SCALAR_LEN, UINT320_CANON_LEN,
};
use rs_matter::dm::clusters::decl::administrator_commissioning::{
    self, AdministratorCommissioningAttrResponses as _, AdministratorCommissioningCmdRequests as _,
    CommissioningWindowStatusEnum,
};
use rs_matter::dm::clusters::decl::operational_credentials::{
    self, OperationalCredentialsAttrResponses as _,
};
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::im::client::ImClient as _;
use rs_matter::im::{AttrPath, GenericPath};
use rs_matter::pairing::qr::{no_optional_data, CommFlowType, Qr, QrPayload};
use rs_matter::pairing::DiscoveryCapabilities;
use rs_matter::sc::pase::Spake2pVerifierPassword;
use rs_matter::tlv::{Nullable, OctetStr};
use rs_matter::transport::exchange::Exchange;
use rs_matter::{BasicCommData, Matter};

/// The Administrator Commissioning and Operational Credentials clusters live
/// on the root node endpoint.
const ROOT_ENDPOINT: EndptId = 0;

/// The PBKDF2 iterations of the verifier: the least the spec allows, as CHIP.
const ITERATIONS: u32 = 1000;
/// The salt of the verifier: the longest the spec allows.
const SALT_LEN: usize = 32;
/// The PAKE verifier: `w0` and `L`.
pub const VERIFIER_LEN: usize = EC_CANON_SCALAR_LEN + EC_CANON_POINT_LEN;

/// How long the device may take to act on the timed invoke.
const TIMED_TIMEOUT_MS: u16 = 10_000;

/// The passcodes the spec forbids.
const INVALID_PASSCODES: [u32; 12] = [
    0, 11111111, 22222222, 33333333, 44444444, 55555555, 66666666, 77777777, 88888888, 99999999,
    12345678, 87654321,
];
/// The largest passcode.
const MAX_PASSCODE: u32 = 99999998;

/// Buffers for drawing a QR code: enough for the version a setup payload
/// needs.
const QR_BUF_LEN: usize = 1024;

/// What we need to show for a window we opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCode {
    pub passcode: u32,
    pub discriminator: u16,
}

/// A device's commissioning window and fabrics, as it reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowState {
    /// `None` if closed; else whether it is enhanced.
    pub open: Option<bool>,
    /// Whether we opened it: its opener's fabric is ours.
    pub ours: bool,
    /// The vendor of the controller that opened it.
    pub admin_vendor_id: Option<u16>,
    /// The fabrics it is on, and how many it supports.
    pub fabrics: Option<u8>,
    pub supported_fabrics: Option<u8>,
}

/// Open an enhanced commissioning window for `timeout_secs` on the device,
/// with a fresh passcode and discriminator.
pub async fn open<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    timeout_secs: u16,
) -> Result<WindowCode, Error> {
    let mut rand = crypto.rand()?;

    let code = WindowCode {
        passcode: passcode(|| rand.next_u32()),
        discriminator: (rand.next_u32() & 0x0FFF) as u16,
    };
    let mut salt = [0; SALT_LEN];
    rand.fill_bytes(&mut salt);
    let verifier = verifier(&crypto, code.passcode, &salt, ITERATIONS)?;

    let exchange = Exchange::initiate(matter, &crypto, fab_idx, node_id).await?;
    let mut chunk = exchange
        .invoke_with(Some(TIMED_TIMEOUT_MS), |msg| {
            msg.timed_request(true)?
                .invoke_requests()?
                .administrator_commissioning_inv()
                .open_commissioning_window(ROOT_ENDPOINT)?
                .commissioning_timeout(timeout_secs)?
                .pake_passcode_verifier(OctetStr::new(&verifier))?
                .discriminator(code.discriminator)?
                .iterations(ITERATIONS)?
                .salt(OctetStr::new(&salt))?
                .end()?
                .end()?
                .end()?
                .end()
        })
        .await?;
    while let Some(next) = chunk.complete().await? {
        chunk = next;
    }

    Ok(code)
}

/// Close the device's commissioning window, whoever opened it.
pub async fn revoke<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<(), Error> {
    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut chunk = exchange
        .invoke_with(Some(TIMED_TIMEOUT_MS), |msg| {
            msg.timed_request(true)?
                .invoke_requests()?
                .administrator_commissioning_inv()
                .revoke_commissioning(ROOT_ENDPOINT)?
                .end()?
                .end()
        })
        .await?;
    while let Some(next) = chunk.complete().await? {
        chunk = next;
    }

    Ok(())
}

/// Read the device's commissioning window and fabrics, in one round trip.
pub async fn read<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<WindowState, Error> {
    let paths = [
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(administrator_commissioning::FULL_CLUSTER.id),
            None,
        )),
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(operational_credentials::FULL_CLUSTER.id),
            Some(operational_credentials::AttributeId::SupportedFabrics as _),
        )),
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(operational_credentials::FULL_CLUSTER.id),
            Some(operational_credentials::AttributeId::CommissionedFabrics as _),
        )),
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(operational_credentials::FULL_CLUSTER.id),
            Some(operational_credentials::AttributeId::CurrentFabricIndex as _),
        )),
    ];

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut chunk = exchange
        .read_with(|msg| {
            msg.attr_requests_from(&paths)?
                .fabric_filtered(false)?
                .end()
        })
        .await?;

    let mut state = WindowState::default();
    let (mut admin_fabric, mut our_fabric) = (None, None);
    loop {
        {
            let resp = chunk.response()?;
            let window = resp.administrator_commissioning_read_resp();
            if let Some((_, Ok(status))) = window.window_status().next() {
                state.open = match status {
                    CommissioningWindowStatusEnum::WindowNotOpen => None,
                    CommissioningWindowStatusEnum::EnhancedWindowOpen => Some(true),
                    CommissioningWindowStatusEnum::BasicWindowOpen => Some(false),
                };
            }
            if let Some((_, Ok(vendor))) = window.admin_vendor_id().next() {
                state.admin_vendor_id = Nullable::into_option(vendor);
            }
            if let Some((_, Ok(fabric))) = window.admin_fabric_index().next() {
                admin_fabric = Nullable::into_option(fabric);
            }

            let fabrics = resp.operational_credentials_read_resp();
            if let Some((_, Ok(n))) = fabrics.supported_fabrics().next() {
                state.supported_fabrics = Some(n);
            }
            if let Some((_, Ok(n))) = fabrics.commissioned_fabrics().next() {
                state.fabrics = Some(n);
            }
            if let Some((_, Ok(index))) = fabrics.current_fabric_index().next() {
                our_fabric = Some(index);
            };
        }

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => {
                state.ours = admin_fabric.is_some() && admin_fabric == our_fabric;
                return Ok(state);
            }
        }
    }
}

/// The PAKE verifier of `passcode`: `w0` and `L = w1 * G`, from the PBKDF2 of
/// the passcode.
pub fn verifier<C: Crypto>(
    crypto: &C,
    passcode: u32,
    salt: &[u8],
    iterations: u32,
) -> Result<[u8; VERIFIER_LEN], Error> {
    let password: Spake2pVerifierPassword = passcode.to_le_bytes().into();
    let mut ws = CryptoSensitive::<{ UINT320_CANON_LEN * 2 }>::new();
    crypto
        .pbkdf()?
        .derive(password.reference(), iterations as usize, salt, &mut ws)?;

    let (w0s, w1s) = ws
        .reference()
        .split::<UINT320_CANON_LEN, UINT320_CANON_LEN>();
    let w0 = crypto.ec_scalar_mod_p(w0s)?;
    let w1 = crypto.ec_scalar_mod_p(w1s)?;
    let l = crypto.ec_generator_point()?.mul(&w1)?;

    let mut w0_canon = CanonEcScalar::new();
    w0.write_canon(&mut w0_canon)?;
    let mut l_canon = CanonEcPoint::new();
    l.write_canon(&mut l_canon)?;

    let mut verifier = [0; VERIFIER_LEN];
    verifier[..EC_CANON_SCALAR_LEN].copy_from_slice(w0_canon.access());
    verifier[EC_CANON_SCALAR_LEN..].copy_from_slice(l_canon.access());

    Ok(verifier)
}

/// The 11-digit manual pairing code and the QR code text (`MT:...`) of a
/// window on a device of vendor `vid` and product `pid`, found on the network.
pub fn setup_codes(code: &WindowCode, vid: u16, pid: u16) -> Result<(String, String), Error> {
    let comm_data = BasicCommData {
        password: code.passcode.to_le_bytes().into(),
        discriminator: code.discriminator,
    };
    let manual = comm_data.compute_pairing_code().to_string();

    let payload = QrPayload::new(
        DiscoveryCapabilities::IP,
        CommFlowType::Standard,
        comm_data,
        vid,
        pid,
        "",
        no_optional_data,
    );
    let mut buf = [0; 128];
    let (qr, _) = payload.as_str(&mut buf)?;

    Ok((manual, qr.to_string()))
}

/// The modules of the QR code of `text`, row by row, `1` dark.
pub fn qr_rows(text: &str) -> Result<Vec<String>, Error> {
    let mut tmp = vec![0; QR_BUF_LEN];
    let mut out = vec![0; QR_BUF_LEN];
    let qr = Qr::compute(text, &mut tmp, &mut out)?;
    let size = qr.size() as i32;

    Ok((0..size)
        .map(|y| {
            (0..size)
                .map(|x| if qr.get_module(x, y) { '1' } else { '0' })
                .collect()
        })
        .collect())
}

/// A random passcode the spec allows, from `next`.
fn passcode(mut next: impl FnMut() -> u32) -> u32 {
    loop {
        let passcode = next() % MAX_PASSCODE + 1;
        if !INVALID_PASSCODES.contains(&passcode) {
            return passcode;
        }
    }
}

#[cfg(test)]
mod tests {
    use rs_matter::crypto::test_only_crypto;

    use super::*;

    #[test]
    fn the_verifier_matches_chips() {
        // CHIP's test vector: passcode 20202021, salt "SPAKE2P Key Salt",
        // 1000 iterations.
        const CHIP: &str = "uWFwqugDNGiEck/po7KHwwMwwqZgN10XuyBajPGuyzUEV/iree4lOrao5GuwnlQ65CJzbeUB49s31EH+NEkg0JVI5MGCQGMMT/SRPFNRODm3wH/MBiehuFc6FJ/NH6Rmzw==";

        let verifier = verifier(&test_only_crypto(), 20202021, b"SPAKE2P Key Salt", 1000).unwrap();

        use base64::Engine as _;
        let expected = base64::engine::general_purpose::STANDARD
            .decode(CHIP)
            .unwrap();
        assert_eq!(verifier.as_slice(), expected.as_slice());
    }

    #[test]
    fn passcodes_are_valid() {
        let mut values = [0, 11111110, 99999998, 5].into_iter();
        let mut next = || passcode(|| values.next().unwrap());

        // 0 + 1 = 1; 11111110 + 1 is forbidden, so the next value is taken.
        assert_eq!(next(), 1);
        assert_eq!(next(), 1);
        assert_eq!(next(), 6);
    }

    #[test]
    fn codes_and_qr_carry_the_passcode() {
        let code = WindowCode {
            passcode: 20202021,
            discriminator: 3840,
        };
        let (manual, qr) = setup_codes(&code, 0xFFF1, 0x8000).unwrap();

        assert_eq!(manual, "34970112332");
        assert!(qr.starts_with("MT:"));
        let rows = qr_rows(&qr).unwrap();
        assert!(rows.len() >= 21 && rows.iter().all(|row| row.len() == rows.len()));
    }
}
