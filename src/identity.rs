//! Stable controller identities and the udev rules that hide a remapped
//! controller's physical nodes from games.
//!
//! An identity is `vendor:product` plus the controller's unique id (a MAC
//! address or serial number) when the driver reports one, otherwise its name.
//! Neither depends on the bus or port, so the same controller keeps its
//! profile over USB and Bluetooth.

use evdev::Device;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Name prefix used by virtual controllers of Switch Swap 0.1.x.
const LEGACY_VIRTUAL_NAME_PREFIX: &str = "Switch Swap";
const IDENTITY_VERSION: &str = "v2";

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyKind {
    Unique(String),
    Name(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ControllerKey {
    pub vendor: u16,
    pub product: u16,
    pub kind: KeyKind,
}

impl ControllerKey {
    pub fn encode(&self) -> String {
        let (kind, value) = match &self.kind {
            KeyKind::Unique(value) => ("uniq", value),
            KeyKind::Name(value) => ("name", value),
        };
        hex_encode(&format!(
            "{IDENTITY_VERSION}|{:04x}|{:04x}|{kind}|{value}",
            self.vendor, self.product
        ))
    }

    fn decode(raw: &str) -> Option<Self> {
        let mut fields = raw.splitn(5, '|');
        if fields.next()? != IDENTITY_VERSION {
            return None;
        }
        let vendor = u16::from_str_radix(fields.next()?, 16).ok()?;
        let product = u16::from_str_radix(fields.next()?, 16).ok()?;
        let kind = match (fields.next()?, fields.next()?) {
            ("uniq", value) => KeyKind::Unique(value.to_owned()),
            ("name", value) => KeyKind::Name(value.to_owned()),
            _ => return None,
        };
        Some(Self {
            vendor,
            product,
            kind,
        })
    }
}

/// What a saved profile's udev rules match on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Selector {
    Key(ControllerKey),
    /// Identities saved by Switch Swap 0.1.x; kept until the controller
    /// reconnects and its profile is moved to a [`ControllerKey`].
    LegacyById(String),
    LegacyUnique(String),
    LegacyPhysical(String),
}

pub fn identify(device: &Device) -> ControllerKey {
    let id = device.input_id();
    let kind = match device.unique_name().map(str::trim) {
        Some(unique) if meaningful_unique(unique) => KeyKind::Unique(unique.to_owned()),
        _ => KeyKind::Name(device.name().unwrap_or("Unnamed input device").to_owned()),
    };
    ControllerKey {
        vendor: id.vendor(),
        product: id.product(),
        kind,
    }
}

/// Unique ids made only of zeros (common on cheap USB pads) identify nothing.
fn meaningful_unique(value: &str) -> bool {
    value.chars().any(|c| c.is_ascii_alphanumeric() && c != '0')
}

/// The identity Switch Swap 0.1.x saved, so its profiles can be migrated.
pub fn legacy_identity(path: &Path, device: &Device) -> String {
    let current = fs::canonicalize(path).ok();
    let by_id = fs::read_dir("/dev/input/by-id")
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let target = fs::canonicalize(entry.path()).ok()?;
            (Some(&target) == current.as_ref())
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .min_by_key(|name| {
            let priority = if name.contains("event-joystick") || name.contains("event-gamepad") {
                0
            } else {
                1
            };
            (priority, name.clone())
        });
    let (kind, value) = if let Some(by_id) = by_id {
        ("by-id", by_id)
    } else if let Some(unique) = device.unique_name().filter(|value| !value.is_empty()) {
        ("unique", unique.to_owned())
    } else if let Some(physical) = device.physical_path().filter(|value| !value.is_empty()) {
        ("physical", physical.to_owned())
    } else {
        (
            "event",
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        )
    };
    hex_encode(&format!(
        "{:?}|{}|{kind}|{value}",
        device.input_id(),
        device.name().unwrap_or_default()
    ))
}

/// Whether `/dev/input/eventN` is a virtual (uinput) device: Switch Swap's
/// own output, Steam Input's pads, other remappers. These are never offered
/// for remapping, matching the udev rules, which never hide them.
pub fn is_virtual_device(event_path: &Path) -> bool {
    let Some(event) = event_path.file_name() else {
        return false;
    };
    Path::new("/sys/class/input")
        .join(event)
        .join("device")
        .canonicalize()
        .is_ok_and(|syspath| syspath.starts_with("/sys/devices/virtual"))
}

pub fn is_legacy_virtual_name(name: &str) -> bool {
    name.starts_with(LEGACY_VIRTUAL_NAME_PREFIX)
}

pub fn selector_from_id(id: &str) -> Option<Selector> {
    let raw = hex_decode(id)?;
    if let Some(key) = ControllerKey::decode(&raw) {
        return udev_safe(match &key.kind {
            KeyKind::Unique(value) | KeyKind::Name(value) => value,
        })
        .then_some(Selector::Key(key));
    }

    let mut fields = raw.rsplit('|');
    let value = fields.next()?;
    let kind = fields.next()?;
    let value = match kind {
        "by-id" => &value[..value.rfind("-event-")?],
        "unique" | "physical" => value,
        _ => return None,
    };
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:+/-".contains(&byte))
    {
        return None;
    }
    match kind {
        "by-id" => Some(Selector::LegacyById(value.to_owned())),
        "unique" => Some(Selector::LegacyUnique(value.to_owned())),
        _ => Some(Selector::LegacyPhysical(value.to_owned())),
    }
}

/// A value can be placed inside a quoted udev match only if it has no quote,
/// escape, glob, or alternative (`|`) characters.
pub fn udev_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| !c.is_control() && !matches!(c, '"' | '\\' | '*' | '?' | '[' | ']' | '|'))
}

/// How to recognise a controller's hidraw node: the HID device's own unique id
/// or name, which can differ from what the input driver reports (hid-nintendo
/// sets the input `uniq` to the MAC, while USB HID reports the serial number).
pub fn hidraw_match(event_path: &Path) -> Option<String> {
    let event = event_path.file_name()?.to_str()?;
    let hid = Path::new("/sys/class/input")
        .join(event)
        .join("device/device");
    let subsystem = fs::read_link(hid.join("subsystem")).ok()?;
    if subsystem.file_name()? != "hid" {
        return None;
    }
    let uevent = fs::read_to_string(hid.join("uevent")).ok()?;
    let property = |key: &str| {
        uevent
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
            .filter(|value| udev_safe(value))
    };
    if let Some(unique) = property("HID_UNIQ").filter(|value| meaningful_unique(value)) {
        Some(format!("uniq={unique}"))
    } else {
        property("HID_NAME").map(|name| format!("name={name}"))
    }
}

// `MODE:=` is final, so no later rule can open the node up again.
const HIDE: &str = "ENV{ID_INPUT_JOYSTICK}=\"\", ENV{LIBINPUT_IGNORE_DEVICE}=\"1\", MODE:=\"0000\", TAG-=\"uaccess\"";
const HIDE_RAW: &str = "MODE:=\"0000\", TAG-=\"uaccess\"";
/// HID_UNIQ and HID_PHYS live on the hidraw node's parent HID device.
const IMPORT_HID: &str = "IMPORT{parent}=\"HID_*\"";
/// Virtual devices (ours, Steam's, other remappers') are never hidden.
const PHYSICAL_ONLY: &str = "DEVPATH!=\"/devices/virtual/*\"";

pub fn rule_lines(selector: &Selector, hidraw_matches: &BTreeSet<String>) -> Vec<String> {
    let key = match selector {
        Selector::Key(key) => key,
        // by-id links exist only for input nodes; the hidraw node of such a
        // profile is hidden once the controller reconnects and is migrated.
        Selector::LegacyById(base) => {
            return vec![format!(
                "SUBSYSTEM==\"input\", KERNEL==\"event*|js*\", {PHYSICAL_ONLY}, ENV{{DEVLINKS}}==\"*{base}*\", {HIDE}"
            )];
        }
        Selector::LegacyUnique(unique) => {
            return legacy_lines(
                &format!("ATTRS{{uniq}}==\"{unique}\""),
                &format!("ENV{{HID_UNIQ}}==\"{unique}\""),
            );
        }
        Selector::LegacyPhysical(physical) => {
            return legacy_lines(
                &format!("ATTRS{{phys}}==\"{physical}\""),
                &format!("ENV{{HID_PHYS}}==\"{physical}\""),
            );
        }
    };

    let ids = format!(
        "ATTRS{{id/vendor}}==\"{:04x}\", ATTRS{{id/product}}==\"{:04x}\"",
        key.vendor, key.product
    );
    let input_match = match &key.kind {
        KeyKind::Unique(unique) => format!("{ids}, ATTRS{{uniq}}==\"{unique}\""),
        KeyKind::Name(name) => format!("{ids}, ATTRS{{name}}==\"{name}\""),
    };
    let mut lines = vec![format!(
        "SUBSYSTEM==\"input\", KERNEL==\"event*|js*\", {PHYSICAL_ONLY}, {input_match}, {HIDE}"
    )];

    // HID device names are BUS:VENDOR:PRODUCT.INSTANCE in upper-case hex; the
    // bus is left open so USB and Bluetooth connections both match.
    let hid_kernel = format!("KERNELS==\"*:{:04X}:{:04X}.*\"", key.vendor, key.product);
    let mut raw_matches = hidraw_matches
        .iter()
        .filter_map(|entry| {
            let (field, value) = entry.split_once('=')?;
            let property = match field {
                "uniq" => "HID_UNIQ",
                "name" => "HID_NAME",
                _ => return None,
            };
            udev_safe(value).then(|| format!("ENV{{{property}}}==\"{value}\""))
        })
        .collect::<BTreeSet<_>>();
    if raw_matches.is_empty() {
        raw_matches.insert(match &key.kind {
            KeyKind::Unique(unique) => format!("ENV{{HID_UNIQ}}==\"{unique}\""),
            KeyKind::Name(name) => format!("ENV{{HID_NAME}}==\"{name}\""),
        });
    }
    lines.push(format!("SUBSYSTEM==\"hidraw\", {hid_kernel}, {IMPORT_HID}"));
    for raw_match in raw_matches {
        lines.push(format!(
            "SUBSYSTEM==\"hidraw\", {hid_kernel}, {raw_match}, {HIDE_RAW}"
        ));
    }
    lines
}

fn legacy_lines(input_match: &str, raw_match: &str) -> Vec<String> {
    vec![
        format!(
            "SUBSYSTEM==\"input\", KERNEL==\"event*|js*\", {PHYSICAL_ONLY}, {input_match}, {HIDE}"
        ),
        format!("SUBSYSTEM==\"hidraw\", {IMPORT_HID}"),
        format!("SUBSYSTEM==\"hidraw\", {raw_match}, {HIDE_RAW}"),
    ]
}

fn hex_encode(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(value: &str) -> Option<String> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    let bytes = pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pro_controller() -> ControllerKey {
        ControllerKey {
            vendor: 0x057e,
            product: 0x2009,
            kind: KeyKind::Unique("E4:17:D8:D3:AD:15".to_owned()),
        }
    }

    #[test]
    fn identities_round_trip_and_ignore_the_bus() {
        let key = pro_controller();
        assert_eq!(selector_from_id(&key.encode()), Some(Selector::Key(key)));
    }

    #[test]
    fn unique_rules_match_usb_and_bluetooth_and_skip_virtual_devices() {
        let rules = rule_lines(&Selector::Key(pro_controller()), &BTreeSet::new());
        assert!(rules[0].contains("KERNEL==\"event*|js*\""));
        assert!(rules[0].contains("DEVPATH!=\"/devices/virtual/*\""));
        assert!(rules[0].contains("ATTRS{id/vendor}==\"057e\", ATTRS{id/product}==\"2009\""));
        assert!(rules[0].contains("ATTRS{uniq}==\"E4:17:D8:D3:AD:15\""));
        assert!(rules[0].contains("MODE:=\"0000\", TAG-=\"uaccess\""));
        assert!(rules[1].contains("KERNELS==\"*:057E:2009.*\", IMPORT{parent}=\"HID_*\""));
        assert!(rules[2].contains("ENV{HID_UNIQ}==\"E4:17:D8:D3:AD:15\""));
    }

    #[test]
    fn learned_hidraw_matches_replace_the_guess() {
        let learned = BTreeSet::from([
            "uniq=000000000001".to_owned(),
            "uniq=e4:17:d8:d3:ad:15".to_owned(),
        ]);
        let rules = rule_lines(&Selector::Key(pro_controller()), &learned);
        assert_eq!(rules.len(), 4);
        assert!(rules[2].contains("ENV{HID_UNIQ}==\"000000000001\""));
        assert!(rules[3].contains("ENV{HID_UNIQ}==\"e4:17:d8:d3:ad:15\""));
    }

    #[test]
    fn name_identities_match_by_name() {
        let key = ControllerKey {
            vendor: 0x20d6,
            product: 0xa718,
            kind: KeyKind::Name("PowerA NSW Wired controller".to_owned()),
        };
        let rules = rule_lines(&Selector::Key(key), &BTreeSet::new());
        assert!(rules[0].contains("ATTRS{name}==\"PowerA NSW Wired controller\""));
        assert!(rules[2].contains("ENV{HID_NAME}==\"PowerA NSW Wired controller\""));
    }

    #[test]
    fn names_that_would_break_a_udev_rule_are_not_isolatable() {
        for name in ["Pad \"X\"", "Pad*", "A|B", "Pad\\1"] {
            let key = ControllerKey {
                vendor: 1,
                product: 2,
                kind: KeyKind::Name(name.to_owned()),
            };
            assert_eq!(selector_from_id(&key.encode()), None, "{name}");
        }
    }

    #[test]
    fn zero_serials_are_not_unique() {
        assert!(!meaningful_unique("000000000000"));
        assert!(!meaningful_unique("00:00:00:00:00:00"));
        assert!(meaningful_unique("000000000001"));
    }

    #[test]
    fn legacy_identities_still_produce_rules() {
        let id = hex_encode("ignored|Pro Controller|by-id|usb-Nintendo_Pro_0001-event-joystick");
        let selector = selector_from_id(&id).unwrap();
        assert_eq!(
            selector,
            Selector::LegacyById("usb-Nintendo_Pro_0001".to_owned())
        );
        let rules = rule_lines(&selector, &BTreeSet::new());
        assert!(rules[0].contains("ENV{DEVLINKS}==\"*usb-Nintendo_Pro_0001*\""));
        assert_eq!(rules.len(), 1);
        let unique = rule_lines(
            &Selector::LegacyUnique("AA:BB".to_owned()),
            &BTreeSet::new(),
        );
        assert!(unique[1].contains("IMPORT{parent}=\"HID_*\""));
        assert!(unique[2].contains("ENV{HID_UNIQ}==\"AA:BB\""));
        assert_eq!(selector_from_id(&hex_encode("x|y|event|event29")), None);
        assert_eq!(selector_from_id("abc"), None);
    }
}
