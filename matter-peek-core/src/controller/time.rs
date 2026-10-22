//! Time: our node as the devices' time source (its Time Synchronization
//! cluster, see [`crate::node`]), and the clocks of the devices, which we read
//! and set from this host's clock and time zone.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use core::num::NonZeroU8;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::operational_credentials::{
    self as op_creds, OperationalCredentialsAttrResponses as _,
};
use rs_matter::dm::clusters::decl::time_synchronization::{
    self as time_sync, DSTOffsetStruct, Feature, GranularityEnum, TimeSourceEnum,
    TimeSynchronizationAttrResponses as _, TimeSynchronizationClient as _, TimeZoneStruct,
};
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::im::client::ImClient as _;
use rs_matter::im::{AttrPath, GenericPath};
use rs_matter::tlv::Nullable;
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::api::{
    DeviceTime, DstOffsetEntry, TimeFeatures, TimeGranularity, TimeZoneEntry, TrustedTimeSource,
};
use crate::diagnostics::list_items;

/// The Time Synchronization cluster lives on the root node endpoint.
const ROOT_ENDPOINT: EndptId = 0;

/// The Matter epoch, 2000-01-01, in seconds since the Unix epoch.
pub const MATTER_EPOCH_UNIX_SECS: u64 = 946_684_800;

/// This host's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTime {
    /// UTC, in microseconds since the Matter epoch.
    pub utc_us: u64,
    /// Whether it is synchronized (NTP); `None` if this host cannot tell.
    pub synced: Option<bool>,
}

impl HostTime {
    /// The accuracy and source of our clock: synchronized, as CHIP's hosts
    /// report it; else drifting.
    pub fn quality(&self) -> (GranularityEnum, TimeSourceEnum) {
        if self.synced == Some(false) {
            (GranularityEnum::MinutesGranularity, TimeSourceEnum::Unknown)
        } else {
            (
                GranularityEnum::MillisecondsGranularity,
                TimeSourceEnum::NonMatterSNTP,
            )
        }
    }
}

/// This host's time zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostZone {
    /// Its IANA name (`Europe/Sofia`), if known.
    pub name: Option<String>,
    /// Its standard offset from UTC, in seconds.
    pub offset: i32,
    /// Its daylight saving times, the one in force (if any) first; empty if
    /// it has none.
    pub dst: Vec<DstPeriod>,
}

/// A daylight saving time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DstPeriod {
    /// What it adds to the standard offset, in seconds.
    pub offset: i32,
    /// From when, and until when (`None`: for good), in microseconds since the
    /// Matter epoch.
    pub start_us: u64,
    pub until_us: Option<u64>,
}

/// This host's clock; `None` if it has none.
pub fn host_time() -> Option<HostTime> {
    #[cfg(feature = "std")]
    return crate::host::clock::now();

    #[cfg(not(feature = "std"))]
    None
}

/// This host's time zone at `utc_us` (microseconds since the Matter epoch);
/// `None` if unknown.
pub fn host_zone(utc_us: u64) -> Option<HostZone> {
    #[cfg(feature = "std")]
    return crate::host::clock::zone(utc_us);

    #[cfg(not(feature = "std"))]
    {
        let _ = utc_us;
        None
    }
}

/// Microseconds since the Matter epoch, as milliseconds since the Unix epoch.
pub fn unix_ms(matter_us: u64) -> u64 {
    matter_us / 1000 + MATTER_EPOCH_UNIX_SECS * 1000
}

/// Read the device's clock and time setup, in one round trip. `our_node_id`
/// is ours on its fabric, to tell whether we are its time source.
pub async fn read<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    our_node_id: u64,
) -> Result<DeviceTime, Error> {
    let paths = [
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(time_sync::FULL_CLUSTER.id),
            None,
        )),
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(op_creds::FULL_CLUSTER.id),
            Some(op_creds::AttributeId::CurrentFabricIndex as _),
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
    let ours_at = host_time();

    let mut time = DeviceTime {
        node_id,
        ..Default::default()
    };
    let mut utc_us = None;
    let mut our_fabric = None;
    loop {
        {
            let resp = chunk.response()?;
            let attrs = resp.time_synchronization_read_resp();

            if let Some((_, Ok(features))) = attrs.feature_map().next() {
                time.supported = true;
                time.features = TimeFeatures {
                    time_zone: features & Feature::TIME_ZONE.bits() != 0,
                    ntp_client: features & Feature::NTP_CLIENT.bits() != 0,
                    ntp_server: features & Feature::NTP_SERVER.bits() != 0,
                    time_sync_client: features & Feature::TIME_SYNC_CLIENT.bits() != 0,
                };
            }
            if let Some((_, Ok(utc))) = attrs.utc_time().next() {
                utc_us = utc.into_option();
            }
            if let Some((_, Ok(granularity))) = attrs.granularity().next() {
                time.granularity = granularity_of(granularity);
            }
            if let Some((_, Ok(source))) = attrs.time_source().next() {
                time.time_source = Some(source as u8);
            }
            if let Some((_, Ok(source))) = attrs.trusted_time_source().next() {
                time.trusted_source = source.into_option().and_then(|source| {
                    Some(TrustedTimeSource {
                        fabric_index: source.fabric_index().ok()?,
                        node_id: source.node_id().ok()?,
                        endpoint: source.endpoint().ok()?,
                        ours: false,
                    })
                });
            }
            if let Some((_, Ok(ntp))) = attrs.default_ntp().next() {
                time.default_ntp = ntp.into_option().map(|ntp| ntp.to_string());
            }
            if let Some((_, Ok(local))) = attrs.local_time().next() {
                time.local_unix_ms = local.into_option().map(unix_ms);
            }
            if let Some((_, Ok(max))) = attrs.dst_offset_list_max_size().next() {
                time.max_dst_offsets = Some(max);
            }

            let cluster = time_sync::FULL_CLUSTER.id;
            list_items(
                &resp,
                cluster,
                time_sync::AttributeId::TimeZone as _,
                &mut time.time_zones,
                time_zone,
            );
            list_items(
                &resp,
                cluster,
                time_sync::AttributeId::DSTOffset as _,
                &mut time.dst_offsets,
                dst_offset,
            );

            if let Some((_, Ok(index))) = resp
                .operational_credentials_read_resp()
                .current_fabric_index()
                .next()
            {
                our_fabric = Some(index);
            };
        }

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => break,
        }
    }

    time.utc_unix_ms = utc_us.map(unix_ms);
    time.offset_ms = utc_us
        .zip(ours_at)
        .map(|(theirs, ours)| (theirs as i64 - ours.utc_us as i64) / 1000);
    if let Some(source) = time.trusted_source.as_mut() {
        source.ours = Some(source.fabric_index) == our_fabric && source.node_id == our_node_id;
    }

    Ok(time)
}

/// Set the device's UTC time to `now`.
pub async fn set_utc<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    now: &HostTime,
) -> Result<(), Error> {
    let (granularity, source) = now.quality();

    Exchange::initiate(matter, crypto, fab_idx, node_id)
        .await?
        .time_synchronization()
        .set_utc_time(ROOT_ENDPOINT, |req| {
            req.utc_time(now.utc_us)?
                .granularity(granularity)?
                .time_source(Some(source))?
                .end()
        })
        .await
}

/// Set the device's time zone to `zone` (its standard offset: daylight saving
/// time is separate). Answers whether the device needs its DST offsets set.
pub async fn set_zone<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    zone: &HostZone,
) -> Result<bool, Error> {
    let handle = Exchange::initiate(matter, crypto, fab_idx, node_id)
        .await?
        .time_synchronization()
        .set_time_zone(ROOT_ENDPOINT, |req| {
            req.time_zone()?
                .push()?
                .offset(zone.offset)?
                .valid_at(0)?
                .name(zone.name.as_deref())?
                .end()?
                .end()?
                .end()
        })
        .await?;

    let required = handle.response()?.dst_offset_required()?;
    handle.complete().await?;

    Ok(required)
}

/// Set the device's daylight saving times: the first `max` of `dst`; a single
/// one of no offset, for good, if the zone has none.
pub async fn set_dst<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    dst: &[DstPeriod],
    max: usize,
) -> Result<(), Error> {
    const NONE: [DstPeriod; 1] = [DstPeriod {
        offset: 0,
        start_us: 0,
        until_us: None,
    }];

    let dst = if dst.is_empty() { &NONE[..] } else { dst };

    Exchange::initiate(matter, crypto, fab_idx, node_id)
        .await?
        .time_synchronization()
        .set_dst_offset(ROOT_ENDPOINT, |req| {
            let mut list = req.dst_offset()?;
            for period in dst.iter().take(max.max(1)) {
                list = list
                    .push()?
                    .offset(period.offset)?
                    .valid_starting(period.start_us)?
                    .valid_until(Nullable::new(period.until_us))?
                    .end()?;
            }
            list.end()?.end()
        })
        .await
}

/// Make node `source` (endpoint 0), on the device's fabric of ours, its
/// trusted time source.
pub async fn set_trusted_source<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    source: u64,
) -> Result<(), Error> {
    Exchange::initiate(matter, crypto, fab_idx, node_id)
        .await?
        .time_synchronization()
        .set_trusted_time_source(ROOT_ENDPOINT, |req| {
            req.trusted_time_source()?
                .non_null()?
                .node_id(source)?
                .endpoint(ROOT_ENDPOINT)?
                .end()?
                .end()
        })
        .await
}

fn granularity_of(granularity: GranularityEnum) -> TimeGranularity {
    match granularity {
        GranularityEnum::NoTimeGranularity => TimeGranularity::NoTime,
        GranularityEnum::MinutesGranularity => TimeGranularity::Minutes,
        GranularityEnum::SecondsGranularity => TimeGranularity::Seconds,
        GranularityEnum::MillisecondsGranularity => TimeGranularity::Milliseconds,
        GranularityEnum::MicrosecondsGranularity => TimeGranularity::Microseconds,
    }
}

fn time_zone(item: &TimeZoneStruct<'_>) -> Option<TimeZoneEntry> {
    let valid_at = item.valid_at().ok()?;

    Some(TimeZoneEntry {
        offset_secs: item.offset().ok()?,
        valid_at_unix_ms: (valid_at > 0).then(|| unix_ms(valid_at)),
        name: item.name().ok().flatten().map(|name| name.to_string()),
    })
}

fn dst_offset(item: &DSTOffsetStruct<'_>) -> Option<DstOffsetEntry> {
    let from = item.valid_starting().ok()?;

    Some(DstOffsetEntry {
        offset_secs: item.offset().ok()?,
        from_unix_ms: (from > 0).then(|| unix_ms(from)),
        until_unix_ms: item.valid_until().ok()?.into_option().map(unix_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_ms_from_matter_us() {
        assert_eq!(unix_ms(0), 946_684_800_000);
        assert_eq!(unix_ms(1_500_000), 946_684_801_500);
    }

    #[test]
    fn our_quality_follows_sync() {
        let at = |synced| HostTime { utc_us: 1, synced };

        assert_eq!(
            at(Some(true)).quality(),
            (
                GranularityEnum::MillisecondsGranularity,
                TimeSourceEnum::NonMatterSNTP
            )
        );
        assert_eq!(at(None).quality(), at(Some(true)).quality());
        assert_eq!(
            at(Some(false)).quality(),
            (GranularityEnum::MinutesGranularity, TimeSourceEnum::Unknown)
        );
    }
}
