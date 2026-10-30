//! A bug report anyone can paste into a GitHub issue: versions, system,
//! service state, the controllers Linux sees, and the service's recent log.
//! Shown by `switch-swap report` and copied by the window's "Copy bug report".
//! MAC addresses and serial numbers are masked.

use std::fmt::Write as _;
use std::fs;
use std::process::Command;

pub const ISSUES_URL: &str =
    "https://github.com/sweatshopking/switch-swap/issues/new?template=bug_report.yml";

pub fn generate() -> String {
    let mut report = String::new();
    let _ = writeln!(report, "### Switch Swap bug report");
    let _ = writeln!(report, "```");
    let _ = writeln!(report, "app: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(report, "install: {}", install_kind());
    let _ = writeln!(report, "system: {}", os_name());
    let _ = writeln!(report, "kernel: {}", run("uname", &["-r"]).trim());
    let _ = writeln!(
        report,
        "desktop: {} ({})",
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown".into()),
        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into())
    );
    let _ = writeln!(
        report,
        "service: {}",
        run("systemctl", &["is-active", "switch-swap.service"]).trim()
    );
    let _ = writeln!(report, "uinput: {}", describe_path("/dev/uinput"));
    let _ = writeln!(report, "```");

    section(&mut report, "Service status", &service_status());
    section(&mut report, "Controllers Linux sees", &controllers());
    section(
        &mut report,
        "Hiding rules",
        &fs::read_to_string("/etc/udev/rules.d/72-switch-swap-isolation.rules")
            .unwrap_or_else(|_| "(none)".into()),
    );
    section(
        &mut report,
        "Service log (last 60 lines)",
        &run(
            "journalctl",
            &[
                "-u",
                "switch-swap.service",
                "-n",
                "60",
                "--no-pager",
                "-o",
                "short-iso",
            ],
        ),
    );
    section(
        &mut report,
        "Kernel messages about controllers",
        &kernel_messages(),
    );
    mask_identifiers(&report)
}

fn section(report: &mut String, title: &str, body: &str) {
    let body = body.trim();
    let _ = writeln!(report, "\n<details><summary>{title}</summary>\n\n```");
    let _ = writeln!(report, "{}", if body.is_empty() { "(empty)" } else { body });
    let _ = writeln!(report, "```\n</details>");
}

fn run(program: &str, arguments: &[&str]) -> String {
    match Command::new(program).args(arguments).output() {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            let errors = String::from_utf8_lossy(&output.stderr);
            if !errors.trim().is_empty() {
                text.push_str(&errors);
            }
            text
        }
        Err(error) => format!("(cannot run {program}: {error})"),
    }
}

fn os_name() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|release| {
            release.lines().find_map(|line| {
                Some(
                    line.strip_prefix("PRETTY_NAME=")?
                        .trim_matches('"')
                        .to_owned(),
                )
            })
        })
        .unwrap_or_else(|| "unknown".into())
}

fn install_kind() -> String {
    if std::env::var_os("APPIMAGE").is_some() {
        "AppImage (this window)".into()
    } else if std::path::Path::new("/opt/switch-swap/switch-swap.AppImage").exists() {
        "AppImage (service in /opt)".into()
    } else if std::path::Path::new("/var/lib/switch-swap/switch-swap.AppImage").exists() {
        "AppImage (service in /var/lib)".into()
    } else if std::path::Path::new("/usr/lib/systemd/system/switch-swap.service").exists() {
        "package (.deb/.rpm)".into()
    } else if std::path::Path::new("/etc/systemd/system/switch-swap.service").exists() {
        "install.sh or in-app install".into()
    } else {
        "service not installed".into()
    }
}

fn describe_path(path: &str) -> String {
    match fs::metadata(path) {
        Ok(_) => "present".into(),
        Err(error) => format!("missing ({error})"),
    }
}

fn service_status() -> String {
    use std::io::{Read, Write};
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(crate::SOCKET_PATH) else {
        return "(service not reachable)".into();
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let mut response = String::new();
    if stream.write_all(b"STATUS\n").is_err() || stream.read_to_string(&mut response).is_err() {
        return "(no answer from the service)".into();
    }
    // Controller ids are the hex-encoded identity; decode them so they are readable.
    response
        .lines()
        .map(|line| match line.split_once('=') {
            Some((key @ ("device" | "profile"), value)) => {
                let (id, rest) = value.split_once('|').unwrap_or((value, ""));
                format!(
                    "{key}={}|{rest}",
                    decode_hex(id).unwrap_or_else(|| id.to_owned())
                )
            }
            _ => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_hex(value: &str) -> Option<String> {
    let bytes = (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(value.get(index..index + 2)?, 16).ok())
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}

/// Every input device with buttons or axes, as the kernel lists them.
fn controllers() -> String {
    let devices = fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
    devices
        .split("\n\n")
        .filter(|block| {
            block.lines().any(|line| {
                line.starts_with("H: Handlers=") && (line.contains("js") || line.contains("event"))
            }) && block.lines().any(|line| line.starts_with("B: ABS="))
        })
        .map(|block| {
            block
                .lines()
                .filter(|line| {
                    ["I:", "N:", "P:", "U:", "H:", "B: KEY=", "B: ABS=", "B: FF="]
                        .iter()
                        .any(|prefix| line.starts_with(prefix))
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn kernel_messages() -> String {
    let log = run("journalctl", &["-k", "-b", "--no-pager", "-o", "short-iso"]);
    let wanted = [
        "nintendo",
        "xpad",
        "hid-",
        "hid_",
        "input:",
        "gamepad",
        "controller",
        "joystick",
        "bluetooth",
        "uinput",
        "playstation",
        "sony",
    ];
    let lines = log
        .lines()
        .filter(|line| {
            let line = line.to_lowercase();
            wanted.iter().any(|word| line.contains(word))
        })
        .collect::<Vec<_>>();
    lines[lines.len().saturating_sub(60)..].join("\n")
}

/// Masks MAC addresses (aa:bb:cc:dd:ee:ff -> aa:bb:cc:xx:xx:xx) and USB serial
/// numbers that could identify someone's hardware.
fn mask_identifiers(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let characters = text.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < characters.len() {
        if let Some(length) = mac_at(&characters[index..]) {
            let mac = characters[index..index + length].iter().collect::<String>();
            output.push_str(&mac[..8]);
            output.push_str(":xx:xx:xx");
            index += length;
        } else {
            output.push(characters[index]);
            index += 1;
        }
    }
    output
}

fn mac_at(characters: &[char]) -> Option<usize> {
    const LENGTH: usize = 17;
    let candidate = characters.get(..LENGTH)?;
    let is_mac = candidate.iter().enumerate().all(|(position, character)| {
        if position % 3 == 2 {
            *character == ':' || *character == '-'
        } else {
            character.is_ascii_hexdigit()
        }
    });
    let followed_by_hex = characters.get(LENGTH).is_some_and(char::is_ascii_hexdigit);
    (is_mac && !followed_by_hex).then_some(LENGTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_addresses_are_masked() {
        assert_eq!(
            mask_identifiers("uniq=E4:17:D8:D3:AD:15 and e4-17-d8-d3-ad-15."),
            "uniq=E4:17:D8:xx:xx:xx and e4-17-d8:xx:xx:xx."
        );
        assert_eq!(mask_identifiers("12:34:56 time"), "12:34:56 time");
    }

    #[test]
    fn identities_are_decoded() {
        assert_eq!(decode_hex("76327c"), Some("v2|".to_owned()));
        assert_eq!(decode_hex("zz"), None);
    }
}
