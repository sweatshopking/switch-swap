use crate::mapping::{Layout, map_event};
use evdev::{
    AbsoluteAxisCode, AttributeSet, Device, EventType, KeyCode, UinputAbsSetup,
    uinput::VirtualDevice,
};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

const SOCKET_DIR: &str = "/run/switch-swap";
const SOCKET_PATH: &str = "/run/switch-swap/control.sock";
const SETTINGS_DIR: &str = "/etc/switch-swap";
const SETTINGS_PATH: &str = "/etc/switch-swap/settings.conf";
const MAX_CONTROLLERS: usize = 4;

#[derive(Clone, Default)]
struct Settings {
    profiles: HashMap<String, ControllerProfile>,
    legacy_global_setting: bool,
}

#[derive(Clone)]
struct ControllerProfile {
    name: String,
    layout: Layout,
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

    let settings = read_settings()?;
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
    let mut last_scan_error = None;

    loop {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) = handle_client(stream, &settings, &status) {
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
                }

                for controller in discovery.controllers {
                    let DiscoveredController { path, id, name } = controller;
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
                        controller_worker(path, worker_path_key, id, name, settings, status);
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
            "version" if value.trim() == "2" => {}
            "device" => {
                let fields = value.splitn(3, '|').collect::<Vec<_>>();
                if fields.len() != 3 || !valid_device_id(fields[0]) {
                    return Err(
                        format!("Invalid controller profile at line {}", line_number + 1).into(),
                    );
                }
                let layout = Layout::parse(fields[1]).ok_or_else(|| {
                    format!(
                        "Unsupported layout at line {}: {}",
                        line_number + 1,
                        fields[1]
                    )
                })?;
                settings.profiles.insert(
                    fields[0].to_owned(),
                    ControllerProfile {
                        name: fields[2].to_owned(),
                        layout,
                    },
                );
            }
            _ => return Err(format!("Unknown setting at line {}", line_number + 1).into()),
        }
    }
    Ok(settings)
}

fn save_settings(settings: &Settings) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(SETTINGS_DIR)?;
    let temporary_path = format!("{SETTINGS_PATH}.tmp");
    let mut profiles = settings.profiles.iter().collect::<Vec<_>>();
    profiles.sort_by(|left, right| left.0.cmp(right.0));
    let mut contents = String::from("version=2\n");
    for (id, profile) in profiles {
        contents.push_str(&format!(
            "device={}|{}|{}\n",
            id,
            profile.layout.as_str(),
            clean_field(&profile.name)
        ));
    }
    fs::write(&temporary_path, contents)?;
    fs::rename(temporary_path, SETTINGS_PATH)?;
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
        if fields.len() != 2 || !valid_device_id(fields[1]) {
            "ERR Usage: APPLY xbox|switch <controller-id>.\n".to_owned()
        } else if let Some(layout) = Layout::parse(fields[0]) {
            match apply_profile(fields[1], layout, settings, status) {
                Ok(name) => format!("OK {} layout saved for {name} only.\n", layout.as_str()),
                Err(error) => format!("ERR {error}\n"),
            }
        } else {
            "ERR Usage: APPLY xbox|switch <controller-id>.\n".to_owned()
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
        "ERR Unknown request. Use STATUS, APPLY xbox|switch <controller-id>, or DISABLE <controller-id>.\n"
            .to_owned()
    };

    stream.write_all(response.as_bytes())?;
    Ok(())
}

fn apply_profile(
    id: &str,
    layout: Layout,
    settings: &Arc<Mutex<Settings>>,
    status: &Arc<Mutex<ServiceStatus>>,
) -> Result<String, String> {
    let name = status
        .lock()
        .map_err(|_| "Service status is unavailable".to_owned())?
        .devices
        .values()
        .find(|device| device.id == id)
        .map(|device| device.name.clone())
        .ok_or_else(|| {
            "The selected controller is not currently connected or compatible.".to_owned()
        })?;

    let mut current = settings
        .lock()
        .map_err(|_| "Service settings are unavailable".to_owned())?;
    let updated = with_profile(&current, id, &name, layout);
    save_settings(&updated).map_err(|error| format!("Cannot save persistent settings: {error}"))?;
    *current = updated;
    Ok(name)
}

fn with_profile(settings: &Settings, id: &str, name: &str, layout: Layout) -> Settings {
    let mut updated = settings.clone();
    updated.legacy_global_setting = false;
    updated.profiles.insert(
        id.to_owned(),
        ControllerProfile {
            name: name.to_owned(),
            layout,
        },
    );
    updated
}

fn disable_profile(id: &str, settings: &Arc<Mutex<Settings>>) -> Result<String, String> {
    let mut current = settings
        .lock()
        .map_err(|_| "Service settings are unavailable".to_owned())?;
    let mut updated = current.clone();
    updated.legacy_global_setting = false;
    let profile = updated
        .profiles
        .remove(id)
        .ok_or_else(|| "The selected controller has no saved remapping to undo.".to_owned())?;
    save_settings(&updated).map_err(|error| format!("Cannot save persistent settings: {error}"))?;
    *current = updated;
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

    let mut output = String::from("OK\nprotocol=3\n");
    if let Some(message) = &status.message {
        output.push_str(&format!("message={}\n", clean_field(message)));
    }
    if settings.legacy_global_setting {
        output.push_str(
            "warning=Previous global remapping is disabled for safety. Configure controllers individually.\n",
        );
    }
    output.push_str(&format!("repair_required={}\n", status.repair_required));
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
            "profile={}|{}|{}\n",
            id,
            profile.layout.as_str(),
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
        let name = device.name().unwrap_or("Unnamed input device").to_owned();
        if discovery.names.len() < 5 {
            discovery.names.push(name.clone());
        }
        match controller_compatibility(&device) {
            ControllerCompatibility::Compatible => {
                if !name.starts_with("Switch Swap - ") {
                    let id = controller_identity(&path, &device);
                    discovery
                        .controllers
                        .push(DiscoveredController { path, id, name });
                }
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

fn controller_identity(path: &Path, device: &Device) -> String {
    let by_id = fs::read_dir("/dev/input/by-id")
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let target = fs::canonicalize(entry.path()).ok()?;
            let current = fs::canonicalize(path).ok()?;
            (target == current).then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .min_by_key(|name| {
            let priority = if name.contains("event-joystick") || name.contains("event-gamepad") {
                0
            } else {
                1
            };
            (priority, name.clone())
        });
    let (identity_kind, identity_value) = if let Some(by_id) = by_id {
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
    let raw = format!(
        "{:?}|{}|{}|{}",
        device.input_id(),
        device.name().unwrap_or_default(),
        identity_kind,
        identity_value
    );
    raw.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn controller_worker(
    path: PathBuf,
    path_key: String,
    device_id: String,
    name: String,
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
    loop {
        let current_layout = match settings.lock() {
            Ok(settings) => settings
                .profiles
                .get(&device_id)
                .map(|profile| profile.layout),
            Err(_) => {
                set_device_error(
                    &status,
                    &path_key,
                    "Service settings lock is unavailable".to_owned(),
                );
                break;
            }
        };

        if current_layout.is_some() && !grabbed {
            match create_virtual_device(&device, &name) {
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
        } else if current_layout.is_none() && grabbed {
            if let Err(error) = device.ungrab() {
                set_device_error(
                    &status,
                    &path_key,
                    format!("Cannot release controller grab: {error}"),
                );
            }
            virtual_device = None;
            grabbed = false;
            set_device_active(&status, &path_key, false);
        }

        match device.fetch_events() {
            Ok(events) => {
                let mapped = events
                    .filter(|event| {
                        event.event_type() == EventType::KEY
                            || event.event_type() == EventType::ABSOLUTE
                    })
                    .map(|event| map_event(event, current_layout.unwrap_or(Layout::Xbox)))
                    .collect::<Vec<_>>();
                if let Some(virtual_device) = virtual_device.as_mut()
                    && !mapped.is_empty()
                    && let Err(error) = virtual_device.emit(&mapped)
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
            Err(error) => {
                set_device_error(
                    &status,
                    &path_key,
                    format!("Controller disconnected or input read failed: {error}"),
                );
                break;
            }
        }
        thread::sleep(Duration::from_millis(4));
    }

    if grabbed {
        let _ = device.ungrab();
    }
    drop(virtual_device);
}

fn create_virtual_device(device: &Device, name: &str) -> Result<VirtualDevice, std::io::Error> {
    let keys: AttributeSet<KeyCode> = device
        .supported_keys()
        .into_iter()
        .flat_map(|keys| keys.iter())
        .collect();
    let virtual_name = format!("Switch Swap - {name}");
    let mut builder = VirtualDevice::builder()?
        .name(&virtual_name)
        .input_id(device.input_id())
        .with_keys(&keys)?;

    for (axis, info) in device.get_absinfo()? {
        builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
    }
    builder.build()
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
                    layout: Layout::Switch,
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

        assert!(response.starts_with("OK\nprotocol=3\n"));
        assert!(response.contains("device=abcd|Test controller|true|"));
        assert!(response.contains("profile=abcd|switch|Test controller"));
        assert!(response.contains("repair_required=true\n"));
    }

    #[test]
    fn malformed_control_requests_do_not_change_settings() {
        let settings = Arc::new(Mutex::new(Settings::default()));
        let status = Arc::new(Mutex::new(ServiceStatus::default()));
        let (mut client, service) = UnixStream::pair().unwrap();
        client.write_all(b"APPLY not-a-layout abcd\n").unwrap();
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
        client.write_all(b"APPLY switch abcd\n").unwrap();
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
                    layout: Layout::Switch,
                },
            )]),
            legacy_global_setting: true,
        };
        let updated = with_profile(&settings, "second", "Second controller", Layout::Xbox);
        assert_eq!(updated.profiles.len(), 2);
        assert_eq!(updated.profiles["first"].layout, Layout::Switch);
        assert_eq!(updated.profiles["second"].layout, Layout::Xbox);
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
        assert_eq!(settings.profiles["abcd"].layout, Layout::Switch);
        assert_eq!(settings.profiles["abcd"].name, "Pro Controller");
    }
}
