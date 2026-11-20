use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const SERVICE_UNIT_PATH: &str = "/etc/systemd/system/switch-swap.service";
const SETTINGS_PATH: &str = "/etc/switch-swap/settings.conf";
/// Where the AppImage is copied for the service. /opt first; /var/lib on
/// systems whose /opt is read-only (SteamOS). Both survive system updates.
const APPIMAGE_PATHS: [&str; 2] = [
    "/opt/switch-swap/switch-swap.AppImage",
    "/var/lib/switch-swap/switch-swap.AppImage",
];
const DEVICE_ACCESS_DROPIN_PATH: &str =
    "/etc/systemd/system/switch-swap.service.d/10-controller-device-access.conf";
const SERVICE_UNIT: &str = include_str!("../systemd/switch-swap.service");
const APP_ID: &str = "io.github.switchswap.SwitchSwap";
const DESKTOP_ENTRY: &str = include_str!("../packaging/io.github.switchswap.SwitchSwap.desktop");
const ICON: &[u8] = include_bytes!("../packaging/io.github.switchswap.SwitchSwap.svg");
const SYMBOLIC_ICON: &[u8] =
    include_bytes!("../packaging/io.github.switchswap.SwitchSwap-symbolic.svg");
const DEVICE_ACCESS_DROPIN: &str =
    include_str!("../systemd/switch-swap.service.d/10-controller-device-access.conf");

pub fn install_service() -> Result<(), Box<dyn Error>> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(
            "Administrator authorization is required to install the system service.".into(),
        );
    }

    let appimage = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|path| path.is_file());
    let (binary_source, binary_destination, service_unit) = if let Some(appimage) = appimage {
        let destination = APPIMAGE_PATHS
            .iter()
            .map(Path::new)
            .find(|path| path.parent().is_some_and(writable_dir))
            .ok_or(
                "Neither /opt nor /var/lib can be written to, so the service cannot be installed.",
            )?;
        (
            appimage,
            destination.to_path_buf(),
            appimage_unit(SERVICE_UNIT, destination),
        )
    } else {
        (
            std::env::current_exe()?,
            PathBuf::from("/usr/bin/switch-swap"),
            SERVICE_UNIT.to_owned(),
        )
    };

    let packaged_unit = Path::new("/usr/lib/systemd/system/switch-swap.service");
    if Path::new(SERVICE_UNIT_PATH).exists() || packaged_unit.exists() {
        let unit_path = if Path::new(SERVICE_UNIT_PATH).exists() {
            Path::new(SERVICE_UNIT_PATH)
        } else {
            packaged_unit
        };
        let existing_unit = fs::read_to_string(unit_path)
            .map_err(|error| format!("Cannot read {}: {error}", unit_path.display()))?;
        let destination = if let Some(path) = APPIMAGE_PATHS
            .iter()
            .find(|path| existing_unit.contains(**path))
        {
            PathBuf::from(path)
        } else if existing_unit.contains("/usr/bin/switch-swap") {
            PathBuf::from("/usr/bin/switch-swap")
        } else {
            return Err(format!(
                "{} does not use a supported Switch Swap executable path.",
                unit_path.display()
            )
            .into());
        };
        // An install that runs the plain program (/usr/bin, from a package or install.sh) gets the plain program, never
        // the AppImage file: an AppImage can't run in the service's sandbox (no FUSE), so the service died and the
        // controller it had hidden stayed hidden (2026-11-20). The AppImage carries the plain program inside it.
        let binary_source =
            if !is_appimage_install(&destination) && is_appimage_file(&binary_source) {
                plain_program_in_appimage()?
            } else {
                binary_source
            };
        // the program that works now, kept until the new one has been seen running (see below)
        let previous = backup_path(&destination);
        let had_previous = destination.is_file() && fs::copy(&destination, &previous).is_ok();
        install_executable(&binary_source, &destination)?;
        install_menu_entry_if_possible(&destination);
        create_default_settings()?;
        let mut dropin = DEVICE_ACCESS_DROPIN.replace(
            "/usr/bin/switch-swap",
            destination
                .to_str()
                .ok_or("Invalid application install path")?,
        );
        if is_appimage_install(&destination) {
            // Installs from 0.1.x lack this; see appimage_unit.
            dropin = dropin.replace(
                "[Service]\n",
                "[Service]\nEnvironment=APPIMAGE_EXTRACT_AND_RUN=1\n",
            );
        }
        replace_file(
            Path::new(DEVICE_ACCESS_DROPIN_PATH),
            dropin.as_bytes(),
            0o644,
        )?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", "--now", "switch-swap.service"])?;
        systemctl(&["restart", "switch-swap.service"])?;
        // An update must never leave a dead service behind (2026-11-20: one did, and the controller it had hidden stayed
        // hidden). If the new program isn't running a few seconds after the restart, the previous one goes back.
        if !service_stays_up() {
            if had_previous && fs::rename(&previous, &destination).is_ok() {
                let _ = systemctl(&["reset-failed", "switch-swap.service"]);
                let _ = systemctl(&["restart", "switch-swap.service"]);
                return Err("The new version of the service did not start, so the previous version was put back. \
                            Please use Report a problem."
                    .into());
            }
            return Err(
                "The new version of the service did not start. Please use Report a problem.".into(),
            );
        }
        let _ = fs::remove_file(&previous);
        println!("Switch Swap application and system service updated.");
        return Ok(());
    }

    install_executable(&binary_source, &binary_destination)?;
    install_menu_entry_if_possible(&binary_destination);
    create_default_settings()?;
    write_new_file(Path::new(SERVICE_UNIT_PATH), service_unit.as_bytes(), 0o644)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", "switch-swap.service"])?;
    println!("Switch Swap system service installed and enabled.");
    Ok(())
}

/// The service unit for an AppImage install. The service's sandbox has no
/// FUSE (DevicePolicy=closed, NoNewPrivileges), so the AppImage unpacks itself
/// into the service's private /tmp instead of mounting.
fn appimage_unit(unit: &str, destination: &Path) -> String {
    unit.replace("/usr/bin/switch-swap", &destination.to_string_lossy())
        .replace(
            "[Service]\n",
            "[Service]\nEnvironment=APPIMAGE_EXTRACT_AND_RUN=1\n",
        )
}

fn install_executable(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    if let (Ok(source_path), Ok(destination_path)) =
        (source.canonicalize(), destination.canonicalize())
        && source_path == destination_path
    {
        return Ok(());
    }
    let parent = destination
        .parent()
        .ok_or("Application install path has no parent directory")?;
    fs::create_dir_all(parent)?;

    let temporary = parent.join(format!(".switch-swap-install-{}.tmp", std::process::id()));
    let mut source_file = File::open(source)
        .map_err(|error| format!("Cannot read application from {}: {error}", source.display()))?;
    let mut destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&temporary)
        .map_err(|error| format!("Cannot prepare {}: {error}", destination.display()))?;
    io::copy(&mut source_file, &mut destination_file)?;
    destination_file.sync_all()?;
    drop(destination_file);
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))?;
    fs::rename(&temporary, destination)
        .map_err(|error| format!("Cannot install {}: {error}", destination.display()))?;
    Ok(())
}

/// Whether the file is an AppImage (type 2: "AI" and 2 after the ELF header's first 8 bytes).
fn is_appimage_file(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 11];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && &head[..4] == b"\x7fELF"
        && &head[8..11] == b"AI\x02"
}

/// The plain program inside the running AppImage (its runtime sets APPDIR to where it is mounted).
fn plain_program_in_appimage() -> Result<PathBuf, Box<dyn Error>> {
    let program = std::env::var_os("APPDIR")
        .map(|dir| PathBuf::from(dir).join("usr/bin/switch-swap"))
        .filter(|path| path.is_file() && !is_appimage_file(path))
        .ok_or("This Switch Swap is installed as a program, and the AppImage's own copy of it could not be found. Nothing was changed.")?;
    Ok(program)
}

/// Where the program in use is kept while an update is checked.
fn backup_path(destination: &Path) -> PathBuf {
    let mut name = destination.as_os_str().to_owned();
    name.push(".previous");
    PathBuf::from(name)
}

/// The service is running a few seconds after a restart, and wasn't restarted by systemd meanwhile (a program that
/// dies at once is restarted every 2 s: one look at the right moment could catch it "active").
fn service_stays_up() -> bool {
    let state = || {
        Command::new("systemctl")
            .args(["show", "-p", "ActiveState,NRestarts", "switch-swap.service"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    std::thread::sleep(std::time::Duration::from_secs(1));
    let first = state();
    std::thread::sleep(std::time::Duration::from_secs(4));
    let second = state();
    service_state_ok(&first, &second)
}

/// From two `systemctl show -p ActiveState,NRestarts` readings, seconds apart: active both times, no restart between.
fn service_state_ok(first: &str, second: &str) -> bool {
    let field = |text: &str, key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
            .map(str::to_owned)
    };
    let active = |text: &str| field(text, "ActiveState").as_deref() == Some("active");
    active(first) && active(second) && field(first, "NRestarts") == field(second, "NRestarts")
}

fn is_appimage_install(path: &Path) -> bool {
    APPIMAGE_PATHS
        .iter()
        .any(|candidate| Path::new(candidate) == path)
}

/// Whether files can be created in `dir` (creating it if needed). False on
/// read-only system folders, such as /opt on SteamOS.
fn writable_dir(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".switch-swap-write-test-{}", std::process::id()));
    let writable = File::create(&probe).is_ok();
    let _ = fs::remove_file(probe);
    writable
}

/// The menu entry is a convenience: where /usr/local is read-only (SteamOS)
/// the service is still installed and the AppImage can be opened directly.
fn install_menu_entry_if_possible(executable: &Path) {
    if let Err(error) = install_desktop_entry(executable) {
        eprintln!("Switch Swap was installed, but its menu entry could not be added: {error}");
    }
}

/// Adds the menu entry and icons unless a distribution package already did.
/// The entry is named after the GTK application id so the desktop can match
/// the running window to its icon.
fn install_desktop_entry(executable: &Path) -> Result<(), Box<dyn Error>> {
    if Path::new(&format!("/usr/share/applications/{APP_ID}.desktop")).exists() {
        return Ok(());
    }
    let executable = executable
        .to_str()
        .ok_or("Invalid application install path")?;
    let entry = DESKTOP_ENTRY.replace("Exec=switch-swap", &format!("Exec={executable}"));
    replace_file(
        Path::new(&format!("/usr/local/share/applications/{APP_ID}.desktop")),
        entry.as_bytes(),
        0o644,
    )?;
    let icons = Path::new("/usr/local/share/icons/hicolor/scalable/apps");
    replace_file(&icons.join(format!("{APP_ID}.svg")), ICON, 0o644)?;
    replace_file(
        &icons.join(format!("{APP_ID}-symbolic.svg")),
        SYMBOLIC_ICON,
        0o644,
    )?;
    Ok(())
}

fn create_default_settings() -> Result<(), Box<dyn Error>> {
    let path = Path::new(SETTINGS_PATH);
    fs::create_dir_all(
        path.parent()
            .ok_or("Settings path has no parent directory")?,
    )?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(b"version=4\n")?;
            file.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("Cannot create {}: {error}", path.display()).into()),
    }
}

fn write_new_file(path: &Path, contents: &[u8], mode: u32) -> Result<(), Box<dyn Error>> {
    let parent = path
        .parent()
        .ok_or("Service unit path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|error| format!("Cannot create {}: {error}", path.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn replace_file(path: &Path, contents: &[u8], mode: u32) -> Result<(), Box<dyn Error>> {
    let parent = path
        .parent()
        .ok_or("Configuration path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".switch-swap-update-{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)
        .map_err(|error| format!("Cannot prepare {}: {error}", path.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("Cannot update {}: {error}", path.display()))?;
    Ok(())
}

fn systemctl(arguments: &[&str]) -> Result<(), Box<dyn Error>> {
    let output = Command::new("systemctl")
        .args(arguments)
        .output()
        .map_err(|error| format!("Cannot run systemctl: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "systemctl {} failed ({}): {}",
            arguments.join(" "),
            output.status,
            detail.trim()
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appimage_service_runs_without_fuse() {
        let unit = appimage_unit(SERVICE_UNIT, Path::new(APPIMAGE_PATHS[1]));
        assert!(unit.contains("[Service]\nEnvironment=APPIMAGE_EXTRACT_AND_RUN=1\n"));
        assert!(unit.contains("ExecStart=/var/lib/switch-swap/switch-swap.AppImage service"));
        assert!(is_appimage_install(Path::new(APPIMAGE_PATHS[0])));
        assert!(!is_appimage_install(Path::new("/usr/bin/switch-swap")));
        assert!(!unit.contains("/usr/bin/switch-swap"));
    }

    #[test]
    fn a_service_that_keeps_dying_is_not_taken_as_up() {
        assert!(service_state_ok(
            "ActiveState=active\nNRestarts=0\n",
            "ActiveState=active\nNRestarts=0\n"
        ));
        // caught "active" twice, but systemd restarted it in between: it is dying and coming back
        assert!(!service_state_ok(
            "ActiveState=active\nNRestarts=1\n",
            "ActiveState=active\nNRestarts=3\n"
        ));
        assert!(!service_state_ok(
            "ActiveState=active\nNRestarts=0\n",
            "ActiveState=failed\nNRestarts=4\n"
        ));
        assert!(!service_state_ok(
            "ActiveState=activating\nNRestarts=0\n",
            "ActiveState=active\nNRestarts=0\n"
        ));
        assert!(!service_state_ok("", ""));
        assert_eq!(
            backup_path(Path::new("/usr/bin/switch-swap")),
            PathBuf::from("/usr/bin/switch-swap.previous")
        );
    }

    #[test]
    fn an_appimage_is_told_from_a_plain_program() {
        let dir = std::env::temp_dir().join(format!("switch-swap-ai-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut ai = b"\x7fELF\x02\x01\x01\x00AI\x02".to_vec();
        ai.extend_from_slice(&[0u8; 64]);
        fs::write(dir.join("a.AppImage"), &ai).unwrap();
        let mut plain = b"\x7fELF\x02\x01\x01\x00\x00\x00\x00".to_vec();
        plain.extend_from_slice(&[0u8; 64]);
        fs::write(dir.join("plain"), &plain).unwrap();
        fs::write(dir.join("short"), b"\x7fEL").unwrap();
        assert!(is_appimage_file(&dir.join("a.AppImage")));
        assert!(!is_appimage_file(&dir.join("plain")));
        assert!(!is_appimage_file(&dir.join("short")));
        assert!(!is_appimage_file(&dir.join("missing")));
        // the real program built here is not an AppImage
        if let Ok(exe) = std::env::current_exe() {
            assert!(!is_appimage_file(&exe));
        }
        // running an AppImage over a /usr/bin install: its own plain copy is used, found through APPDIR
        fs::create_dir_all(dir.join("mnt/usr/bin")).unwrap();
        fs::write(dir.join("mnt/usr/bin/switch-swap"), &plain).unwrap();
        unsafe { std::env::set_var("APPDIR", dir.join("mnt")) };
        assert_eq!(
            plain_program_in_appimage().unwrap(),
            dir.join("mnt/usr/bin/switch-swap")
        );
        unsafe { std::env::set_var("APPDIR", dir.join("nowhere")) };
        assert!(plain_program_in_appimage().is_err());
        unsafe { std::env::remove_var("APPDIR") };
        let _ = fs::remove_dir_all(&dir);
    }
}
