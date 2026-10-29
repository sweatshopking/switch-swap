use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const SERVICE_UNIT_PATH: &str = "/etc/systemd/system/switch-swap.service";
const SETTINGS_PATH: &str = "/etc/switch-swap/settings.conf";
const APPIMAGE_PATH: &str = "/opt/switch-swap/switch-swap.AppImage";
const SERVICE_UNIT: &str = include_str!("../systemd/switch-swap.service");

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
        (
            appimage,
            PathBuf::from(APPIMAGE_PATH),
            SERVICE_UNIT.replace(
                "/usr/bin/switch-swap",
                "/opt/switch-swap/switch-swap.AppImage",
            ),
        )
    } else {
        (
            std::env::current_exe()?,
            PathBuf::from("/usr/bin/switch-swap"),
            SERVICE_UNIT.to_owned(),
        )
    };

    if Path::new(SERVICE_UNIT_PATH).exists()
        || Path::new("/usr/lib/systemd/system/switch-swap.service").exists()
    {
        systemctl(&["daemon-reload"])?;
        return systemctl(&["enable", "--now", "switch-swap.service"]);
    }

    install_executable(&binary_source, &binary_destination)?;
    create_default_settings()?;
    write_new_file(Path::new(SERVICE_UNIT_PATH), service_unit.as_bytes(), 0o644)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", "switch-swap.service"])?;
    println!("Switch Swap system service installed and enabled.");
    Ok(())
}

fn install_executable(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    if source.canonicalize().ok() == destination.canonicalize().ok() {
        return Ok(());
    }
    if destination.exists() {
        return Err(format!(
            "{} already exists; refusing to replace it.",
            destination.display()
        )
        .into());
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
            file.write_all(b"enabled=false\nlayout=xbox\n")?;
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
