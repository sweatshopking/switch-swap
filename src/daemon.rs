use crate::identity;
use crate::mapping::{Layout, map_event};
use crate::virtual_pad::VirtualPad;
use evdev::{AbsoluteAxisCode, Device, KeyCode};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

const SOCKET_DIR: &str = "/run/switch-swap";
const SOCKET_PATH: &str = "/run/switch-swap/control.sock";
const SETTINGS_DIR: &str = "/etc/switch-swap";
const SETTINGS_PATH: &str = "/etc/switch-swap/settings.conf";
/// After every rule that grants controller access (steam-devices at 60,
/// systemd's 70-uaccess, game-devices-udev's 71-*) and before 73-seat-late,
/// which turns the uaccess tag into the user's access.
const ISOLATION_RULE_PATH: &str = "/etc/udev/rules.d/72-switch-swap-isolation.rules";
/// Where Switch Swap 0.1.x kept its rules.
const LEGACY_ISOLATION_RULE_PATH: &str = "/etc/udev/rules.d/69-switch-swap-isolation.rules";
/// Set while udev may not have applied the rule file on disk (a reload failed).
static UDEV_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const ISOLATION_RULE_HEADER: &str =
    "# Managed by Switch Swap; do not edit while controller isolation is enabled.\n";
const MAX_CONTROLLERS: usize = 4;

#[derive(Clone, Default)]
struct Settings {
    profiles: HashMap<String, ControllerProfile>,
    legacy_global_setting: bool,
}

#[derive(Clone)]
struct ControllerProfile {
    name: String,
    source_layout: Layout,
    target_layout: Layout,
    /// HID-level matches learned from the connected controller (see
    /// [`identity::hidraw_match`]), so its hidraw node is hidden at plug time.
    hidraw: BTreeSet<String>,
}

#[derive(Clone, Default)]
struct DeviceStatus {
    id: String,
    name: String,
    active: bool,
    error: Option<String>,
}

#[derive(Default)]
struct ServiceStatus {
    devices: HashMap<String, DeviceStatus>,
    candidates: Vec<(String, String)>,
    errors: HashMap<String, String>,
    repair_required: bool,
    message: Option<String>,
}

struct DiscoveredController {
    path: PathBuf,
    id: String,
    legacy_id: String,
    hidraw: Option<String>,
    name: String,
}

#[derive(Default)]
struct Discovery {
    controllers: Vec<DiscoveredController>,
    candidates: Vec<(String, String)>,
    scanned: usize,
    names: Vec<String>,
    open_errors: Vec<String>,
    access_denied: bool,
}

pub fn run() -> Result<(), Box<dyn Error>> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("The system service must run as root. Start it with systemd; do not run the GUI as root.".into());
    }

    let mut settings = read_settings()?;
    let profile_count = settings.profiles.len();
    settings
        .profiles
        .retain(|_, profile| !identity::is_legacy_virtual_name(&profile.name));
    if settings.profiles.len() != profile_count {
        save_settings(&settings)?;
        eprintln!("Removed saved profiles that targeted Switch Swap virtual controllers.");
    }
    refresh_isolation_rules(&settings, None)?;
    fs::create_dir_all(SOCKET_DIR)?;
    if Path::new(SOCKET_PATH).exists() {
        let metadata = fs::symlink_metadata(SOCKET_PATH)?;
        if !metadata.file_type().is_socket() {
            return Err(format!(
                "{SOCKET_PATH} exists and is not a socket; refusing to replace it"
            )
            .into());
        }
        fs::remove_file(SOCKET_PATH)?;
    }

    let listener = UnixListener::bind(SOCKET_PATH)?;
    fs::set_permissions(SOCKET_PATH, fs::Permissions::from_mode(0o666))?;
    listener.set_nonblocking(true)?;

    let settings = Arc::new(Mutex::new(settings));
    let status = Arc::new(Mutex::new(ServiceStatus::default()));
    let (disconnected_tx, disconnected_rx) = mpsc::channel::<String>();
    let mut workers = HashSet::<String>::new();
    // Profile updates that failed, by "path|id", so they are not retried twice a second.
    let mut update_failures = HashSet::<String>::new();
    // Controllers already reported as impossible to hide, so they are logged once.
    let mut unsafe_logged = HashSet::<String>::new();
    let mut last_scan_error = None;

    loop {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // A window closing while it waits for an answer is not a problem.
                    if let Err(error) = handle_client(stream, &settings, &status)
                        && error
                            .downcast_ref::<std::io::Error>()
                            .map(std::io::Error::kind)
                            != Some(std::io::ErrorKind::BrokenPipe)
                    {
                        eprintln!("control request failed: {error}");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    eprintln!("control socket accept failed: {error}");
                    thread::sleep(Duration::from_millis(100));
                    break;
                }
            }
        }

        while let Ok(path) = disconnected_rx.try_recv() {
            workers.remove(&path);
            if let Ok(mut current) = status.lock()
                && let Some(device) = current.devices.remove(&path)
                && let Some(error) = device.error
            {
                current
                    .errors
                    .insert(path, format!("{}: {error}", device.name));
            }
        }

        match discover_controllers() {
            Ok(mut discovery) => {
                last_scan_error = None;
                discovery
                    .controllers
                    .sort_by(|left, right| left.id.cmp(&right.id));
                let discovered_count = discovery.controllers.len();
                let legacy_global_setting = settings
                    .lock()
                    .map_err(|_| "Service settings lock is poisoned")?
                    .legacy_global_setting;
                if let Ok(mut current) = status.lock() {
                    current.candidates.clone_from(&discovery.candidates);
                    current.repair_required = discovery.access_denied;
                    current.message =
                        Some(discovery.message(MAX_CONTROLLERS, legacy_global_setting));
                    for controller in &discovery.controllers {
                        current
                            .errors
                            .remove(&controller.path.to_string_lossy().into_owned());
                    }
                    current.errors.retain(|path, _| Path::new(path).exists());
                    // Devices without a worker (those that cannot be hidden) get no
                    // disconnect message; drop them once they are gone.
                    current
                        .devices
                        .retain(|path, _| workers.contains(path) || Path::new(path).exists());
                }
                update_failures.retain(|key| {
                    key.split_once('|')
                        .is_some_and(|(path, _)| Path::new(path).exists())
                });
                unsafe_logged.retain(|path| Path::new(path).exists());

                for controller in discovery.controllers {
                    // Workers lock the settings on every loop, so the slow work
                    // (writing files, running udevadm) happens without the lock.
                    // The control socket is served on this thread too, so nothing
                    // else changes the settings meanwhile.
                    let failure_key = format!("{}|{}", controller.path.display(), controller.id);
                    let current = settings
                        .lock()
                        .map_err(|_| "Service settings lock is poisoned")?
                        .clone();
                    let mut updated = current.clone();
                    if !update_failures.contains(&failure_key)
                        && update_connected_profile(&mut updated, &controller)
                    {
                        match save_settings(&updated)
                            .and_then(|()| refresh_isolation_rules(&updated, Some(&current)))
                        {
                            Ok(()) => {
                                *settings
                                    .lock()
                                    .map_err(|_| "Service settings lock is poisoned")? = updated;
                            }
                            Err(error) => {
                                // Not retried until the controller reconnects.
                                eprintln!(
                                    "Cannot update the saved profile for {}: {error}",
                                    controller.name
                                );
                                update_failures.insert(failure_key);
                            }
                        }
                    }
                    let DiscoveredController { path, id, name, .. } = controller;
                    let path_key = path.to_string_lossy().into_owned();
                    if workers.contains(&path_key) {
                        continue;
                    }
                    if workers.len() >= MAX_CONTROLLERS {
                        if let Ok(mut current) = status.lock() {
                            current.message = Some(format!(
                                "Only the first {MAX_CONTROLLERS} supported controllers are active."
                            ));
                        }
                        break;
                    }

                    let settings_snapshot = settings
                        .lock()
                        .map_err(|_| "Service settings lock is poisoned")?
                        .clone();
                    let profile_requires_isolation = settings_snapshot.profiles.contains_key(&id);
                    if profile_requires_isolation
                        && !profile_has_safe_isolation(&settings_snapshot, &id)
                    {
                        let error =
                            "Cannot safely hide this controller: its identity is not stable or is shared with another saved controller."
                                .to_owned();
                        if let Ok(mut current) = status.lock() {
                            current.devices.insert(
                                path_key.clone(),
                                DeviceStatus {
                                    id: id.clone(),
                                    name: name.clone(),
                                    active: false,
                                    error: Some(error.clone()),
                                },
                            );
                        }
                        if unsafe_logged.insert(path_key.clone()) {
                            eprintln!("controller {path_key}: {error}");
                        }
                        continue;
                    }

                    workers.insert(path_key.clone());
                    if let Ok(mut current) = status.lock() {
                        current.errors.remove(&path_key);
                        current.message = None;
                        current.devices.insert(
                            path_key.clone(),
                            DeviceStatus {
                                id: id.clone(),
                                name: name.clone(),
                                active: false,
                                error: None,
                            },
                        );
                    }

                    let settings = Arc::clone(&settings);
                    let status = Arc::clone(&status);
                    let disconnected = disconnected_tx.clone();
                    let worker_path_key = path_key.clone();
                    thread::spawn(move || {
                        controller_worker(path, worker_path_key, id, settings, status);
                        let _ = disconnected.send(path_key);
                    });
                }
                if discovered_count > MAX_CONTROLLERS
                    && let Ok(mut current) = status.lock()
                {
                    current.message = Some(format!(
                        "Found {discovered_count} compatible controllers; only the first {MAX_CONTROLLERS} are active."
                    ));
                }
            }
            Err(error) => {
                let message = format!("Cannot scan /dev/input: {error}");
                if last_scan_error.as_deref() != Some(message.as_str()) {
                    eprintln!("{message}");
                    last_scan_error = Some(message.clone());
                }
                if let Ok(mut current) = status.lock() {
                    current.message = Some(message);
                }
            }
        }

        thread::sleep(Duration::from_millis(500));
    }
}

pub fn restore_isolation() -> Result<(), Box<dyn Error>> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("Restoring controller access requires administrator authorization.".into());
    }
    let mut removed = false;
    for path in [ISOLATION_RULE_PATH, LEGACY_ISOLATION_RULE_PATH] {
        removed |= remove_managed_rule_file(Path::new(path))?;
    }
    if removed {
        reconcile_udev(None)?;
    }
    Ok(())
}

/// Removes a Switch Swap rule file; refuses one it did not write.
fn remove_managed_rule_file(rule_path: &Path) -> Result<bool, Box<dyn Error>> {
    let contents = match fs::read_to_string(rule_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!("Cannot read {}: {error}", rule_path.display()).into());
        }
    };
    if !contents.starts_with(ISOLATION_RULE_HEADER) {
        return Err(format!(
            "Refusing to remove {} because it is not managed by Switch Swap.",
            rule_path.display()
        )
        .into());
    }
    fs::remove_file(rule_path)
        .map_err(|error| format!("Cannot remove {}: {error}", rule_path.display()))?;
    Ok(true)
}

fn read_settings() -> Result<Settings, Box<dyn Error>> {
    let contents = match fs::read_to_string(SETTINGS_PATH) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(error) => return Err(format!("Cannot read {SETTINGS_PATH}: {error}").into()),
    };

    parse_settings(&contents)
}

fn parse_settings(contents: &str) -> Result<Settings, Box<dyn Error>> {
    let mut settings = Settings::default();
    let mut hidraw_lines = Vec::new();
    for (line_number, line) in contents.lines().enumerate() {
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("Invalid settings at line {}", line_number + 1).into());
        };
        match key.trim() {
            "enabled" => {
                settings.legacy_global_setting = match value.trim() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(
                            format!("Invalid enabled value at line {}", line_number + 1).into()
                        );
                    }
                };
            }
            "layout" => {
                Layout::parse(value.trim()).ok_or_else(|| {
                    format!(
                        "Unsupported layout at line {}: {}",
                        line_number + 1,
                        value.trim()
                    )
                })?;
            }
            "version" if matches!(value.trim(), "2" | "3" | "4") => {}
            "hidraw" => {
                let (id, matcher) = value
                    .split_once('|')
                    .ok_or_else(|| format!("Invalid hidraw match at line {}", line_number + 1))?;
                hidraw_lines.push((id.to_owned(), matcher.to_owned()));
            }
            "device" => {
                let fields = value.splitn(4, '|').collect::<Vec<_>>();
                if !matches!(fields.len(), 3 | 4) || !valid_device_id(fields[0]) {
                    return Err(
                        format!("Invalid controller profile at line {}", line_number + 1).into(),
                    );
                }
                let first_layout = Layout::parse(fields[1]).ok_or_else(|| {
                    format!(
                        "Unsupported layout at line {}: {}",
                        line_number + 1,
                        fields[1]
                    )
                })?;
                let (source_layout, target_layout, name) = if fields.len() == 3 {
                    let source_layout = if first_layout == Layout::Switch {
                        Layout::Xbox
                    } else {
                        first_layout
                    };
                    (source_layout, first_layout, fields[2])
                } else {
                    let target_layout = Layout::parse(fields[2]).ok_or_else(|| {
                        format!(
                            "Unsupported layout at line {}: {}",
                            line_number + 1,
                            fields[2]
                        )
                    })?;
                    (first_layout, target_layout, fields[3])
                };
                settings.profiles.insert(
                    fields[0].to_owned(),
                    ControllerProfile {
                        name: name.to_owned(),
                        source_layout,
                        target_layout,
                        hidraw: BTreeSet::new(),
                    },
                );
            }
            _ => return Err(format!("Unknown setting at line {}", line_number + 1).into()),
        }
    }
    for (id, matcher) in hidraw_lines {
        if let Some(profile) = settings.profiles.get_mut(&id) {
            profile.hidraw.insert(matcher);
        }
    }
    Ok(settings)
}

fn save_settings(settings: &Settings) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(SETTINGS_DIR)?;
    let temporary_path = format!("{SETTINGS_PATH}.tmp");
    let mut profiles = settings.profiles.iter().collect::<Vec<_>>();
    profiles.sort_by(|left, right| left.0.cmp(right.0));
    let mut contents = String::from("version=4\n");
    for (id, profile) in profiles {
        contents.push_str(&format!(
            "device={}|{}|{}|{}\n",
            id,
            profile.source_layout.as_str(),
            profile.target_layout.as_str(),
            clean_field(&profile.name)
        ));
        for matcher in &profile.hidraw {
            contents.push_str(&format!("hidraw={id}|{}\n", clean_field(matcher)));
        }
    }
    fs::write(&temporary_path, contents)?;
    fs::rename(temporary_path, SETTINGS_PATH)?;
    Ok(())
}

fn profile_has_safe_isolation(settings: &Settings, id: &str) -> bool {
    let Some(selector) = identity::selector_from_id(id) else {
        return false;
    };
    !settings.profiles.keys().any(|other_id| {
        other_id != id && identity::selector_from_id(other_id).as_ref() == Some(&selector)
    })
}

fn render_isolation_rules(settings: &Settings) -> String {
    let mut profiles = settings.profiles.iter().collect::<Vec<_>>();
    profiles.sort_by(|left, right| left.0.cmp(right.0));
    // Order matters: a hidraw IMPORT line must precede the lines that use it.
    let mut rules = Vec::new();
    for (id, profile) in profiles {
        if !profile_has_safe_isolation(settings, id) {
            continue;
        }
        if let Some(selector) = identity::selector_from_id(id) {
            for line in identity::rule_lines(&selector, &profile.hidraw) {
                if !rules.contains(&line) {
                    rules.push(line);
                }
            }
        }
    }
    if rules.is_empty() {
        String::new()
    } else {
        format!("{ISOLATION_RULE_HEADER}{}\n", rules.join("\n"))
    }
}

/// Writes the rule file for `settings` and has udev apply it. `previous` is the
/// settings the current rules were made for, so only controllers that gain or
/// lose a rule are re-examined; `None` re-examines every input device.
fn refresh_isolation_rules(
    settings: &Settings,
    previous: Option<&Settings>,
) -> Result<(), Box<dyn Error>> {
    use std::sync::atomic::Ordering;

    let desired = render_isolation_rules(settings);
    let existing = match fs::read_to_string(ISOLATION_RULE_PATH) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("Cannot read {ISOLATION_RULE_PATH}: {error}").into());
        }
    };
    if existing
        .as_ref()
        .is_some_and(|contents| !contents.starts_with(ISOLATION_RULE_HEADER))
    {
        return Err(format!(
            "Refusing to overwrite {} because it is not managed by Switch Swap.",
            ISOLATION_RULE_PATH
        )
        .into());
    }
    let legacy_removed = remove_managed_rule_file(Path::new(LEGACY_ISOLATION_RULE_PATH))?;
    let unchanged =
        existing.as_deref() == Some(desired.as_str()) || (existing.is_none() && desired.is_empty());
    let pending = UDEV_PENDING.load(Ordering::SeqCst);
    if unchanged && !legacy_removed && !pending {
        return Ok(());
    }

    if !unchanged {
        let rule_path = Path::new(ISOLATION_RULE_PATH);
        let parent = rule_path
            .parent()
            .ok_or("Isolation rule path has no parent directory")?;
        fs::create_dir_all(parent)?;
        if desired.is_empty() {
            match fs::remove_file(rule_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("Cannot remove {}: {error}", rule_path.display()).into());
                }
            }
        } else {
            let temporary =
                parent.join(format!(".switch-swap-isolation-{}.tmp", std::process::id()));
            fs::write(&temporary, desired.as_bytes())
                .map_err(|error| format!("Cannot write {}: {error}", temporary.display()))?;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644))?;
            fs::rename(&temporary, rule_path)
                .map_err(|error| format!("Cannot install {}: {error}", rule_path.display()))?;
        }
    }

    UDEV_PENDING.store(true, Ordering::SeqCst);
    // A failed earlier reload may have left any device out of date.
    let scope = if pending || legacy_removed {
        None
    } else {
        previous.and_then(|previous| affected_models(settings, previous))
    };
    reconcile_udev(scope.as_ref())?;
    UDEV_PENDING.store(false, Ordering::SeqCst);
    Ok(())
}

/// The vendor:product pairs whose rules differ between two settings; None when
/// a 0.1.x profile is involved, whose rules are not tied to one model.
fn affected_models(settings: &Settings, previous: &Settings) -> Option<BTreeSet<(u16, u16)>> {
    let mut models = BTreeSet::new();
    for id in settings.profiles.keys().chain(previous.profiles.keys()) {
        match identity::selector_from_id(id) {
            Some(identity::Selector::Key(key)) => {
                models.insert((key.vendor, key.product));
            }
            Some(_) => return None,
            None => {}
        }
    }
    Some(models)
}

/// Reloads udev's rules and has it re-apply them: to the input and hidraw
/// devices of `models`, or to every input and hidraw device when `None`.
fn reconcile_udev(models: Option<&BTreeSet<(u16, u16)>>) -> Result<(), Box<dyn Error>> {
    run_udevadm(&["control", "--reload"])?;
    let Some(models) = models else {
        run_udevadm(&["trigger", "--action=change", "--subsystem-match=input"])?;
        run_udevadm(&["trigger", "--action=change", "--subsystem-match=hidraw"])?;
        return Ok(());
    };
    let devices = model_devices(models);
    if devices.is_empty() {
        return Ok(());
    }
    let mut arguments = vec!["trigger".to_owned(), "--action=change".to_owned()];
    arguments.extend(devices);
    run_udevadm(&arguments.iter().map(String::as_str).collect::<Vec<_>>())
}

/// sysfs paths of the input and hidraw devices of the given models.
fn model_devices(models: &BTreeSet<(u16, u16)>) -> Vec<String> {
    let read_hex = |path: PathBuf| {
        fs::read_to_string(path)
            .ok()
            .and_then(|value| u16::from_str_radix(value.trim(), 16).ok())
    };
    let mut devices = Vec::new();
    for entry in fs::read_dir("/sys/class/input")
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        let id = path.join("device/id");
        if let (Some(vendor), Some(product)) =
            (read_hex(id.join("vendor")), read_hex(id.join("product")))
            && models.contains(&(vendor, product))
        {
            devices.push(path.to_string_lossy().into_owned());
        }
    }
    for entry in fs::read_dir("/sys/class/hidraw")
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        // HID_ID=0005:0000057E:00002009
        let model = fs::read_to_string(path.join("device/uevent"))
            .ok()
            .and_then(|uevent| {
                let id = uevent
                    .lines()
                    .find_map(|line| line.strip_prefix("HID_ID="))?;
                let mut fields = id.split(':').skip(1);
                let mut field = || {
                    u32::from_str_radix(fields.next()?, 16)
                        .ok()?
                        .try_into()
                        .ok()
                };
                Some((field()?, field()?))
            });
        if model.is_some_and(|model| models.contains(&model)) {
            devices.push(path.to_string_lossy().into_owned());
        }
    }
    devices
}

fn run_udevadm(arguments: &[&str]) -> Result<(), Box<dyn Error>> {
    let output = Command::new("udevadm")
        .args(arguments)
        .output()
        .map_err(|error| format!("Cannot run udevadm: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "udevadm {} failed ({}): {}",
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}

fn handle_client(
    mut stream: UnixStream,
    settings: &Arc<Mutex<Settings>>,
    status: &Arc<Mutex<ServiceStatus>>,
) -> Result<(), Box<dyn Error>> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let peer = peer_uid(&stream)?;
    let mut request = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut request)?;
    let request = request.trim();

    let response = if request == "STATUS" {
        make_status(settings, status)
    } else if peer != 0 {
        "ERR Applying or undoing a controller layout requires administrator authorization.\n"
            .to_owned()
    } else if let Some(arguments) = request.strip_prefix("APPLY ") {
        let fields = arguments.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 || !valid_device_id(fields[2]) {
            "ERR Usage: APPLY <source-layout> <target-layout> <controller-id>.\n".to_owned()
        } else if let (Some(source), Some(target)) =
            (Layout::parse(fields[0]), Layout::parse(fields[1]))
        {
            match apply_profile(fields[2], source, target, settings, status) {
                Ok(name) => format!(
                    "OK {} -> {} conversion saved for {name} only. Physical input isolation is active; restart games/Steam or reboot to close handles that were already open.\n",
                    source.as_str(),
                    target.as_str()
                ),
                Err(error) => format!("ERR {error}\n"),
            }
        } else {
            "ERR Usage: APPLY <source-layout> <target-layout> <controller-id>.\n".to_owned()
        }
    } else if let Some(id) = request.strip_prefix("DISABLE ") {
        if !valid_device_id(id) {
            "ERR Usage: DISABLE <controller-id>.\n".to_owned()
        } else {
            match disable_profile(id, settings) {
                Ok(name) => format!("OK Remapping disabled for {name} only.\n"),
                Err(error) => format!("ERR {error}\n"),
            }
        }
    } else {
        "ERR Unknown request. Use STATUS, APPLY <source-layout> <target-layout> <controller-id>, or DISABLE <controller-id>.\n"
            .to_owned()
    };

    stream.write_all(response.as_bytes())?;
    Ok(())
}

fn apply_profile(
    id: &str,
    source_layout: Layout,
    target_layout: Layout,
    settings: &Arc<Mutex<Settings>>,
    status: &Arc<Mutex<ServiceStatus>>,
) -> Result<String, String> {
    let (name, id, path) = status
        .lock()
        .map_err(|_| "Service status is unavailable".to_owned())?
        .devices
        .iter()
        .find(|(_, device)| device.id == id)
        .map(|(path, device)| (device.name.clone(), device.id.clone(), path.clone()))
        .ok_or_else(|| {
            "The selected controller is not currently connected or compatible.".to_owned()
        })?;
    if identity::selector_from_id(&id).is_none() {
        return Err(
            "This controller's name contains characters that cannot be matched safely, so Switch Swap cannot hide it from games."
                .to_owned(),
        );
    }
    let hidraw = identity::hidraw_match(Path::new(&path));

    let current = settings
        .lock()
        .map_err(|_| "Service settings are unavailable".to_owned())?
        .clone();
    let updated = with_profile(&current, &id, &name, source_layout, target_layout, hidraw);
    if !profile_has_safe_isolation(&updated, &id) {
        return Err(
            "This controller shares its isolation identity with another saved controller; refusing to hide both."
                .to_owned(),
        );
    }
    save_settings(&updated).map_err(|error| format!("Cannot save persistent settings: {error}"))?;
    if let Err(error) = refresh_isolation_rules(&updated, Some(&current)) {
        save_settings(&current).map_err(|rollback| {
            format!("Isolation setup failed ({error}); cannot restore settings: {rollback}")
        })?;
        refresh_isolation_rules(&current, Some(&updated)).map_err(|rollback| {
            format!("Isolation setup failed ({error}); cannot restore isolation rules: {rollback}")
        })?;
        return Err(format!("Cannot isolate the physical controller: {error}"));
    }
    *settings
        .lock()
        .map_err(|_| "Service settings are unavailable".to_owned())? = updated;
    Ok(name)
}

fn with_profile(
    settings: &Settings,
    id: &str,
    name: &str,
    source_layout: Layout,
    target_layout: Layout,
    hidraw: Option<String>,
) -> Settings {
    let mut updated = settings.clone();
    updated.legacy_global_setting = false;
    let mut learned = updated
        .profiles
        .get(id)
        .map(|profile| profile.hidraw.clone())
        .unwrap_or_default();
    learned.extend(hidraw);
    updated.profiles.insert(
        id.to_owned(),
        ControllerProfile {
            name: name.to_owned(),
            source_layout,
            target_layout,
            hidraw: learned,
        },
    );
    updated
}

/// Brings a connected controller's saved profile up to date: moves a profile
/// saved under its 0.1.x identity to the current one and records how to match
/// its hidraw node. Returns whether anything changed.
fn update_connected_profile(settings: &mut Settings, controller: &DiscoveredController) -> bool {
    let mut changed = false;
    if !settings.profiles.contains_key(&controller.id)
        && let Some(profile) = settings.profiles.remove(&controller.legacy_id)
    {
        settings.profiles.insert(controller.id.clone(), profile);
        changed = true;
    }
    if let (Some(profile), Some(matcher)) = (
        settings.profiles.get_mut(&controller.id),
        controller.hidraw.as_ref(),
    ) && profile.hidraw.insert(matcher.clone())
    {
        changed = true;
    }
    changed
}

fn disable_profile(id: &str, settings: &Arc<Mutex<Settings>>) -> Result<String, String> {
    let current = settings
        .lock()
        .map_err(|_| "Service settings are unavailable".to_owned())?
        .clone();
    let mut updated = current.clone();
    updated.legacy_global_setting = false;
    let profile = updated
        .profiles
        .remove(id)
        .ok_or_else(|| "The selected controller has no saved remapping to undo.".to_owned())?;
    save_settings(&updated).map_err(|error| format!("Cannot save persistent settings: {error}"))?;
    if let Err(error) = refresh_isolation_rules(&updated, Some(&current)) {
        save_settings(&current).map_err(|rollback| {
            format!(
                "Cannot release controller isolation ({error}); cannot restore settings: {rollback}"
            )
        })?;
        refresh_isolation_rules(&current, Some(&updated)).map_err(|rollback| {
            format!("Cannot release controller isolation ({error}); cannot restore isolation rules: {rollback}")
        })?;
        return Err(format!(
            "Cannot release physical controller isolation: {error}"
        ));
    }
    *settings
        .lock()
        .map_err(|_| "Service settings are unavailable".to_owned())? = updated;
    Ok(profile.name)
}

fn peer_uid(stream: &UnixStream) -> Result<u32, std::io::Error> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { credentials.assume_init() }.uid)
}

fn make_status(settings: &Arc<Mutex<Settings>>, status: &Arc<Mutex<ServiceStatus>>) -> String {
    let Ok(settings) = settings.lock() else {
        return "ERR Service settings are unavailable.\n".to_owned();
    };
    let Ok(status) = status.lock() else {
        return "ERR Service status is unavailable.\n".to_owned();
    };

    let mut output = String::from("OK\nprotocol=4\n");
    if let Some(message) = &status.message {
        output.push_str(&format!("message={}\n", clean_field(message)));
    }
    if settings.legacy_global_setting {
        output.push_str(
            "warning=Previous global remapping is disabled for safety. Configure controllers individually.\n",
        );
    }
    output.push_str(&format!("repair_required={}\n", status.repair_required));
    output.push_str(&format!("version={}\n", env!("CARGO_PKG_VERSION")));
    let mut devices = status.devices.values().collect::<Vec<_>>();
    devices.sort_by(|left, right| left.id.cmp(&right.id));
    for device in devices {
        output.push_str(&format!(
            "device={}|{}|{}|{}\n",
            device.id,
            clean_field(&device.name),
            device.active,
            device.error.as_deref().map(clean_field).unwrap_or_default()
        ));
    }
    let mut profiles = settings.profiles.iter().collect::<Vec<_>>();
    profiles.sort_by(|left, right| left.0.cmp(right.0));
    for (id, profile) in profiles {
        output.push_str(&format!(
            "profile={}|{}|{}|{}\n",
            id,
            profile.source_layout.as_str(),
            profile.target_layout.as_str(),
            clean_field(&profile.name)
        ));
    }
    let mut errors = status.errors.values().collect::<Vec<_>>();
    errors.sort();
    for error in errors {
        output.push_str(&format!("error={}\n", clean_field(error)));
    }
    let mut candidates = status.candidates.clone();
    candidates.sort();
    for (name, reason) in candidates {
        output.push_str(&format!(
            "candidate={}|{}\n",
            clean_field(&name),
            clean_field(&reason)
        ));
    }
    output
}

fn clean_field(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '|' => ' ',
            other => other,
        })
        .collect()
}

fn valid_device_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Discovery {
    fn message(&self, max_controllers: usize, legacy_global_setting: bool) -> String {
        if legacy_global_setting {
            return "A previous global remapping was disabled for safety. Select a controller to configure it individually."
                .to_owned();
        }
        if self.controllers_count() > 0 {
            return format!(
                "Found {} compatible controller(s). Select one; layout changes affect only that controller.",
                self.controllers_count().min(max_controllers)
            );
        }
        if !self.candidates.is_empty() {
            let examples = self
                .candidates
                .iter()
                .take(3)
                .map(|(name, reason)| format!("{name}: {reason}"))
                .collect::<Vec<_>>()
                .join("; ");
            return format!(
                "Scanned {} input event devices, but none has all four face buttons and X/Y axes. {examples}",
                self.scanned
            );
        }

        let visible = if self.names.is_empty() {
            "no input device names were readable".to_owned()
        } else {
            format!("visible devices include {}", self.names.join(", "))
        };
        let scan_errors = if self.open_errors.is_empty() {
            String::new()
        } else {
            format!(
                " Some devices could not be inspected: {}.",
                self.open_errors.join("; ")
            )
        };
        format!(
            "Scanned {} /dev/input/event* nodes; the kernel exposed no gamepad-like interface ({visible}). A Bluetooth device can show as connected even when its HID driver fails to create a gamepad event device; inspect the kernel log with journalctl -k.{scan_errors}",
            self.scanned
        )
    }

    fn controllers_count(&self) -> usize {
        self.controllers.len()
    }
}

fn discover_controllers() -> Result<Discovery, std::io::Error> {
    let mut discovery = Discovery::default();
    for entry in fs::read_dir("/dev/input")? {
        let entry = entry?;
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"))
        {
            continue;
        }
        discovery.scanned += 1;

        let device = match Device::open(&path) {
            Ok(device) => device,
            Err(error) => {
                if error.kind() == std::io::ErrorKind::PermissionDenied {
                    discovery.access_denied = true;
                }
                if discovery.open_errors.len() < 3 {
                    discovery.open_errors.push(format!(
                        "{}: {error}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                continue;
            }
        };
        if identity::is_virtual_device(&path)
            || device.name().is_some_and(identity::is_legacy_virtual_name)
        {
            continue;
        }
        let name = device.name().unwrap_or("Unnamed input device").to_owned();
        if discovery.names.len() < 5 {
            discovery.names.push(name.clone());
        }
        match controller_compatibility(&device) {
            ControllerCompatibility::Compatible => {
                discovery.controllers.push(DiscoveredController {
                    id: identity::identify(&device).encode(),
                    legacy_id: identity::legacy_identity(&path, &device),
                    hidraw: identity::hidraw_match(&path),
                    path,
                    name,
                });
            }
            ControllerCompatibility::Unsupported(reason) => {
                discovery.candidates.push((
                    format!(
                        "{} ({})",
                        name,
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    reason,
                ));
            }
            ControllerCompatibility::NotController => {}
        }
    }
    Ok(discovery)
}

enum ControllerCompatibility {
    Compatible,
    Unsupported(String),
    NotController,
}

fn controller_compatibility(device: &Device) -> ControllerCompatibility {
    let keys = device.supported_keys();
    let axes = device.supported_absolute_axes();
    let face_buttons = [
        KeyCode::BTN_SOUTH,
        KeyCode::BTN_EAST,
        KeyCode::BTN_WEST,
        KeyCode::BTN_NORTH,
    ];
    let button_count = keys
        .as_ref()
        .map(|keys| {
            face_buttons
                .iter()
                .filter(|key| keys.contains(**key))
                .count()
        })
        .unwrap_or_default();
    let gamepad_controls = [
        KeyCode::BTN_TL,
        KeyCode::BTN_TR,
        KeyCode::BTN_SELECT,
        KeyCode::BTN_START,
        KeyCode::BTN_MODE,
    ];
    let has_gamepad_controls = keys.as_ref().is_some_and(|keys| {
        gamepad_controls.iter().any(|key| keys.contains(*key)) || button_count >= 2
    });
    let gamepad_axes = [
        AbsoluteAxisCode::ABS_X,
        AbsoluteAxisCode::ABS_Y,
        AbsoluteAxisCode::ABS_RX,
        AbsoluteAxisCode::ABS_RY,
        AbsoluteAxisCode::ABS_HAT0X,
        AbsoluteAxisCode::ABS_HAT0Y,
    ];
    let axis_count = axes
        .as_ref()
        .map(|axes| {
            gamepad_axes
                .iter()
                .filter(|axis| axes.contains(**axis))
                .count()
        })
        .unwrap_or_default();

    if button_count == 0 && !(has_gamepad_controls && axis_count >= 2) {
        return ControllerCompatibility::NotController;
    }

    let mut missing = face_buttons
        .iter()
        .filter(|key| !keys.as_ref().is_some_and(|keys| keys.contains(**key)))
        .map(|key| format!("{key:?}"))
        .collect::<Vec<_>>();
    for axis in [AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y] {
        if !axes.as_ref().is_some_and(|axes| axes.contains(axis)) {
            missing.push(format!("{axis:?}"));
        }
    }
    if missing.is_empty() {
        ControllerCompatibility::Compatible
    } else {
        ControllerCompatibility::Unsupported(format!(
            "missing required input codes: {}",
            missing.join(", ")
        ))
    }
}

fn controller_worker(
    path: PathBuf,
    path_key: String,
    device_id: String,
    settings: Arc<Mutex<Settings>>,
    status: Arc<Mutex<ServiceStatus>>,
) {
    let mut device = match Device::open(&path) {
        Ok(device) => device,
        Err(error) => {
            set_device_error(
                &status,
                &path_key,
                format!("Cannot open controller: {error}"),
            );
            return;
        }
    };
    if let Err(error) = device.set_nonblocking(true) {
        set_device_error(
            &status,
            &path_key,
            format!("Cannot configure input reader: {error}"),
        );
        return;
    }

    let mut virtual_device = None;
    let mut grabbed = false;
    let mut virtual_target = None;
    loop {
        let current_conversion = match settings.lock() {
            Ok(settings) => settings
                .profiles
                .get(&device_id)
                .map(|profile| (profile.source_layout, profile.target_layout)),
            Err(_) => {
                set_device_error(
                    &status,
                    &path_key,
                    "Service settings lock is unavailable".to_owned(),
                );
                break;
            }
        };

        let target = current_conversion.map(|(_, target)| target);
        if grabbed && target != virtual_target {
            if let Err(error) = device.ungrab() {
                set_device_error(
                    &status,
                    &path_key,
                    format!(
                        "Cannot release controller before changing its virtual layout: {error}"
                    ),
                );
                break;
            }
            virtual_device = None;
            grabbed = false;
            virtual_target = None;
            set_device_active(&status, &path_key, false);
        }

        if let Some(target) = target
            && !grabbed
        {
            match VirtualPad::create(&device, target) {
                Ok(new_virtual) => {
                    if let Err(error) = device.grab() {
                        set_device_error(
                            &status,
                            &path_key,
                            format!("Cannot take exclusive controller access: {error}"),
                        );
                        drop(new_virtual);
                    } else {
                        virtual_device = Some(new_virtual);
                        grabbed = true;
                        virtual_target = Some(target);
                        set_device_active(&status, &path_key, true);
                    }
                }
                Err(error) => {
                    set_device_error(
                        &status,
                        &path_key,
                        format!(
                            "Cannot create virtual controller; check /dev/uinput and kernel uinput support: {error}"
                        ),
                    );
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            }
        } else if target.is_none() && grabbed {
            if let Err(error) = device.ungrab() {
                set_device_error(
                    &status,
                    &path_key,
                    format!("Cannot release controller grab: {error}"),
                );
            }
            virtual_device = None;
            grabbed = false;
            virtual_target = None;
            set_device_active(&status, &path_key, false);
        }

        match device.fetch_events() {
            Ok(events) => {
                let (source, target) = current_conversion.unwrap_or((Layout::Xbox, Layout::Xbox));
                let mapped = events
                    .map(|event| map_event(event, source, target))
                    .collect::<Vec<_>>();
                if let Some(virtual_device) = virtual_device.as_mut()
                    && let Err(error) = virtual_device.emit(mapped)
                {
                    set_device_error(
                        &status,
                        &path_key,
                        format!("Cannot forward controller input: {error}"),
                    );
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            // Unplugged or powered off: not an error, the worker just ends.
            Err(error) if error.raw_os_error() == Some(libc::ENODEV) => break,
            Err(error) => {
                set_device_error(
                    &status,
                    &path_key,
                    format!("Controller disconnected or input read failed: {error}"),
                );
                break;
            }
        }
        if let Some(virtual_device) = virtual_device.as_mut()
            && let Err(error) = virtual_device.forward_rumble(&mut device)
        {
            eprintln!("controller {path_key}: cannot forward rumble: {error}");
        }
        wait_for_input(
            &device,
            virtual_device.as_ref().map(VirtualPad::as_raw_fd),
            Duration::from_millis(100),
        );
    }

    if grabbed {
        let _ = device.ungrab();
    }
    drop(virtual_device);
}

/// Sleeps until the controller has input, a game sends rumble to the virtual
/// pad, or `timeout` passes, so neither waits on a polling delay while profile
/// changes are still noticed.
fn wait_for_input(device: &Device, virtual_fd: Option<i32>, timeout: Duration) {
    let mut poll_fds = [device.as_raw_fd()]
        .into_iter()
        .chain(virtual_fd)
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect::<Vec<_>>();
    let timeout = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    unsafe {
        libc::poll(
            poll_fds.as_mut_ptr(),
            poll_fds.len() as libc::nfds_t,
            timeout,
        );
    }
}

fn set_device_active(status: &Arc<Mutex<ServiceStatus>>, path: &str, active: bool) {
    if let Ok(mut current) = status.lock()
        && let Some(device) = current.devices.get_mut(path)
    {
        device.active = active;
        device.error = None;
    }
}

fn set_device_error(status: &Arc<Mutex<ServiceStatus>>, path: &str, error: String) {
    if let Ok(mut current) = status.lock()
        && let Some(device) = current.devices.get_mut(path)
    {
        device.active = false;
        device.error = Some(error.clone());
    }
    eprintln!("controller {path}: {error}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn status_request_is_available_to_unprivileged_gui_clients() {
        let settings = Arc::new(Mutex::new(Settings {
            profiles: HashMap::from([(
                "abcd".to_owned(),
                ControllerProfile {
                    name: "Test controller".to_owned(),
                    source_layout: Layout::Switch,
                    target_layout: Layout::Xbox,
                    hidraw: BTreeSet::new(),
                },
            )]),
            legacy_global_setting: false,
        }));
        let status = Arc::new(Mutex::new(ServiceStatus::default()));
        status.lock().unwrap().devices.insert(
            "/dev/input/event0".to_owned(),
            DeviceStatus {
                id: "abcd".to_owned(),
                name: "Test controller".to_owned(),
                active: true,
                error: None,
            },
        );
        status.lock().unwrap().repair_required = true;
        let (mut client, service) = UnixStream::pair().unwrap();
        client.write_all(b"STATUS\n").unwrap();
        handle_client(service, &settings, &status).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        assert!(response.starts_with("OK\nprotocol=4\n"));
        assert!(response.contains("device=abcd|Test controller|true|"));
        assert!(response.contains("profile=abcd|switch|xbox|Test controller"));
        assert!(response.contains("repair_required=true\n"));
    }

    #[test]
    fn malformed_control_requests_do_not_change_settings() {
        let settings = Arc::new(Mutex::new(Settings::default()));
        let status = Arc::new(Mutex::new(ServiceStatus::default()));
        let (mut client, service) = UnixStream::pair().unwrap();
        client.write_all(b"APPLY not-a-layout xbox abcd\n").unwrap();
        handle_client(service, &settings, &status).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        assert!(response.starts_with("ERR"));
        assert!(settings.lock().unwrap().profiles.is_empty());
    }

    #[test]
    fn normal_user_cannot_apply_a_system_wide_mapping() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let settings = Arc::new(Mutex::new(Settings::default()));
        let status = Arc::new(Mutex::new(ServiceStatus::default()));
        let (mut client, service) = UnixStream::pair().unwrap();
        client.write_all(b"APPLY xbox switch abcd\n").unwrap();
        handle_client(service, &settings, &status).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        assert!(response.contains("requires administrator authorization"));
        assert!(settings.lock().unwrap().profiles.is_empty());
    }

    #[test]
    fn controller_ids_are_hex_and_bounded() {
        assert!(valid_device_id("02af"));
        assert!(!valid_device_id(""));
        assert!(!valid_device_id("not-hex"));
        assert!(!valid_device_id(&"a".repeat(513)));
    }

    #[test]
    fn applying_a_profile_does_not_change_other_controllers() {
        let settings = Settings {
            profiles: HashMap::from([(
                "first".to_owned(),
                ControllerProfile {
                    name: "First controller".to_owned(),
                    source_layout: Layout::Xbox,
                    target_layout: Layout::Switch,
                    hidraw: BTreeSet::new(),
                },
            )]),
            legacy_global_setting: true,
        };
        let updated = with_profile(
            &settings,
            "second",
            "Second controller",
            Layout::Switch,
            Layout::Xbox,
            Some("uniq=AA".to_owned()),
        );
        assert_eq!(updated.profiles.len(), 2);
        assert!(updated.profiles["second"].hidraw.contains("uniq=AA"));
        assert_eq!(updated.profiles["first"].target_layout, Layout::Switch);
        assert_eq!(updated.profiles["second"].source_layout, Layout::Switch);
        assert_eq!(updated.profiles["second"].target_layout, Layout::Xbox);
        assert!(!updated.legacy_global_setting);
    }

    #[test]
    fn legacy_global_profiles_are_not_imported_as_controller_profiles() {
        let settings = parse_settings("enabled=true\nlayout=switch\n").unwrap();
        assert!(settings.legacy_global_setting);
        assert!(settings.profiles.is_empty());
    }

    #[test]
    fn version_two_settings_restore_profiles_by_controller_id() {
        let settings = parse_settings("version=2\ndevice=abcd|switch|Pro Controller\n").unwrap();
        assert!(!settings.legacy_global_setting);
        assert_eq!(settings.profiles.len(), 1);
        assert_eq!(settings.profiles["abcd"].source_layout, Layout::Xbox);
        assert_eq!(settings.profiles["abcd"].target_layout, Layout::Switch);
        assert_eq!(settings.profiles["abcd"].name, "Pro Controller");
    }

    #[test]
    fn version_three_settings_restore_both_conversion_directions() {
        let settings =
            parse_settings("version=3\ndevice=abcd|switch|xbox|Pro Controller\n").unwrap();
        assert_eq!(settings.profiles["abcd"].source_layout, Layout::Switch);
        assert_eq!(settings.profiles["abcd"].target_layout, Layout::Xbox);
    }

    #[test]
    fn version_four_settings_keep_learned_hidraw_matches() {
        let settings = parse_settings(
            "version=4\ndevice=abcd|switch|xbox|Pro Controller\nhidraw=abcd|uniq=000000000001\n",
        )
        .unwrap();
        assert!(
            settings.profiles["abcd"]
                .hidraw
                .contains("uniq=000000000001")
        );
    }

    #[test]
    fn connected_controllers_move_legacy_profiles_and_learn_hidraw() {
        let mut settings = parse_settings("version=3\ndevice=0011|switch|xbox|Pro\n").unwrap();
        let controller = DiscoveredController {
            path: PathBuf::from("/dev/input/event9"),
            id: "2233".to_owned(),
            legacy_id: "0011".to_owned(),
            hidraw: Some("uniq=e4:17".to_owned()),
            name: "Pro".to_owned(),
        };
        assert!(update_connected_profile(&mut settings, &controller));
        assert!(!settings.profiles.contains_key("0011"));
        assert!(settings.profiles["2233"].hidraw.contains("uniq=e4:17"));
        assert!(!update_connected_profile(&mut settings, &controller));
    }
}
