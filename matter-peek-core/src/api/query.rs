//! Device filters: words and `key:value` terms over a device's tags and what
//! is known of it, e.g. `kitchen net:thread -eco:google uptime:<1h`.
//!
//! - A word matches the start of a word of anything known of the device: its
//!   tags, its names, and the values of every facet. All terms must match.
//! - `key:value` matches one of the [`FACETS`]; `key:a,b` either value;
//!   `-term` negates a term; quotes keep spaces (`tag:"ground floor"`).
//! - Facets with numbers also take `<`, `<=`, `>`, `>=` and `=`
//!   (`reboots:>5`), in their units (`uptime:<1h`, `heap:<16k`, `spec:>=1.4`).
//!
//! Values match as [`slug`]s, so case and punctuation do not matter. A term
//! about a fact not known (a live one, without the device's health) does not
//! match.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use super::display;
use super::types::*;

/// What a facet's numbers are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// A count, an ID or a version number: `5`, `0x1B66B`.
    Plain,
    /// Seconds: `90`, `30m`, `2h`, `1d`, `1w`.
    Duration,
    /// Bytes: `512`, `16k`, `1m`.
    Bytes,
    /// A Matter version: `1.4`, `1.4.2`.
    Version,
}

/// A facet: what `key:` matches.
#[derive(Debug)]
pub struct Facet {
    pub key: &'static str,
    /// What it matches.
    pub help: &'static str,
    /// Its numbers, if it has any.
    pub unit: Option<Unit>,
    /// Comparisons to offer as examples.
    pub examples: &'static [&'static str],
}

/// What a filter cannot take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// `key:` is no facet.
    UnknownFacet(String),
    /// `is:state` is no state.
    UnknownState(String),
    /// A comparison on a facet without numbers.
    NotNumeric(&'static str),
    /// A number that does not read in its facet's unit.
    BadNumber { key: &'static str, value: String },
}

/// A filter of devices: see the [module](self).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    terms: Vec<Term>,
}

/// What is known of a device beyond its record, for [`Facts::of`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Known<'a> {
    /// Whether it answers now, when known better than its record says.
    pub online: Option<bool>,
    /// Its health, as last reported.
    pub health: Option<&'a DeviceHealth>,
    /// Its update, while going on or lately ended.
    pub ota: Option<&'a OtaProgress>,
    /// The OTA images there are.
    pub images: &'a [OtaImageInfo],
}

/// What filters match a device on: its facts, each a facet and a value.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    facts: Vec<Fact>,
}

/// A completion of a filter's last term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The filter, completed.
    pub text: String,
    /// The term it completes to.
    pub label: String,
    /// What it is: a facet's help, or how many devices have it.
    pub help: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    negated: bool,
    test: Test,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Test {
    /// Plain words, as a slug.
    Words(String),
    /// A facet, with any of `values`.
    Facet {
        key: &'static str,
        values: Vec<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    /// A slug, and the number it reads as, if any.
    Text(String, Option<u64>),
    Compare(Cmp, u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

#[derive(Debug, Clone)]
struct Fact {
    key: &'static str,
    /// A slug.
    text: String,
    number: Option<u64>,
}

/// The facets, in the order a list of them shows.
pub static FACETS: &[Facet] = &[
    facet("tag", "one of your tags", None, &[]),
    facet(
        "is",
        "a state: online, offline, shared, updatable, …",
        None,
        &[],
    ),
    facet(
        "net",
        "its network: wifi, thread, ethernet, cellular",
        None,
        &[],
    ),
    facet("ssid", "the Wi-Fi network it is on", None, &[]),
    facet("thread", "the Thread network it is on", None, &[]),
    facet(
        "role",
        "its Thread role: leader, router, reed, end-device, sleepy-end-device",
        None,
        &[],
    ),
    facet(
        "type",
        "a device type of it (name or ID)",
        Some(Unit::Plain),
        &[],
    ),
    facet("vendor", "its vendor (name or ID)", Some(Unit::Plain), &[]),
    facet(
        "product",
        "its product (name or ID)",
        Some(Unit::Plain),
        &[],
    ),
    facet("name", "its name", None, &[]),
    facet("label", "its NodeLabel", None, &[]),
    facet("fabric", "the fabric of ours it is on", None, &[]),
    facet(
        "eco",
        "another controller it is on: google, apple, home-assistant, … (name or vendor ID)",
        Some(Unit::Plain),
        &[],
    ),
    facet(
        "fabrics",
        "how many fabrics it is on",
        Some(Unit::Plain),
        &[">1"],
    ),
    facet(
        "cluster",
        "a cluster it serves (name or ID)",
        Some(Unit::Plain),
        &[],
    ),
    facet(
        "can",
        "what it can do: ota, logs, time, identify, groups, scenes, memory, energy",
        None,
        &[],
    ),
    facet(
        "power",
        "its power: wired, battery, rechargeable, replaceable",
        None,
        &[],
    ),
    facet(
        "icd",
        "how it idles, if intermittently connected: sit, lit",
        None,
        &[],
    ),
    facet(
        "endpoints",
        "how many application endpoints it has",
        Some(Unit::Plain),
        &[">1"],
    ),
    facet(
        "spec",
        "the Matter version it implements",
        Some(Unit::Version),
        &[">=1.4"],
    ),
    facet(
        "sw",
        "its software version (string or number)",
        Some(Unit::Plain),
        &[],
    ),
    facet(
        "hw",
        "its hardware version (string or number)",
        Some(Unit::Plain),
        &[],
    ),
    facet("node", "its node ID", Some(Unit::Plain), &[]),
    facet(
        "uptime",
        "the time since it booted",
        Some(Unit::Duration),
        &["<1h", ">7d"],
    ),
    facet(
        "reboots",
        "how many times it rebooted",
        Some(Unit::Plain),
        &[">5"],
    ),
    facet(
        "hours",
        "its operational hours",
        Some(Unit::Plain),
        &[">1000"],
    ),
    facet("heap", "its free heap", Some(Unit::Bytes), &["<16k"]),
    facet(
        "ota",
        "its update: offered, downloading, applying, applied, failed",
        None,
        &[],
    ),
];

/// The states `is:` takes, and what each means.
pub static STATES: &[(&str, &str)] = &[
    ("online", "answering"),
    ("offline", "not answering"),
    ("remembered", "we keep a record of it"),
    ("shared", "on another controller's fabric too"),
    ("fabrics-full", "no fabric slot left to share it"),
    ("updatable", "an OTA image here is newer than its software"),
    ("updating", "an update is going on"),
    ("sleepy", "it sleeps: an ICD, or a sleepy end device"),
    ("bridge", "it bridges other devices"),
    (
        "test-vendor",
        "a test vendor ID: a development or certification device",
    ),
];

/// The clusters that tell what a device can do, for `can:`.
const CAPABILITIES: &[(u32, &str)] = &[
    (0x0003, "identify"),
    (0x0004, "groups"),
    (0x002A, "ota"),
    (0x0032, "logs"),
    (0x0034, "memory"),
    (0x0038, "time"),
    (0x0062, "scenes"),
    (0x0090, "energy"),
    (0x0091, "energy"),
];

/// Names a controller's vendor goes by, beyond the registry's.
const ECOSYSTEMS: &[(u16, &str)] = &[(0x1217, "Amazon"), (0x1349, "Apple"), (0x1384, "Apple")];

/// The Aggregator device type: a bridge.
const AGGREGATOR: u32 = 0x000E;

impl Query {
    /// The filter `text`; empty, it matches every device.
    pub fn parse(text: &str) -> Result<Self, QueryError> {
        let mut terms = Vec::new();

        for token in tokens(text) {
            let (negated, token) = match token.strip_prefix('-') {
                Some(rest) if !rest.is_empty() => (true, rest),
                _ => (false, token),
            };

            let test = match split_key(token) {
                Some((key, rest)) => {
                    let facet = facet_of(&key).ok_or(QueryError::UnknownFacet(key))?;
                    let values = values(facet, rest)?;
                    if values.is_empty() {
                        continue;
                    }
                    Test::Facet {
                        key: facet.key,
                        values,
                    }
                }
                None => {
                    let words = slug(&unquote(token));
                    if words.is_empty() {
                        continue;
                    }
                    Test::Words(words)
                }
            };

            terms.push(Term { negated, test });
        }

        Ok(Self { terms })
    }

    /// Whether it matches every device.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Whether a device with `facts` matches.
    pub fn matches(&self, facts: &Facts) -> bool {
        self.terms
            .iter()
            .all(|term| term.test.matches(facts) != term.negated)
    }
}

impl Facts {
    /// The facts of `device`, with what else is `known` of it.
    pub fn of(device: &DeviceInfo, known: &Known<'_>) -> Self {
        let mut facts = Self::default();
        let details = &device.details;
        let fp = &device.fingerprint;

        for tag in &device.tags {
            facts.text("tag", tag);
        }
        facts.text("name", &device.title());
        facts.text("label", &device.node_label);
        facts.text("fabric", &device.fabric);
        facts.number(
            "node",
            &display::hex_u64(device.handle.node_id),
            device.handle.node_id,
        );

        if let Some(net) = device.network.and_then(network_word) {
            facts.text("net", net);
        }
        match device.network {
            Some(NetworkKind::WiFi) => facts.text("ssid", &details.network_name),
            Some(NetworkKind::Thread) => facts.text("thread", &details.network_name),
            _ => {}
        }
        if let Some(role) = details.thread_role {
            facts.text("role", role_word(role));
        }

        for &id in &device.device_types {
            let name = display::device_type_name(id).unwrap_or_default();
            facts.number("type", &name, id.into());
        }
        if let Some(vid) = fp.vendor_id {
            facts.number("vendor", &device.vendor_name, vid.into());
            if let Some(name) = display::vendor_name(vid) {
                facts.number("vendor", &name, vid.into());
            }
        }
        if let (Some(vid), Some(pid)) = (fp.vendor_id, fp.product_id) {
            facts.number("product", &device.product_name, pid.into());
            if let Some(name) = display::product_name(vid, pid) {
                facts.number("product", &name, pid.into());
            }
        }
        if let Some(version) = fp.software_version {
            facts.number("sw", &device.software_version_string, version.into());
        }
        if let Some(version) = fp.hardware_version {
            facts.number("hw", &device.hardware_version_string, version.into());
        }
        if let Some(version) = details.spec_version {
            facts.number("spec", &spec_text(version), version.into());
        }

        for &id in &details.clusters {
            let name = display::cluster_name(id).map(|name| camel_words(&name));
            facts.number("cluster", name.as_deref().unwrap_or_default(), id.into());
        }
        for (id, word) in CAPABILITIES {
            if details.clusters.contains(id) {
                facts.text("can", word);
            }
        }
        for kind in &details.power {
            facts.text("power", power_word(*kind));
        }
        if let Some(mode) = details.icd.as_ref().and_then(|icd| icd.mode) {
            facts.text("icd", icd_word(mode));
        }
        if let Some(endpoints) = details.endpoints {
            facts.number("endpoints", "", endpoints.into());
        }

        let health = known.health;
        let general = health.and_then(|health| health.general);
        let software = health.and_then(|health| health.software.as_ref());
        if let Some(secs) = general.and_then(|g| g.up_time_secs) {
            facts.number("uptime", "", secs);
        }
        if let Some(count) = general.and_then(|g| g.reboot_count) {
            facts.number("reboots", "", count.into());
        }
        if let Some(hours) = general.and_then(|g| g.total_operational_hours) {
            facts.number("hours", "", hours.into());
        }
        if let Some(free) = software.and_then(|s| s.current_heap_free) {
            facts.number("heap", "", free);
        }

        let fabrics = health.and_then(|health| health.fabrics.as_ref());
        if let Some(fabrics) = fabrics {
            facts.number("fabrics", "", fabrics.len() as u64);
            for fabric in fabrics.iter().filter(|fabric| !fabric.ours) {
                let vid = fabric.vendor_id.into();
                let names = display::vendor_name(fabric.vendor_id)
                    .into_iter()
                    .chain(
                        ECOSYSTEMS
                            .iter()
                            .filter(|(id, _)| *id == fabric.vendor_id)
                            .map(|(_, name)| name.to_string()),
                    )
                    .chain(Some(fabric.label.clone()));
                for name in names {
                    facts.number("eco", &name, vid);
                }
                facts.number("eco", &display::hex_u16(fabric.vendor_id), vid);
            }
        }
        if let Some(progress) = known.ota {
            facts.text("ota", ota_word(progress));
        }

        let online = known.online.or(device.online);
        let full = fabrics
            .zip(health.and_then(|health| health.supported_fabrics))
            .is_some_and(|(fabrics, supported)| fabrics.len() >= supported.into());
        let states = [
            ("online", online == Some(true)),
            ("offline", online == Some(false)),
            ("remembered", device.remembered),
            (
                "shared",
                fabrics.is_some_and(|f| f.iter().any(|fabric| !fabric.ours)),
            ),
            ("fabrics-full", full),
            (
                "updatable",
                known.images.iter().any(|image| image.applies_to(device)),
            ),
            ("updating", known.ota.is_some_and(OtaProgress::in_progress)),
            (
                "sleepy",
                details.icd.is_some() || details.thread_role == Some(ThreadRole::SleepyEndDevice),
            ),
            ("bridge", device.device_types.contains(&AGGREGATOR)),
            ("test-vendor", device.is_test_vendor()),
        ];
        for (state, holds) in states {
            if holds {
                facts.text("is", state);
            }
        }

        facts
    }

    /// The values of facet `key`, as slugs.
    pub fn values<'s>(&'s self, key: &'s str) -> impl Iterator<Item = &'s str> + 's {
        self.facts
            .iter()
            .filter(move |fact| fact.key == key && !fact.text.is_empty())
            .map(|fact| fact.text.as_str())
    }

    fn text(&mut self, key: &'static str, text: &str) {
        let text = slug(text);
        if !text.is_empty() {
            self.facts.push(Fact {
                key,
                text,
                number: None,
            });
        }
    }

    fn number(&mut self, key: &'static str, text: &str, number: u64) {
        self.facts.push(Fact {
            key,
            text: slug(text),
            number: Some(number),
        });
    }
}

impl OtaImageInfo {
    /// Whether it updates `device`: for its vendor and product, newer than
    /// what it runs.
    pub fn applies_to(&self, device: &DeviceInfo) -> bool {
        let fp = &device.fingerprint;

        fp.vendor_id
            .is_none_or(|vid| self.vendor_id == 0 || self.vendor_id == vid)
            && fp
                .product_id
                .is_none_or(|pid| self.product_id == 0 || self.product_id == pid)
            && fp.software_version.is_none_or(|v| self.version > v)
    }
}

impl Test {
    fn matches(&self, facts: &Facts) -> bool {
        match self {
            Self::Words(words) => facts.facts.iter().any(|fact| word_start(&fact.text, words)),
            Self::Facet { key, values } => facts
                .facts
                .iter()
                .filter(|fact| fact.key == *key)
                .any(|fact| values.iter().any(|value| value.matches(fact))),
        }
    }
}

impl Value {
    fn matches(&self, fact: &Fact) -> bool {
        match self {
            Self::Text(text, number) => {
                word_start(&fact.text, text) || (number.is_some() && *number == fact.number)
            }
            Self::Compare(cmp, n) => fact.number.is_some_and(|number| match cmp {
                Cmp::Lt => number < *n,
                Cmp::Le => number <= *n,
                Cmp::Eq => number == *n,
                Cmp::Ge => number >= *n,
                Cmp::Gt => number > *n,
            }),
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFacet(key) => {
                let keys: Vec<_> = FACETS.iter().map(|facet| facet.key).collect();
                write!(f, "no filter `{key}:`; there are {}", keys.join(", "))
            }
            Self::UnknownState(state) => {
                let states: Vec<_> = STATES.iter().map(|(state, _)| *state).collect();
                write!(f, "no state `is:{state}`; there are {}", states.join(", "))
            }
            Self::NotNumeric(key) => write!(f, "`{key}:` takes no comparison"),
            Self::BadNumber { key, value } => {
                write!(f, "`{key}:{value}`: not a number `{key}:` takes")
            }
        }
    }
}

impl core::error::Error for QueryError {}

/// `text` as filters match it: lowercase, its words joined by `-`.
pub fn slug(text: &str) -> String {
    let mut out = String::new();

    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }

    out
}

/// The filters, described: how they read, and every facet and state.
pub fn help() -> String {
    let mut out = String::from(
        "A device filter, e.g. `net:thread is:online`, `tag:kitchen,bedroom -eco:google`, \
         `cluster:on-off spec:>=1.4`. All terms must match (AND). Words match the start \
         of a word of anything known of a device: its tags, names and facet values. \
         `key:value` matches one facet; `key:a,b` either value (the only OR); `-term` \
         negates; quotes keep spaces (`tag:\"ground floor\"`). There is no OR between \
         terms and no brackets: list several times instead. Values ignore case and \
         punctuation. Facets with numbers take <, <=, >, >= and =: durations in \
         s/m/h/d/w (uptime:<1h), sizes in k/m/g (heap:<16k), versions as 1.4 \
         (spec:>=1.4), IDs in hex or decimal. The facets from a device's health (eco, \
         fabrics, uptime, reboots, hours, heap, is:shared, is:fabrics-full) are known \
         only of the devices someone follows now: a term on one matches no \
         other device, and its negation matches them all. Facets:",
    );
    for facet in FACETS {
        out.push_str(&format!(" {}: {};", facet.key, facet.help));
    }
    out.push_str(" States (is:):");
    for (state, help) in STATES {
        out.push_str(&format!(" {state}: {help};"));
    }
    out.pop();
    out.push('.');

    out
}

/// The facet `key:` names.
pub fn facet_of(key: &str) -> Option<&'static Facet> {
    let key = key.to_lowercase();
    FACETS.iter().find(|facet| facet.key == key)
}

/// Completions of the last term of filter `text`, from the facts of the
/// devices there are: facets and values for a word, values for `key:`.
pub fn suggest(text: &str, devices: &[Facts], max: usize) -> Vec<Suggestion> {
    let start = last_token_start(text);
    let (head, token) = text.split_at(start);
    let (neg, token) = match token.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", token),
    };

    let mut out = Vec::new();
    match split_key(token) {
        Some((key, rest)) => {
            let Some(facet) = facet_of(&key) else {
                return out;
            };
            // Complete the value after the last comma.
            let (done, partial) = match rest.rfind(',') {
                Some(i) => rest.split_at(i + 1),
                None => ("", rest),
            };
            let partial = slug(&unquote(partial));
            let prefix = format!("{head}{neg}{}:{done}", facet.key);

            if facet.key == "is" {
                for (state, help) in STATES {
                    if word_start(state, &partial) {
                        out.push(Suggestion {
                            text: format!("{prefix}{state} "),
                            label: format!("is:{state}"),
                            help: help.to_string(),
                        });
                    }
                }
            } else {
                for (value, count) in counts(devices, Some(facet.key)) {
                    if word_start(&value.1, &partial) {
                        out.push(Suggestion {
                            text: format!("{prefix}{} ", value.1),
                            label: format!("{}:{}", facet.key, value.1),
                            help: devices_text(count),
                        });
                    }
                }
                for example in facet.examples {
                    if partial.is_empty() {
                        out.push(Suggestion {
                            text: format!("{prefix}{example} "),
                            label: format!("{}:{example}", facet.key),
                            help: facet.help.to_string(),
                        });
                    }
                }
            }
        }
        None => {
            let partial = slug(&unquote(token));
            for facet in FACETS
                .iter()
                .filter(|facet| facet.key.starts_with(&partial))
            {
                out.push(Suggestion {
                    text: format!("{head}{neg}{}:", facet.key),
                    label: format!("{}:", facet.key),
                    help: facet.help.to_string(),
                });
            }
            if !partial.is_empty() {
                for ((key, value), count) in counts(devices, None) {
                    if word_start(&value, &partial) {
                        out.push(Suggestion {
                            text: format!("{head}{neg}{key}:{value} "),
                            label: format!("{key}:{value}"),
                            help: devices_text(count),
                        });
                    }
                }
            }
        }
    }

    out.truncate(max);
    out
}

const fn facet(
    key: &'static str,
    help: &'static str,
    unit: Option<Unit>,
    examples: &'static [&'static str],
) -> Facet {
    Facet {
        key,
        help,
        unit,
        examples,
    }
}

/// The values of `facet`'s term `rest`, comma-separated.
fn values(facet: &'static Facet, rest: &str) -> Result<Vec<Value>, QueryError> {
    let mut values = Vec::new();

    for raw in split_commas(rest) {
        let value = unquote(&raw);
        let value = value.trim();
        if value.is_empty() {
            continue;
        }

        let compare = [
            ("<=", Cmp::Le),
            (">=", Cmp::Ge),
            ("<", Cmp::Lt),
            (">", Cmp::Gt),
            ("=", Cmp::Eq),
        ]
        .into_iter()
        .find_map(|(op, cmp)| value.strip_prefix(op).map(|n| (cmp, n.trim())));

        if let Some((cmp, n)) = compare {
            let unit = facet.unit.ok_or(QueryError::NotNumeric(facet.key))?;
            let n = number(unit, n).ok_or_else(|| QueryError::BadNumber {
                key: facet.key,
                value: value.to_string(),
            })?;
            values.push(Value::Compare(cmp, n));
            continue;
        }

        let text = slug(value);
        if facet.key == "is" && !STATES.iter().any(|(state, _)| *state == text) {
            return Err(QueryError::UnknownState(value.to_string()));
        }
        let n = facet.unit.and_then(|unit| number(unit, value));
        values.push(Value::Text(text, n));
    }

    Ok(values)
}

/// `text` as a number in `unit`.
fn number(unit: Unit, text: &str) -> Option<u64> {
    let text = text.trim().to_lowercase();

    match unit {
        Unit::Plain => match text.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => text.parse().ok(),
        },
        Unit::Duration => {
            let (n, unit) = split_unit(&text);
            let scale = match unit {
                "" | "s" => 1,
                "m" | "min" => 60,
                "h" => 3600,
                "d" => 86_400,
                "w" => 7 * 86_400,
                _ => return None,
            };
            Some(n? * scale)
        }
        Unit::Bytes => {
            let (n, unit) = split_unit(&text);
            let scale = match unit {
                "" | "b" => 1,
                "k" | "kb" | "kib" => 1 << 10,
                "m" | "mb" | "mib" => 1 << 20,
                "g" | "gb" | "gib" => 1 << 30,
                _ => return None,
            };
            Some(n? * scale)
        }
        Unit::Version => {
            let mut parts = text.split('.').map(|part| part.parse::<u8>().ok());
            let major = parts.next()??;
            let minor = parts.next().unwrap_or(Some(0))?;
            let patch = parts.next().unwrap_or(Some(0))?;
            parts.next().is_none().then_some(
                (u64::from(major) << 24) | (u64::from(minor) << 16) | (u64::from(patch) << 8),
            )
        }
    }
}

/// `text`'s leading number, and the unit after it.
fn split_unit(text: &str) -> (Option<u64>, &str) {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (n, unit) = text.split_at(digits);

    (n.parse().ok(), unit.trim())
}

/// Whether `needle` is at the start of a word of `haystack`, both slugs.
fn word_start(haystack: &str, needle: &str) -> bool {
    let mut prev = None;

    for (i, c) in haystack.char_indices() {
        if prev.is_none_or(|p: char| !p.is_alphanumeric()) && haystack[i..].starts_with(needle) {
            return true;
        }
        prev = Some(c);
    }

    needle.is_empty()
}

/// The terms of `text`: split at spaces, but in quotes.
fn tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut quoted = false;
    let mut start = None;

    for (i, c) in text.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c.is_whitespace() && !quoted {
            if let Some(s) = start.take() {
                tokens.push(&text[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        tokens.push(&text[s..]);
    }

    tokens
}

/// Where the last term of `text` starts: after its last space, but in quotes.
fn last_token_start(text: &str) -> usize {
    let mut quoted = false;
    let mut start = 0;

    for (i, c) in text.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if c.is_whitespace() && !quoted {
            start = i + c.len_utf8();
        }
    }

    start
}

/// `token` as `key` and the rest, if it has a `:` out of quotes.
fn split_key(token: &str) -> Option<(String, &str)> {
    let colon = token.find([':', '"'])?;
    (token.as_bytes()[colon] == b':').then(|| (token[..colon].to_lowercase(), &token[colon + 1..]))
}

/// `text` split at commas, but in quotes.
fn split_commas(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut quoted = false;

    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                part.push(c);
            }
            ',' if !quoted => parts.push(core::mem::take(&mut part)),
            c => part.push(c),
        }
    }
    parts.push(part);

    parts
}

/// `text` without its quotes.
fn unquote(text: &str) -> String {
    text.replace('"', "")
}

/// IDL name `name` as words: `OnOff` is `On Off`, `ICDManagement` is
/// `ICD Management`; `WiFi` stays one word.
fn camel_words(name: &str) -> String {
    let name = name.replace("WiFi", "Wifi");
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();

    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                out.push(' ');
            }
        }
        out.push(c);
    }

    out
}

/// How many devices have each value of facet `key` (of every facet if
/// `None`), most first.
fn counts(devices: &[Facts], key: Option<&str>) -> Vec<((&'static str, String), usize)> {
    let mut counts: BTreeMap<(&'static str, String), usize> = BTreeMap::new();

    for facts in devices {
        let mut seen: Vec<(&'static str, &str)> = Vec::new();
        for fact in &facts.facts {
            if fact.text.is_empty() || key.is_some_and(|key| key != fact.key) {
                continue;
            }
            if !seen.contains(&(fact.key, fact.text.as_str())) {
                seen.push((fact.key, fact.text.as_str()));
                *counts.entry((fact.key, fact.text.clone())).or_default() += 1;
            }
        }
    }

    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
}

fn devices_text(count: usize) -> String {
    match count {
        1 => "1 device".into(),
        n => format!("{n} devices"),
    }
}

/// Matter version `version` (`0x01040200`), as `1.4.2`.
fn spec_text(version: u32) -> String {
    format!(
        "{}.{}.{}",
        version >> 24,
        (version >> 16) & 0xFF,
        (version >> 8) & 0xFF
    )
}

/// The value of `net:` for `kind`.
pub fn network_word(kind: NetworkKind) -> Option<&'static str> {
    match kind {
        NetworkKind::WiFi => Some("wifi"),
        NetworkKind::Thread => Some("thread"),
        NetworkKind::Ethernet => Some("ethernet"),
        NetworkKind::Cellular => Some("cellular"),
        NetworkKind::Unspecified => None,
    }
}

fn role_word(role: ThreadRole) -> &'static str {
    match role {
        ThreadRole::Unspecified => "unspecified",
        ThreadRole::Unassigned => "unassigned",
        ThreadRole::SleepyEndDevice => "sleepy-end-device",
        ThreadRole::EndDevice => "end-device",
        ThreadRole::Reed => "reed",
        ThreadRole::Router => "router",
        ThreadRole::Leader => "leader",
    }
}

fn power_word(kind: PowerKind) -> &'static str {
    match kind {
        PowerKind::Wired => "wired",
        PowerKind::Battery => "battery",
        PowerKind::Rechargeable => "rechargeable",
        PowerKind::Replaceable => "replaceable",
    }
}

fn icd_word(mode: IcdMode) -> &'static str {
    match mode {
        IcdMode::Sit => "sit",
        IcdMode::Lit => "lit",
    }
}

fn ota_word(progress: &OtaProgress) -> &'static str {
    match progress {
        OtaProgress::Offered { .. } => "offered",
        OtaProgress::NotOffered => "not-offered",
        OtaProgress::Downloading { .. } => "downloading",
        OtaProgress::Applying { .. } => "applying",
        OtaProgress::Applied { .. } => "applied",
        OtaProgress::Failed { .. } => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn device() -> DeviceInfo {
        DeviceInfo {
            handle: DeviceHandle {
                fabric_index: 1,
                node_id: 0x1B688,
            },
            fabric: "Home".into(),
            unique_id: String::new(),
            serial_number: String::new(),
            node_label: String::new(),
            vendor_name: "Sysgrok".into(),
            product_name: "ClimaNode".into(),
            hardware_version_string: "esp32c6-8".into(),
            software_version_string: "climanode-0.1.7".into(),
            fingerprint: Fingerprint {
                vendor_id: Some(0xFFF2),
                product_id: Some(0x8002),
                hardware_version: Some(1),
                software_version: Some(0x0001_07FF),
            },
            tags: vec!["ground floor".into(), "offline".into()],
            online: Some(true),
            remembered: true,
            check_ins: false,
            awake_ms: None,
            network: Some(NetworkKind::Thread),
            device_types: vec![0x0072],
            details: DeviceDetails {
                spec_version: Some(0x0104_0200),
                clusters: vec![0x0003, 0x0006, 0x002A, 0x0201],
                endpoints: Some(1),
                power: vec![PowerKind::Wired],
                icd: None,
                network_name: "NEST-PAN-0FD8".into(),
                thread_role: Some(ThreadRole::Router),
            },
        }
    }

    fn health() -> DeviceHealth {
        DeviceHealth {
            general: Some(GeneralDiag {
                up_time_secs: Some(1800),
                reboot_count: Some(7),
                total_operational_hours: Some(120),
            }),
            fabrics: Some(vec![fabric(0xFFF1, true), fabric(0x6006, false)]),
            supported_fabrics: Some(2),
            ..Default::default()
        }
    }

    fn fabric(vendor_id: u16, ours: bool) -> DeviceFabric {
        DeviceFabric {
            index: Some(1),
            vendor_id,
            label: String::new(),
            fabric_id: 1,
            node_id: 1,
            root_key: String::new(),
            ours,
            our_name: None,
        }
    }

    fn matches(query: &str) -> bool {
        let health = health();
        let known = Known {
            health: Some(&health),
            ..Default::default()
        };
        Query::parse(query)
            .unwrap()
            .matches(&Facts::of(&device(), &known))
    }

    #[test]
    fn words_match_the_start_of_any_word() {
        assert!(matches(""));
        assert!(matches("ground"));
        assert!(matches("floor"));
        assert!(matches("thread router"));
        assert!(matches("google"));
        assert!(!matches("roun"));
        assert!(!matches("kitchen"));
    }

    #[test]
    fn a_word_matches_a_tag_and_a_state_alike() {
        // The tag "offline" matches the word, not the state.
        assert!(matches("offline"));
        assert!(matches("tag:offline"));
        assert!(!matches("is:offline"));
        assert!(matches("is:online"));
    }

    #[test]
    fn facets_match_their_values_only() {
        assert!(matches("net:thread"));
        assert!(!matches("net:wifi"));
        assert!(matches("net:wifi,thread"));
        assert!(matches("tag:\"ground floor\""));
        assert!(matches("tag:ground-floor"));
        assert!(matches("type:air-conditioner"));
        assert!(matches("type:0x0072"));
        assert!(matches("vendor:sysgrok vendor:0xfff2"));
        assert!(matches("cluster:on-off cluster:thermostat cluster:0x0201"));
        assert!(matches("can:ota can:identify"));
        assert!(!matches("can:logs"));
        assert!(matches("thread:nest-pan role:router power:wired"));
        assert!(matches("eco:google is:shared is:fabrics-full"));
        assert!(matches("-eco:apple -is:updating"));
        assert!(!matches("-net:thread"));
        assert!(matches("is:test-vendor"));
    }

    #[test]
    fn numbers_compare_in_their_units() {
        assert!(matches("uptime:<1h uptime:>=30m"));
        assert!(!matches("uptime:>1h"));
        assert!(matches("reboots:>5 reboots:7 hours:<=120"));
        assert!(matches("spec:>=1.4 spec:<1.5 spec:1.4"));
        assert!(matches("sw:>=0x000107FF sw:climanode"));
        assert!(matches("fabrics:2 node:0x1B688"));
        // Not known without a health report.
        assert!(!matches("heap:<16k"));
    }

    #[test]
    fn bad_filters_say_why() {
        assert_eq!(
            Query::parse("colour:red"),
            Err(QueryError::UnknownFacet("colour".into()))
        );
        assert_eq!(
            Query::parse("is:sleeping"),
            Err(QueryError::UnknownState("sleeping".into()))
        );
        assert_eq!(Query::parse("tag:>3"), Err(QueryError::NotNumeric("tag")));
        assert!(matches!(
            Query::parse("uptime:<1y"),
            Err(QueryError::BadNumber { key: "uptime", .. })
        ));
        // Incomplete terms are left out, as while typing.
        assert!(Query::parse("net: -").unwrap().is_empty());
    }

    #[test]
    fn names_become_slugs() {
        assert_eq!(slug("On/Off Light"), "on-off-light");
        assert_eq!(slug("  Ground  floor! "), "ground-floor");
        assert_eq!(slug("Кухня"), "кухня");
        assert_eq!(camel_words("OnOff"), "On Off");
        assert_eq!(camel_words("ICDManagement"), "ICD Management");
        assert_eq!(
            camel_words("WiFiNetworkDiagnostics"),
            "Wifi Network Diagnostics"
        );
        assert_eq!(
            camel_words("PM25ConcentrationMeasurement"),
            "PM25 Concentration Measurement"
        );
    }

    #[test]
    fn suggestions_complete_the_last_term() {
        let health = health();
        let known = Known {
            health: Some(&health),
            ..Default::default()
        };
        let devices = [Facts::of(&device(), &known)];

        let keys = suggest("kitchen ne", &devices, 10);
        assert_eq!(keys[0].text, "kitchen net:");

        let values = suggest("-net:", &devices, 10);
        assert_eq!(values[0].text, "-net:thread ");
        assert_eq!(values[0].help, "1 device");

        let states = suggest("is:of", &devices, 10);
        assert_eq!(states[0].label, "is:offline");

        let anywhere = suggest("goo", &devices, 10);
        assert!(anywhere.iter().any(|s| s.label == "eco:google-llc"));
    }
}
