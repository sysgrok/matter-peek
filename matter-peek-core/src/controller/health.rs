//! A device's health, judged from what it reports: signs of crashes (why it
//! last started, its faults, how often it restarts), its heap, the stacks of
//! its tasks, and its link to its network. The thresholds are rules of thumb.
//!
//! Its crash log is not read here: a device may delete it once delivered.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::api::display::bytes;
use crate::api::{
    BootReason, DeviceHealth, HealthArea, HealthAreaKind, HealthCheck, HealthFinding,
    HealthVerdict, NetworkInfo, NetworkKind, ThreadMetric, ThreadNeighbor, ThreadRole,
};
use crate::mesh::quality;

/// From how many restarts a device that restarts more than once per hour of
/// operation may restart over and over: a device being developed restarts
/// often, but not that often.
const RESTARTS_MIN: u16 = 10;

/// Peak heap use, in percent of the heap, from which little is left, and
/// from which it may run out.
const HEAP_FAIR_PERCENT: u64 = 80;
const HEAP_POOR_PERCENT: u64 = 90;

/// Peak stack use, in percent of the stack, from which a task is short of
/// stack, and from which it may overflow it; as the Memory popup marks them.
const STACK_FAIR_PERCENT: u64 = 75;
const STACK_POOR_PERCENT: u64 = 90;

/// The least free stack a task should keep is its heap over this: a device
/// with more memory runs deeper code (larger frames, more libraries)...
const STACK_FLOOR_DIVISOR: u64 = 512;

/// ... at least this, which is also the floor where the heap is unknown ...
const STACK_FLOOR_MIN: u64 = 512;

/// ... and at most this.
const STACK_FLOOR_MAX: u64 = 64 * 1024;

/// How many tasks short of stack to name; the rest are counted.
const STACK_FINDINGS_MAX: usize = 3;

/// Wi-Fi signal strengths up to which the link is weak, and very weak, dBm.
const WIFI_FAIR_RSSI: i8 = -70;
const WIFI_POOR_RSSI: i8 = -80;

/// Beacons missed, in percent of those sent, from which the link drops out.
const BEACONS_FAIR_PERCENT: u64 = 5;
const BEACONS_POOR_PERCENT: u64 = 20;

/// From how many beacons the share missed tells.
const BEACONS_MIN: u64 = 100;

/// Thread signal strengths up to which a link is weak, and very weak, dBm.
const THREAD_FAIR_RSSI: i8 = -85;
const THREAD_POOR_RSSI: i8 = -92;

/// The Thread link quality (0 to 3) up to which a link is weak, and the one
/// of a very weak link.
const THREAD_FAIR_QUALITY: u8 = 1;
const THREAD_POOR_QUALITY: u8 = 0;

/// Frames to a neighbour that fail, in percent, from which a link is lossy,
/// and very lossy.
const FRAME_ERRORS_FAIR: u8 = 10;
const FRAME_ERRORS_POOR: u8 = 30;

/// From how many parent changes, or detaches, since it started a Thread
/// device's links are unstable.
const THREAD_CHURN: u16 = 3;

/// What a check judges, as read from the device (see
/// [`read_check`](crate::diagnostics::read_check)).
#[derive(Debug, Default)]
pub(crate) struct HealthRead {
    /// Its General and Software Diagnostics.
    pub health: DeviceHealth,
    /// Its interfaces, and the diagnostics of its networks.
    pub network: NetworkInfo,
    /// The software faults it reported since it started, by name.
    pub software_faults: Vec<String>,
}

/// An area, as it is judged.
struct Area {
    kind: HealthAreaKind,
    /// Whether the device reports anything to judge it by.
    known: bool,
    summary: Vec<String>,
    findings: Vec<HealthFinding>,
}

impl Area {
    fn new(kind: HealthAreaKind) -> Self {
        Self {
            kind,
            known: false,
            summary: Vec::new(),
            findings: Vec::new(),
        }
    }

    fn fair(&mut self, text: String) {
        self.add(HealthVerdict::Fair, text);
    }

    fn poor(&mut self, text: String) {
        self.add(HealthVerdict::Poor, text);
    }

    fn add(&mut self, verdict: HealthVerdict, text: String) {
        self.known = true;
        self.findings.push(HealthFinding { verdict, text });
    }

    fn done(mut self) -> HealthArea {
        self.findings
            .sort_by_key(|finding| core::cmp::Reverse(finding.verdict));

        let verdict = match self.findings.first() {
            Some(worst) => worst.verdict,
            None if self.known => HealthVerdict::Good,
            None => HealthVerdict::Unknown,
        };
        let summary = if self.summary.is_empty() {
            String::from("not reported")
        } else {
            self.summary.join(", ")
        };

        HealthArea {
            kind: self.kind,
            verdict,
            summary,
            findings: self.findings,
        }
    }
}

/// Judge the health of a device from `read`, checked at `checked_ms`.
pub(crate) fn judge(read: &HealthRead, checked_ms: Option<u64>) -> HealthCheck {
    let mut crash_signs = false;
    let areas = alloc::vec![
        crashes(read, &mut crash_signs),
        heap(read),
        stacks(read),
        link(read),
    ];
    let verdict = areas
        .iter()
        .map(|area| area.verdict)
        .max()
        .unwrap_or(HealthVerdict::Unknown);

    HealthCheck {
        verdict,
        areas,
        crash_signs,
        checked_ms,
    }
}

/// Why it last started, its faults, how often it restarts, and the software
/// faults it reported: `crash_signs` is set where they point to a crash.
fn crashes(read: &HealthRead, crash_signs: &mut bool) -> HealthArea {
    let mut area = Area::new(HealthAreaKind::Crashes);
    let general = read.health.general.unwrap_or_default();

    if let Some(reason) = general.boot_reason {
        area.known = true;
        area.summary.push(format!("last start: {}", reason.label()));

        match reason {
            BootReason::SoftwareWatchdogReset | BootReason::HardwareWatchdogReset => {
                *crash_signs = true;
                area.poor(format!(
                    "It last restarted from its {}: its software hung.",
                    reason.label()
                ));
            }
            BootReason::BrownOutReset => area.poor(String::from(
                "It last restarted after a brown-out: its supply voltage dropped too low.",
            )),
            BootReason::SoftwareReset => {
                *crash_signs = true;
                area.fair(String::from(
                    "Its software last restarted it: on purpose, or after a crash.",
                ));
            }
            _ => {}
        }
    }

    if let Some(restarts) = general.reboot_count {
        area.known = true;

        match general.total_operational_hours {
            Some(hours) => {
                area.summary
                    .push(format!("{restarts} restarts in {hours} h of operation"));

                if restarts >= RESTARTS_MIN && u32::from(restarts) > hours {
                    *crash_signs = true;
                    area.poor(format!(
                        "It restarted {restarts} times in {hours} hours of operation: it may \
                         restart over and over."
                    ));
                }
            }
            None => area.summary.push(format!("{restarts} restarts")),
        }
    }

    if let Some(faults) = &read.health.faults {
        area.known = true;

        if let Some(list) = faults.hardware.as_ref().filter(|list| !list.is_empty()) {
            let names: Vec<&str> = list.iter().map(|fault| fault.label()).collect();
            area.poor(format!("It reports hardware faults: {}.", names.join(", ")));
        }
        if let Some(list) = faults.radio.as_ref().filter(|list| !list.is_empty()) {
            let names: Vec<&str> = list.iter().map(|fault| fault.label()).collect();
            area.poor(format!("It reports radio faults: {}.", names.join(", ")));
        }
        if let Some(list) = faults.network.as_ref().filter(|list| !list.is_empty()) {
            let names: Vec<&str> = list.iter().map(|fault| fault.label()).collect();
            area.fair(format!("It reports network faults: {}.", names.join(", ")));
        }
    }

    if let Some(last) = read.software_faults.last() {
        *crash_signs = true;
        let count = read.software_faults.len();
        area.fair(if count == 1 {
            format!("It reported a software fault since it started: {last}.")
        } else {
            format!("It reported {count} software faults since it started; the last: {last}.")
        });
    }

    area.done()
}

/// Its heap's peak use.
fn heap(read: &HealthRead) -> HealthArea {
    let mut area = Area::new(HealthAreaKind::Heap);
    let Some(software) = read.health.software.as_ref() else {
        return area.done();
    };

    match heap_total(&read.health) {
        Some(total) => {
            let used = software.current_heap_used.unwrap_or_default();
            let peak = software
                .current_heap_high_watermark
                .unwrap_or_default()
                .max(used);
            let percent = peak * 100 / total;

            area.known = true;
            area.summary
                .push(format!("peak {percent}% of {}", bytes(total)));
            if let Some(free) = software.current_heap_free {
                area.summary.push(format!("{} free now", bytes(free)));
            }

            if percent >= HEAP_POOR_PERCENT {
                area.poor(format!(
                    "Its heap was {percent}% used at its peak: it may run out."
                ));
            } else if percent >= HEAP_FAIR_PERCENT {
                area.fair(format!(
                    "Its heap was {percent}% used at its peak: little is left."
                ));
            }
        }
        // Without its size, free memory alone does not tell.
        None => {
            if let Some(free) = software.current_heap_free {
                area.summary.push(format!("{} free now", bytes(free)));
            }
        }
    }

    area.done()
}

/// Its heap's size: what is free and what is used.
fn heap_total(health: &DeviceHealth) -> Option<u64> {
    let software = health.software.as_ref()?;

    software
        .current_heap_free
        .zip(software.current_heap_used)
        .map(|(free, used)| free + used)
        .filter(|total| *total > 0)
}

/// How close its tasks came to the end of their stacks: by the share of a
/// stack used at its peak, and by the bytes never used, which must not fall
/// below a floor that grows with the device's heap.
fn stacks(read: &HealthRead) -> HealthArea {
    let mut area = Area::new(HealthAreaKind::Stacks);
    let threads = read
        .health
        .software
        .as_ref()
        .and_then(|software| software.thread_metrics.as_deref())
        .unwrap_or_default();

    let mut judged: Vec<(&ThreadMetric, HealthVerdict)> = Vec::new();
    let floor = stack_floor(heap_total(&read.health));
    for thread in threads {
        let Some(least) = thread.stack_free_minimum else {
            continue;
        };
        let least = u64::from(least);
        let percent = stack_percent(thread);

        let verdict = if least < floor / 2 || percent.is_some_and(|p| p >= STACK_POOR_PERCENT) {
            HealthVerdict::Poor
        } else if least < floor || percent.is_some_and(|p| p >= STACK_FAIR_PERCENT) {
            HealthVerdict::Fair
        } else {
            HealthVerdict::Good
        };
        judged.push((thread, verdict));
    }

    if judged.is_empty() {
        if !threads.is_empty() {
            area.summary
                .push(format!("{} tasks, without stack figures", threads.len()));
        }
        return area.done();
    }

    area.known = true;
    judged.sort_by_key(|(thread, _)| thread.spare_rank());

    let (tightest, _) = judged[0];
    area.summary.push(format!(
        "{} tasks; the tightest: {}, {} free at least (floor {})",
        threads.len(),
        task_name(tightest),
        bytes(tightest.stack_free_minimum.unwrap_or_default().into()),
        bytes(floor)
    ));

    let short: Vec<&(&ThreadMetric, HealthVerdict)> = judged
        .iter()
        .filter(|(_, verdict)| *verdict > HealthVerdict::Good)
        .collect();
    for (thread, verdict) in short.iter().take(STACK_FINDINGS_MAX) {
        let mut text = format!(
            "{} came within {} of the end of its stack",
            task_name(thread),
            bytes(thread.stack_free_minimum.unwrap_or_default().into())
        );
        if let (Some(percent), Some(size)) = (stack_percent(thread), thread.stack_size) {
            text += &format!(" ({percent}% of {} used at its peak)", bytes(size.into()));
        }
        text.push('.');
        area.add(*verdict, text);
    }

    let rest = &short[short.len().min(STACK_FINDINGS_MAX)..];
    if let Some(worst) = rest.iter().map(|(_, verdict)| *verdict).max() {
        area.add(
            worst,
            match rest.len() {
                1 => String::from("1 more task is short of stack."),
                n => format!("{n} more tasks are short of stack."),
            },
        );
    }

    area.done()
}

/// The least free stack a task of a device with a heap of `heap` should keep.
fn stack_floor(heap: Option<u64>) -> u64 {
    heap.map_or(STACK_FLOOR_MIN, |heap| {
        (heap / STACK_FLOOR_DIVISOR).clamp(STACK_FLOOR_MIN, STACK_FLOOR_MAX)
    })
}

/// The share of its stack a task used at its peak, in percent.
fn stack_percent(thread: &ThreadMetric) -> Option<u64> {
    let size = thread.stack_size.filter(|size| *size > 0)?;

    Some(u64::from(thread.stack_peak()?) * 100 / u64::from(size))
}

fn task_name(thread: &ThreadMetric) -> String {
    thread
        .name
        .clone()
        .unwrap_or_else(|| format!("Task {}", thread.id))
}

/// Its link to each network it is on.
fn link(read: &HealthRead) -> HealthArea {
    let mut area = Area::new(HealthAreaKind::Link);
    let network = &read.network;

    // The diagnostics of a network it is not on now tell nothing.
    let on = |kind: NetworkKind| {
        let mut ifaces = network.interfaces.iter().filter(|iface| iface.kind == kind);
        let listed = ifaces.clone().next().is_some();
        !listed || ifaces.any(|iface| iface.operational)
    };

    if let Some(wifi) = network.wifi.as_ref().filter(|_| on(NetworkKind::WiFi)) {
        area.known = true;
        let mut summary = String::from("Wi-Fi");

        if let Some(rssi) = wifi.rssi {
            summary += &format!(" at {rssi} dBm");

            if rssi <= WIFI_POOR_RSSI {
                area.poor(format!("Its Wi-Fi signal is very weak: {rssi} dBm."));
            } else if rssi <= WIFI_FAIR_RSSI {
                area.fair(format!("Its Wi-Fi signal is weak: {rssi} dBm."));
            }
        }
        if let Some(channel) = wifi.channel {
            summary += &format!(", channel {channel}");
        }
        area.summary.push(summary);

        if let (Some(lost), Some(received)) = (wifi.beacon_lost_count, wifi.beacon_rx_count) {
            let (lost, sent) = (u64::from(lost), u64::from(lost) + u64::from(received));
            let percent = if sent >= BEACONS_MIN {
                lost * 100 / sent
            } else {
                0
            };

            if percent >= BEACONS_POOR_PERCENT {
                area.poor(format!(
                    "It missed {percent}% of its access point's beacons: its link drops out."
                ));
            } else if percent >= BEACONS_FAIR_PERCENT {
                area.fair(format!(
                    "It missed {percent}% of its access point's beacons."
                ));
            }
        }
    }

    if let Some(thread) = network.thread.as_ref().filter(|_| on(NetworkKind::Thread)) {
        area.known = true;
        let role = thread.role.unwrap_or(ThreadRole::Unspecified);
        let mut summary = format!("Thread {}", role.label().to_lowercase());

        match role {
            ThreadRole::Unassigned => {
                area.poor(String::from("It is detached from its Thread network."));
            }
            ThreadRole::Router | ThreadRole::Leader => {
                let best = thread
                    .neighbors
                    .iter()
                    .filter(|neighbor| !neighbor.is_child)
                    .max_by_key(|neighbor| link_rank(neighbor));
                if let Some(best) = best {
                    summary += &format!(", best link to a router {}", signal(best));
                    judge_link(&mut area, best, "Even its best link to another router");
                }
            }
            ThreadRole::EndDevice | ThreadRole::SleepyEndDevice | ThreadRole::Reed => {
                let parent = thread.neighbors.iter().find(|neighbor| !neighbor.is_child);
                if let Some(parent) = parent {
                    summary += &format!(", parent {}", signal(parent));
                    judge_link(&mut area, parent, "Its link to its parent");
                }
            }
            ThreadRole::Unspecified => {}
        }
        area.summary.push(summary);

        if let Some(changes) = thread
            .parent_change_count
            .filter(|count| *count >= THREAD_CHURN)
        {
            area.fair(format!(
                "It changed its parent {changes} times since it started: its links are \
                 unstable."
            ));
        }
        if let Some(detaches) = thread
            .detached_role_count
            .filter(|count| *count >= THREAD_CHURN)
        {
            area.fair(format!(
                "It lost the Thread network {detaches} times since it started."
            ));
        }
    }

    if let Some(ethernet) = network
        .ethernet
        .as_ref()
        .filter(|_| on(NetworkKind::Ethernet))
    {
        area.known = true;
        area.summary.push(match ethernet.phy_rate_mbps {
            Some(rate) => format!("Ethernet at {rate} Mbit/s"),
            None => String::from("Ethernet"),
        });
    }

    area.done()
}

/// How a Thread link ranks, the best the highest: by its quality, then its
/// signal.
fn link_rank(neighbor: &ThreadNeighbor) -> (u8, i8) {
    (
        quality(neighbor.lqi),
        neighbor_rssi(neighbor).unwrap_or(i8::MIN),
    )
}

fn neighbor_rssi(neighbor: &ThreadNeighbor) -> Option<i8> {
    neighbor.average_rssi.or(neighbor.last_rssi)
}

/// Judge the link to `neighbor`: `what` it is, as a sentence starts.
fn judge_link(area: &mut Area, neighbor: &ThreadNeighbor, what: &str) {
    let quality = quality(neighbor.lqi);
    let rssi = neighbor_rssi(neighbor);

    if quality == THREAD_POOR_QUALITY || rssi.is_some_and(|rssi| rssi <= THREAD_POOR_RSSI) {
        area.poor(format!("{what} is very weak: {}.", signal(neighbor)));
    } else if quality <= THREAD_FAIR_QUALITY || rssi.is_some_and(|rssi| rssi <= THREAD_FAIR_RSSI) {
        area.fair(format!("{what} is weak: {}.", signal(neighbor)));
    }

    let errors = neighbor.frame_error_rate;
    if errors >= FRAME_ERRORS_POOR {
        area.poor(format!("{what} loses {errors}% of its frames."));
    } else if errors >= FRAME_ERRORS_FAIR {
        area.fair(format!("{what} loses {errors}% of its frames."));
    }
}

/// `quality 3/3, -44 dBm`.
fn signal(neighbor: &ThreadNeighbor) -> String {
    let quality = quality(neighbor.lqi);

    match neighbor_rssi(neighbor) {
        Some(rssi) => format!("quality {quality}/3, {rssi} dBm"),
        None => format!("quality {quality}/3"),
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec;

    use crate::api::{
        ActiveFaults, GeneralDiag, HardwareFault, NetworkInterfaceInfo, SoftwareDiag, ThreadDiag,
        WifiDiag,
    };

    use super::*;

    /// ClimaNode on its bench, as it reported itself on 2026-10-28: healthy.
    fn climanode() -> HealthRead {
        let task = |name: &str, least: u32| ThreadMetric {
            id: 0,
            name: Some(name.to_string()),
            stack_size: None,
            stack_free_current: None,
            stack_free_minimum: Some(least),
        };

        HealthRead {
            health: DeviceHealth {
                general: Some(GeneralDiag {
                    up_time_secs: Some(19_406),
                    reboot_count: Some(15),
                    total_operational_hours: Some(34),
                    boot_reason: Some(BootReason::PowerOnReboot),
                }),
                faults: Some(ActiveFaults::default()),
                software: Some(SoftwareDiag {
                    current_heap_free: Some(186_932),
                    current_heap_used: Some(109_136),
                    current_heap_high_watermark: Some(113_952),
                    thread_metrics: Some(vec![
                        task("pthread", 1160),
                        task("main", 2172),
                        task("ThreadDr", 9064),
                    ]),
                }),
                ..Default::default()
            },
            network: NetworkInfo {
                thread: Some(ThreadDiag {
                    role: Some(ThreadRole::Router),
                    detached_role_count: Some(1),
                    parent_change_count: Some(0),
                    neighbors: vec![neighbor(3, -44, false)],
                    ..Default::default()
                }),
                ..Default::default()
            },
            software_faults: Vec::new(),
        }
    }

    fn neighbor(lqi: u8, rssi: i8, is_child: bool) -> ThreadNeighbor {
        ThreadNeighbor {
            ext_address: 1,
            rloc16: 0x4000,
            age_secs: 7,
            lqi,
            average_rssi: Some(rssi),
            last_rssi: Some(rssi),
            frame_error_rate: 0,
            message_error_rate: 0,
            is_child,
            rx_on_when_idle: true,
            full_thread_device: true,
        }
    }

    fn area(check: &HealthCheck, kind: HealthAreaKind) -> &HealthArea {
        check.areas.iter().find(|area| area.kind == kind).unwrap()
    }

    #[test]
    fn a_healthy_device_is_good() {
        let check = judge(&climanode(), Some(1));

        assert_eq!(check.verdict, HealthVerdict::Good, "{check:#?}");
        assert!(!check.crash_signs);
        assert_eq!(
            area(&check, HealthAreaKind::Heap).summary,
            "peak 38% of 289.1KB, 182.5KB free now"
        );
        assert_eq!(
            area(&check, HealthAreaKind::Stacks).summary,
            "3 tasks; the tightest: pthread, 1.1KB free at least (floor 578B)"
        );
        assert_eq!(
            area(&check, HealthAreaKind::Link).summary,
            "Thread router, best link to a router quality 3/3, -44 dBm"
        );
    }

    #[test]
    fn nothing_reported_is_unknown() {
        let check = judge(&HealthRead::default(), None);

        assert_eq!(check.verdict, HealthVerdict::Unknown);
        assert!(check
            .areas
            .iter()
            .all(|area| area.verdict == HealthVerdict::Unknown && area.summary == "not reported"));
    }

    #[test]
    fn crash_signs() {
        let mut read = climanode();
        let general = read.health.general.as_mut().unwrap();
        general.boot_reason = Some(BootReason::SoftwareWatchdogReset);
        general.reboot_count = Some(40);
        general.total_operational_hours = Some(12);
        read.health.faults.as_mut().unwrap().hardware = Some(vec![HardwareFault::Sensor]);
        read.software_faults = vec!["main".to_string()];

        let check = judge(&read, None);
        let crashes = area(&check, HealthAreaKind::Crashes);

        assert!(check.crash_signs);
        assert_eq!(crashes.verdict, HealthVerdict::Poor);
        assert_eq!(crashes.findings.len(), 4, "{crashes:#?}");
        // The worst first.
        assert_eq!(crashes.findings[3].verdict, HealthVerdict::Fair);
        assert_eq!(check.verdict, HealthVerdict::Poor);
    }

    #[test]
    fn a_development_device_restarting_often_is_no_crash_loop() {
        // ClimaNode: 15 restarts in 34 hours, flashed often.
        let check = judge(&climanode(), None);

        assert!(area(&check, HealthAreaKind::Crashes).findings.is_empty());
    }

    #[test]
    fn heap_peak() {
        let mut read = climanode();
        let software = read.health.software.as_mut().unwrap();
        software.current_heap_high_watermark = Some(270_000);

        let check = judge(&read, None);
        assert_eq!(
            area(&check, HealthAreaKind::Heap).verdict,
            HealthVerdict::Poor
        );

        let software = read.health.software.as_mut().unwrap();
        software.current_heap_high_watermark = Some(240_000);
        let check = judge(&read, None);
        assert_eq!(
            area(&check, HealthAreaKind::Heap).verdict,
            HealthVerdict::Fair
        );
    }

    #[test]
    fn the_stack_floor_grows_with_the_heap() {
        assert_eq!(stack_floor(None), 512);
        assert_eq!(stack_floor(Some(64 * 1024)), 512);
        assert_eq!(stack_floor(Some(296_068)), 578);
        assert_eq!(stack_floor(Some(1 << 30)), 64 * 1024);

        // 3 KB to spare: fine on a small device, not on one with 64 MB.
        let task = ThreadMetric {
            id: 1,
            name: Some("worker".to_string()),
            stack_size: None,
            stack_free_current: None,
            stack_free_minimum: Some(3 * 1024),
        };
        let mut read = climanode();
        read.health.software.as_mut().unwrap().thread_metrics = Some(vec![task]);
        assert_eq!(
            area(&judge(&read, None), HealthAreaKind::Stacks).verdict,
            HealthVerdict::Good
        );

        let software = read.health.software.as_mut().unwrap();
        software.current_heap_free = Some(32 << 20);
        software.current_heap_used = Some(32 << 20);
        let stacks = judge(&read, None);
        let stacks = area(&stacks, HealthAreaKind::Stacks);
        assert_eq!(stacks.verdict, HealthVerdict::Poor);
        assert_eq!(
            stacks.findings[0].text,
            "worker came within 3.0KB of the end of its stack."
        );
    }

    #[test]
    fn stacks_by_their_share_used() {
        let task = |id, least| ThreadMetric {
            id,
            name: None,
            stack_size: Some(8192),
            stack_free_current: None,
            stack_free_minimum: Some(least),
        };
        let mut read = climanode();
        read.health.software.as_mut().unwrap().thread_metrics = Some(vec![
            task(1, 4096),
            task(2, 1500),
            task(3, 700),
            task(4, 1800),
            task(5, 1900),
        ]);

        let check = judge(&read, None);
        let stacks = area(&check, HealthAreaKind::Stacks);

        assert_eq!(stacks.verdict, HealthVerdict::Poor);
        assert_eq!(
            stacks.findings[0].text,
            "Task 3 came within 700B of the end of its stack (91% of 8.0KB used at its peak)."
        );
        // Three named, the fourth counted.
        assert_eq!(stacks.findings.len(), 4, "{stacks:#?}");
        assert_eq!(stacks.findings[3].text, "1 more task is short of stack.");
    }

    #[test]
    fn weak_wifi() {
        let mut read = climanode();
        read.network = NetworkInfo {
            interfaces: vec![NetworkInterfaceInfo {
                name: "wlan0".to_string(),
                kind: NetworkKind::WiFi,
                operational: true,
                hardware_address: String::new(),
                ipv4_addresses: Vec::new(),
                ipv6_addresses: Vec::new(),
                off_premise_ipv4: None,
                off_premise_ipv6: None,
            }],
            wifi: Some(WifiDiag {
                rssi: Some(-74),
                channel: Some(6),
                beacon_lost_count: Some(30),
                beacon_rx_count: Some(470),
                ..Default::default()
            }),
            // Not on Thread now: not judged.
            thread: Some(ThreadDiag {
                role: Some(ThreadRole::Unassigned),
                ..Default::default()
            }),
            ..Default::default()
        };
        read.network.interfaces.push(NetworkInterfaceInfo {
            name: "ot0".to_string(),
            kind: NetworkKind::Thread,
            operational: false,
            ..read.network.interfaces[0].clone()
        });

        let check = judge(&read, None);
        let link = area(&check, HealthAreaKind::Link);

        assert_eq!(link.verdict, HealthVerdict::Fair);
        assert_eq!(link.summary, "Wi-Fi at -74 dBm, channel 6");
        assert_eq!(link.findings.len(), 2, "{link:#?}");
    }

    #[test]
    fn a_weak_thread_parent() {
        let mut read = climanode();
        read.network.thread = Some(ThreadDiag {
            role: Some(ThreadRole::SleepyEndDevice),
            parent_change_count: Some(4),
            neighbors: vec![ThreadNeighbor {
                frame_error_rate: 35,
                ..neighbor(1, -88, false)
            }],
            ..Default::default()
        });

        let check = judge(&read, None);
        let link = area(&check, HealthAreaKind::Link);

        assert_eq!(link.verdict, HealthVerdict::Poor);
        assert_eq!(
            link.summary,
            "Thread sleepy end device, parent quality 1/3, -88 dBm"
        );
        let texts: Vec<&str> = link.findings.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Its link to its parent loses 35% of its frames.",
                "Its link to its parent is weak: quality 1/3, -88 dBm.",
                "It changed its parent 4 times since it started: its links are unstable.",
            ]
        );
    }
}
