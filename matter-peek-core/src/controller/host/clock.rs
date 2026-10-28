//! This host's clock and time zone, from the C library: what we serve devices
//! as their time source, and set on them.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use std::time::{SystemTime, UNIX_EPOCH};

use crate::time::{DstPeriod, HostTime, HostZone, MATTER_EPOCH_UNIX_SECS};

/// How far ahead to look for changes of the time zone's offset (DST).
const HORIZON_SECS: i64 = 2 * 366 * 24 * 3600;
/// The step of the search for them: shorter than any DST period.
const STEP_SECS: i64 = 24 * 3600;
/// The most changes to look for: two years of DST.
const MAX_CHANGES: usize = 4;

/// The longest time zone name the spec allows.
const MAX_NAME_LEN: usize = 64;

#[cfg(not(windows))]
extern "C" {
    // POSIX; not in the `libc` crate for every target.
    fn tzset();
}

#[cfg(windows)]
use libc::tzset;

/// This host's clock; `None` if it is before the Matter epoch.
pub(crate) fn now() -> Option<HostTime> {
    let since_unix = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let utc_us = u64::try_from(since_unix.as_micros())
        .ok()?
        .checked_sub(MATTER_EPOCH_UNIX_SECS * 1_000_000)?;

    Some(HostTime {
        utc_us,
        synced: synced(),
    })
}

/// Whether the kernel's clock is synchronized (by an NTP or SNTP daemon).
#[cfg(target_os = "linux")]
fn synced() -> Option<bool> {
    // SAFETY: `timex` is plain data; mode 0 only reads.
    let mut timex: libc::timex = unsafe { core::mem::zeroed() };
    let state = unsafe { libc::adjtimex(&mut timex) };

    (state >= 0).then_some(state != libc::TIME_ERROR)
}

#[cfg(not(target_os = "linux"))]
fn synced() -> Option<bool> {
    None
}

/// This host's time zone at `utc_us` (microseconds since the Matter epoch),
/// and its daylight saving times over the next two years.
pub(crate) fn zone(utc_us: u64) -> Option<HostZone> {
    // SAFETY: re-reads the zone (`TZ`, `/etc/localtime`) if it changed.
    unsafe { tzset() };

    let now = (utc_us / 1_000_000 + MATTER_EPOCH_UNIX_SECS) as i64;
    let now_state = local(now)?;

    // When the offset (or the DST flag) changes next, and to what.
    let mut changes = Vec::new();
    let mut from = now;
    while changes.len() < MAX_CHANGES {
        match next_change(from, now + HORIZON_SECS)? {
            Some(change) => {
                changes.push(change);
                from = change.0;
            }
            None => break,
        }
    }

    // The standard offset: of any time without DST, now or ahead.
    let offset = core::iter::once(now_state)
        .chain(changes.iter().map(|&(_, state)| state))
        .find(|&(_, dst)| !dst)
        .map_or(now_state.0, |(offset, _)| offset);

    // Each stretch from now: its start, state and end (the next change).
    let starts = core::iter::once((now, now_state)).chain(changes.iter().copied());
    let ends = changes.iter().map(|&(at, _)| Some(at)).chain([None]);
    let mut dst = Vec::new();
    for (i, ((start, (stretch_offset, is_dst)), end)) in starts.zip(ends).enumerate() {
        if !is_dst {
            continue;
        }
        // An open end within the horizon is a cut-off, not DST for good.
        if end.is_none() && changes.len() == MAX_CHANGES {
            break;
        }
        // The one in force began before now.
        let start = if i == 0 {
            previous_change(now).unwrap_or(now)
        } else {
            start
        };

        dst.push(DstPeriod {
            offset: stretch_offset - offset,
            start_us: matter_us(start),
            until_us: end.map(matter_us),
        });
    }

    Some(HostZone {
        name: zone_name(),
        offset,
        dst,
    })
}

/// The offset from UTC (seconds) and DST flag at `unix_secs`.
#[cfg(not(windows))]
fn local(unix_secs: i64) -> Option<(i32, bool)> {
    // 32 bits on some targets.
    #[allow(clippy::useless_conversion)]
    let time: libc::time_t = unix_secs.try_into().ok()?;
    // SAFETY: `tm` is plain data, filled in by `localtime_r`.
    let mut tm: libc::tm = unsafe { core::mem::zeroed() };
    let filled = unsafe { libc::localtime_r(&time, &mut tm) };

    (!filled.is_null()).then_some((tm.tm_gmtoff as i32, tm.tm_isdst > 0))
}

/// The offset from UTC (seconds) and DST flag at `unix_secs`.
#[cfg(windows)]
fn local(unix_secs: i64) -> Option<(i32, bool)> {
    // 32 bits on some targets.
    #[allow(clippy::useless_conversion)]
    let time: libc::time_t = unix_secs.try_into().ok()?;
    // SAFETY: both `tm`s are plain data, filled in by `localtime_s` and `gmtime_s`.
    let mut tm: libc::tm = unsafe { core::mem::zeroed() };
    let mut utc: libc::tm = unsafe { core::mem::zeroed() };
    if unsafe { libc::localtime_s(&mut tm, &time) } != 0
        || unsafe { libc::gmtime_s(&mut utc, &time) } != 0
    {
        return None;
    }

    // No `tm_gmtoff`: the two times differ by less than a day.
    let days = if tm.tm_year == utc.tm_year {
        tm.tm_yday - utc.tm_yday
    } else {
        (tm.tm_year - utc.tm_year).signum()
    };
    let offset = ((days * 24 + tm.tm_hour - utc.tm_hour) * 60 + tm.tm_min - utc.tm_min) * 60
        + tm.tm_sec
        - utc.tm_sec;

    Some((offset, tm.tm_isdst > 0))
}

/// The first change after `from`, up to `until`: when, and to what.
#[allow(clippy::type_complexity)]
fn next_change(from: i64, until: i64) -> Option<Option<(i64, (i32, bool))>> {
    let state = local(from)?;

    let mut at = from;
    while at < until {
        let next = (at + STEP_SECS).min(until);
        if local(next)? != state {
            let change = first_differing(at, next, state)?;
            return Some(Some((change, local(change)?)));
        }
        at = next;
    }

    Some(None)
}

/// When the stretch around `now` began, within the year before it.
fn previous_change(now: i64) -> Option<i64> {
    let state = local(now)?;

    let mut at = now;
    while at > now - HORIZON_SECS / 2 {
        let before = at - STEP_SECS;
        if local(before)? != state {
            // The last second of the old state, plus one.
            let (mut old, mut new) = (before, at);
            while new - old > 1 {
                let mid = old + (new - old) / 2;
                if local(mid)? == state {
                    new = mid;
                } else {
                    old = mid;
                }
            }
            return Some(new);
        }
        at = before;
    }

    None
}

/// The first second in `(from, to]` whose state is not `state`, which `to`'s
/// is not.
fn first_differing(from: i64, to: i64, state: (i32, bool)) -> Option<i64> {
    let (mut same, mut differs) = (from, to);
    while differs - same > 1 {
        let mid = same + (differs - same) / 2;
        if local(mid)? == state {
            same = mid;
        } else {
            differs = mid;
        }
    }

    Some(differs)
}

fn matter_us(unix_secs: i64) -> u64 {
    (unix_secs as u64).saturating_sub(MATTER_EPOCH_UNIX_SECS) * 1_000_000
}

/// The IANA name of this host's time zone (`Europe/Sofia`): of `TZ`, else of
/// the zone file `/etc/localtime` links to, else in `/etc/timezone`.
fn zone_name() -> Option<String> {
    let name = std::env::var("TZ")
        .ok()
        .map(|tz| tz.trim_start_matches(':').to_string())
        .filter(|tz| !tz.is_empty() && !tz.starts_with('/'))
        .or_else(|| {
            let link = std::fs::read_link("/etc/localtime").ok()?;
            let (_, name) = link.to_str()?.split_once("zoneinfo/")?;
            Some(name.to_string())
        })
        .or_else(|| {
            std::fs::read_to_string("/etc/timezone")
                .ok()
                .map(|name| name.trim().to_string())
        })?;

    // Not a POSIX rule (`EET-2EEST,M3.5.0/3,M10.5.0/4`): `Area/Location`.
    let iana = name.contains('/')
        && name.len() <= MAX_NAME_LEN
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_-+".contains(c));

    iana.then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-01 12:00 UTC, in microseconds since the Matter epoch.
    const JULY_2026_US: u64 = (1_782_907_200 - MATTER_EPOCH_UNIX_SECS) * 1_000_000;

    /// Under `TZ` (process-wide, so one test sets it).
    fn zone_in(tz: &str) -> HostZone {
        std::env::set_var("TZ", tz);
        zone(JULY_2026_US).unwrap()
    }

    #[test]
    fn zones_and_their_dst() {
        // A POSIX rule, so that no zone files are needed: EET/EEST, as Sofia.
        let sofia = zone_in("EET-2EEST,M3.5.0/3,M10.5.0/4");
        assert_eq!(sofia.name, None);
        assert_eq!(sofia.offset, 7200);
        // In force since 2026-03-29 01:00 UTC, until 2026-10-25 01:00 UTC; then
        // 2027-03-28 01:00 to 2027-10-31 01:00 UTC.
        assert_eq!(
            sofia.dst,
            [
                DstPeriod {
                    offset: 3600,
                    start_us: matter_us(1_774_746_000),
                    until_us: Some(matter_us(1_792_890_000)),
                },
                DstPeriod {
                    offset: 3600,
                    start_us: matter_us(1_806_195_600),
                    until_us: Some(matter_us(1_824_944_400)),
                },
            ]
        );

        let utc = zone_in("UTC0");
        assert_eq!((utc.offset, utc.dst.len()), (0, 0));

        let sao_paulo = zone_in("<-03>3");
        assert_eq!((sao_paulo.offset, sao_paulo.dst.len()), (-10_800, 0));

        std::env::remove_var("TZ");
    }

    #[test]
    fn now_is_after_the_matter_epoch() {
        assert!(now().unwrap().utc_us > JULY_2026_US / 2);
    }
}
