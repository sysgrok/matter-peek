//! The client side of intermittently connected devices (ICD Management): how
//! a device sleeps, and our registration for its Check-Ins.

use core::num::NonZeroU8;

use alloc::string::ToString;
use alloc::vec::Vec;

use log::warn;

use rs_matter::crypto::{CanonAeadKey, Crypto};
use rs_matter::dm::clusters::decl::icd_management::{
    ClientTypeEnum, Feature, IcdManagementAttrResponses as _, IcdManagementClient as _,
    OperatingModeEnum,
};
use rs_matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::im::encoding::ReportDataResp;
use rs_matter::sc::checkin::CheckIn;
use rs_matter::sc::AsyncScHandler;
use rs_matter::tlv::OctetStr;
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::api::{IcdInfo, IcdMode};
use crate::model::IcdClient;

/// The length of a Check-In key.
pub const KEY_LEN: usize = 16;

/// How far past its start the counter of a key may get before the key has to
/// be replaced: half its values are used then.
const KEY_REFRESH_OFFSET: u32 = 1 << 31;

/// Takes the Check-Ins devices send us, for whoever has the keys to make sense
/// of them.
pub struct CheckInReceiver<'a>(pub &'a async_channel::Sender<Vec<u8>>);

impl AsyncScHandler for CheckInReceiver<'_> {
    async fn check_in(&self, exchange: Exchange<'_>) -> Result<(), Error> {
        let payload = exchange.rx()?.payload().to_vec();

        if self.0.try_send(payload).is_err() {
            warn!("icd: a Check-In dropped: too many at once");
        }

        Ok(())
    }
}

/// Add what the ICD Management attributes in `resp`, a chunk of a read, say
/// to `icd`, which stays `None` while no chunk had any.
pub fn read(resp: &ReportDataResp<'_>, icd: &mut Option<IcdInfo>) {
    let reads = resp.icd_management_read_resp();

    let mut seen = icd.is_some();
    let mut info = icd.take().unwrap_or_default();

    first(
        &mut seen,
        &mut info.idle_mode_secs,
        reads.idle_mode_duration(),
    );
    first(
        &mut seen,
        &mut info.active_mode_ms,
        reads.active_mode_duration(),
    );
    first(
        &mut seen,
        &mut info.active_threshold_ms,
        reads.active_mode_threshold(),
    );

    if let Some((_, Ok(mode))) = reads.operating_mode().next() {
        seen = true;
        info.mode = Some(match mode {
            OperatingModeEnum::SIT => IcdMode::Sit,
            OperatingModeEnum::LIT => IcdMode::Lit,
        });
    }
    if let Some((_, Ok(features))) = reads.feature_map().next() {
        seen = true;
        info.long_idle = features & Feature::LONG_IDLE_TIME_SUPPORT.bits() != 0;
        info.check_in = features & Feature::CHECK_IN_PROTOCOL_SUPPORT.bits() != 0;
    }
    if let Some((_, Ok(hint))) = reads.user_active_mode_trigger_hint().next() {
        seen = true;
        info.user_trigger = Some(hint.bits());
    }
    if let Some((_, Ok(instruction))) = reads.user_active_mode_trigger_instruction().next() {
        seen = true;
        info.user_trigger_instruction = instruction.to_string();
    }

    *icd = seen.then_some(info);
}

/// Register `our_node_id` for the Check-Ins of the device, as a permanent client
/// monitoring itself, with `key`; returns the device's Check-In counter.
pub async fn register<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    our_node_id: u64,
    key: &[u8; KEY_LEN],
) -> Result<u32, Error> {
    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let handle = exchange
        .icd_management()
        .register_client(ROOT_ENDPOINT_ID, |req| {
            req.check_in_node_id(our_node_id)?
                .monitored_subject(our_node_id)?
                .key(OctetStr::new(key))?
                .verification_key(None)?
                .client_type(ClientTypeEnum::Permanent)?
                .end()
        })
        .await?;

    let counter = handle.response().and_then(|resp| resp.icd_counter());
    handle.complete().await?;

    counter
}

/// Drop our registration for the Check-Ins of the device.
pub async fn unregister<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    our_node_id: u64,
) -> Result<(), Error> {
    Exchange::initiate(matter, crypto, fab_idx, node_id)
        .await?
        .icd_management()
        .unregister_client(ROOT_ENDPOINT_ID, |req| {
            req.check_in_node_id(our_node_id)?
                .verification_key(None)?
                .end()
        })
        .await
}

/// Which of `clients` sent the Check-In `payload`, if any: its index, and the
/// counter of the Check-In.
pub fn whose<C: Crypto>(crypto: C, clients: &[IcdClient], payload: &[u8]) -> Option<(usize, u32)> {
    clients.iter().enumerate().find_map(|(index, client)| {
        let key = CanonAeadKey::from(client.key);
        let mut payload = payload.to_vec();

        CheckIn::new(key.reference())
            .parse(&crypto, &mut payload)
            .ok()
            .map(|check_in| (index, check_in.counter))
    })
}

/// Take a Check-In of `client` with `counter`: `false` if it is not past the
/// last one taken (a replay), else its offset is noted.
pub fn take(client: &mut IcdClient, counter: u32) -> bool {
    let offset = counter.wrapping_sub(client.start_counter);
    if offset <= client.offset {
        return false;
    }

    client.offset = offset;
    true
}

/// Whether the key of `client` has to be replaced, by registering again.
pub fn needs_refresh(client: &IcdClient) -> bool {
    client.offset >= KEY_REFRESH_OFFSET
}

/// Set `field` from the first report in `iter`, if any.
fn first<T>(
    seen: &mut bool,
    field: &mut Option<T>,
    mut iter: impl Iterator<Item = (EndptId, Result<T, Error>)>,
) {
    if let Some((_, Ok(value))) = iter.next() {
        *seen = true;
        *field = Some(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::num::NonZeroU8;

    use rs_matter::crypto::test_only_crypto;

    use crate::model::DeviceHandle;

    fn client(node_id: u64, key: u8, start_counter: u32) -> IcdClient {
        IcdClient {
            handle: DeviceHandle {
                fabric_index: NonZeroU8::new(1).unwrap(),
                node_id,
            },
            key: [key; KEY_LEN],
            start_counter,
            offset: 0,
        }
    }

    /// A Check-In as `client`'s device sends it, with `counter`.
    fn check_in(client: &IcdClient, counter: u32) -> Vec<u8> {
        let key = CanonAeadKey::from(client.key);
        let mut buf = [0; 64];

        CheckIn::new(key.reference())
            .generate(
                test_only_crypto(),
                counter,
                &5000u16.to_le_bytes(),
                &mut buf,
            )
            .unwrap()
            .to_vec()
    }

    #[test]
    fn a_check_in_is_told_by_its_key() {
        let clients = [client(1, 0x11, 100), client(2, 0x22, 7)];

        let payload = check_in(&clients[1], 8);
        assert_eq!(whose(test_only_crypto(), &clients, &payload), Some((1, 8)));

        let stranger = check_in(&client(3, 0x33, 0), 1);
        assert_eq!(whose(test_only_crypto(), &clients, &stranger), None);
    }

    #[test]
    fn only_a_check_in_past_the_last_one_is_taken() {
        let mut client = client(1, 0x11, 100);

        // The first one goes past the counter the device registered with.
        assert!(!take(&mut client, 100));
        assert!(take(&mut client, 101));
        assert!(!take(&mut client, 101));
        assert!(take(&mut client, 105));
        assert!(!take(&mut client, 103));
        assert_eq!(client.offset, 5);
    }

    #[test]
    fn the_counter_may_wrap() {
        let mut client = client(1, 0x11, u32::MAX - 1);

        assert!(take(&mut client, u32::MAX));
        assert!(take(&mut client, 2));
        assert_eq!(client.offset, 4);
        assert!(!needs_refresh(&client));

        assert!(take(
            &mut client,
            (u32::MAX - 1).wrapping_add(KEY_REFRESH_OFFSET)
        ));
        assert!(needs_refresh(&client));
    }
}
