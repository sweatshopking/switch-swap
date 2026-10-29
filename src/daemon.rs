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

#[derive(Clone, Copy)]
struct Settings {
    enabled: bool,
    layout: Layout,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            layout: Layout::Xbox,
        }
    }
}

#[derive(Clone, Default)]
struct DeviceStatus {
    name: String,
    active: bool,
    error: Option<String>,
}

#[derive(Default)]
struct ServiceStatus {
    devices: HashMap<String, DeviceStatus>,
    message: Option<String>,
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
            {
                current.message = Some(match device.error {
                    Some(error) => format!("{}: {error}", device.name),
                    None => format!("{} disconnected.", device.name),
                });
            }
        }

        match discover_controllers() {
            Ok(devices) => {
                last_scan_error = None;
                for (path, name) in devices {
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
                        current.message = None;
                        current.devices.insert(
                            path_key.clone(),
                            DeviceStatus {
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
                        controller_worker(path, worker_path_key, name, settings, status);
                        let _ = disconnected.send(path_key);
                    });
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

    let mut settings = Settings::default();
    for (line_number, line) in contents.lines().enumerate() {
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("Invalid settings at line {}", line_number + 1).into());
        };
        match key.trim() {
            "enabled" => {
                settings.enabled = match value.trim() {
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
                settings.layout = Layout::parse(value.trim()).ok_or_else(|| {
                    format!(
                        "Unsupported layout at line {}: {}",
                        line_number + 1,
                        value.trim()
                    )
                })?;
            }
            _ => return Err(format!("Unknown setting at line {}", line_number + 1).into()),
        }
    }
    Ok(settings)
}

fn save_settings(settings: Settings) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(SETTINGS_DIR)?;
    let temporary_path = format!("{SETTINGS_PATH}.tmp");
    fs::write(
        &temporary_path,
        format!(
            "enabled={}\nlayout={}\n",
            settings.enabled,
            settings.layout.as_str()
        ),
    )?;
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
        "ERR Applying or undoing a system-wide layout requires administrator authorization.\n"
            .to_owned()
    } else if let Some(layout) = request.strip_prefix("APPLY ").and_then(Layout::parse) {
        let new_settings = Settings {
            enabled: true,
            layout,
        };
        match save_settings(new_settings) {
            Ok(()) => {
                *settings.lock().map_err(|_| "Settings lock is poisoned")? = new_settings;
                format!("OK {} layout enabled system-wide.\n", layout.as_str())
            }
            Err(error) => format!("ERR Cannot save persistent settings: {error}\n"),
        }
    } else if request == "DISABLE" {
        let new_settings = Settings::default();
        match save_settings(new_settings) {
            Ok(()) => {
                *settings.lock().map_err(|_| "Settings lock is poisoned")? = new_settings;
                "OK Remapping disabled; physical controllers restored.\n".to_owned()
            }
            Err(error) => format!("ERR Cannot save persistent settings: {error}\n"),
        }
    } else {
        "ERR Unknown request. Use STATUS, APPLY xbox|switch, or DISABLE.\n".to_owned()
    };

    stream.write_all(response.as_bytes())?;
    Ok(())
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

    let mut output = format!(
        "OK\nenabled={}\nlayout={}\n",
        settings.enabled,
        settings.layout.as_str()
    );
    if let Some(message) = &status.message {
        output.push_str(&format!("message={}\n", clean_field(message)));
    }
    for device in status.devices.values() {
        output.push_str(&format!(
            "device={}|{}|{}\n",
            clean_field(&device.name),
            device.active,
            device.error.as_deref().map(clean_field).unwrap_or_default()
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

fn discover_controllers() -> Result<Vec<(PathBuf, String)>, std::io::Error> {
    let mut controllers = Vec::new();
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

        let device = match Device::open(&path) {
            Ok(device) => device,
            Err(_) => continue,
        };
        if is_controller(&device) {
            let name = device.name().unwrap_or("Unnamed controller").to_owned();
            if !name.starts_with("Switch Swap - ") {
                controllers.push((path, name));
            }
        }
    }
    Ok(controllers)
}

fn is_controller(device: &Device) -> bool {
    let Some(keys) = device.supported_keys() else {
        return false;
    };
    let Some(axes) = device.supported_absolute_axes() else {
        return false;
    };

    [
        KeyCode::BTN_SOUTH,
        KeyCode::BTN_EAST,
        KeyCode::BTN_WEST,
        KeyCode::BTN_NORTH,
    ]
    .iter()
    .all(|key| keys.contains(*key))
        && axes.contains(AbsoluteAxisCode::ABS_X)
        && axes.contains(AbsoluteAxisCode::ABS_Y)
}

fn controller_worker(
    path: PathBuf,
    path_key: String,
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
        let current = match settings.lock() {
            Ok(settings) => *settings,
            Err(_) => {
                set_device_error(
                    &status,
                    &path_key,
                    "Service settings lock is unavailable".to_owned(),
                );
                break;
            }
        };

        if current.enabled && !grabbed {
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
        } else if !current.enabled && grabbed {
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
                    .map(|event| map_event(event, current.layout))
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
            enabled: true,
            layout: Layout::Switch,
        }));
        let status = Arc::new(Mutex::new(ServiceStatus::default()));
        status.lock().unwrap().devices.insert(
            "/dev/input/event0".to_owned(),
            DeviceStatus {
                name: "Test controller".to_owned(),
                active: true,
                error: None,
            },
        );
        let (mut client, service) = UnixStream::pair().unwrap();
        client.write_all(b"STATUS\n").unwrap();
        handle_client(service, &settings, &status).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        assert!(response.starts_with("OK\nenabled=true\nlayout=switch\n"));
        assert!(response.contains("device=Test controller|true|"));
    }

    #[test]
    fn malformed_control_requests_do_not_change_settings() {
        let settings = Arc::new(Mutex::new(Settings::default()));
        let status = Arc::new(Mutex::new(ServiceStatus::default()));
        let (mut client, service) = UnixStream::pair().unwrap();
        client.write_all(b"APPLY not-a-layout\n").unwrap();
        handle_client(service, &settings, &status).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        assert!(response.starts_with("ERR"));
        assert!(!settings.lock().unwrap().enabled);
    }

    #[test]
    fn normal_user_cannot_apply_a_system_wide_mapping() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let settings = Arc::new(Mutex::new(Settings::default()));
        let status = Arc::new(Mutex::new(ServiceStatus::default()));
        let (mut client, service) = UnixStream::pair().unwrap();
        client.write_all(b"APPLY switch\n").unwrap();
        handle_client(service, &settings, &status).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        assert!(response.contains("requires administrator authorization"));
        assert!(!settings.lock().unwrap().enabled);
    }
}
