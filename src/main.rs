mod daemon;
mod gui;
mod identity;
mod installer;
mod mapping;
mod report;
mod update;
mod virtual_pad;

use std::error::Error;
use std::os::unix::net::UnixStream;
use std::time::Duration;

const SOCKET_PATH: &str = "/run/switch-swap/control.sock";

fn main() {
    if let Err(error) = run() {
        eprintln!("switch-swap: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("service") => daemon::run(),
        Some("restore-isolation") => daemon::restore_isolation(),
        Some("install-service") => {
            if args.next().is_some() {
                return Err("Usage: switch-swap install-service".into());
            }
            installer::install_service()
        }
        Some("apply") => {
            let source = args.next().ok_or(
                "Usage: switch-swap apply <source-layout> <target-layout> <controller-id>",
            )?;
            let target = args.next().ok_or(
                "Usage: switch-swap apply <source-layout> <target-layout> <controller-id>",
            )?;
            let controller = args.next().ok_or(
                "Usage: switch-swap apply <source-layout> <target-layout> <controller-id>",
            )?;
            if args.next().is_some() {
                return Err(
                    "Usage: switch-swap apply <source-layout> <target-layout> <controller-id>"
                        .into(),
                );
            }
            control(&format!("APPLY {source} {target} {controller}"))
        }
        Some("disable") => {
            let controller = args
                .next()
                .ok_or("Usage: switch-swap disable <controller-id>")?;
            if args.next().is_some() {
                return Err("Usage: switch-swap disable <controller-id>".into());
            }
            control(&format!("DISABLE {controller}"))
        }
        Some("report") => {
            print!("{}", report::generate());
            Ok(())
        }
        Some("--version") | Some("-V") => {
            println!("switch-swap {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(argument) => Err(format!("Unknown command: {argument}").into()),
        None => gui::run(),
    }
}

fn control(command: &str) -> Result<(), Box<dyn Error>> {
    let mut stream = UnixStream::connect(SOCKET_PATH).map_err(|error| {
        format!(
            "Cannot reach the system service at {SOCKET_PATH}: {error}. Is switch-swap.service installed and running?"
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    std::io::Write::write_all(&mut stream, format!("{command}\n").as_bytes())?;
    let mut response = String::new();
    std::io::Read::read_to_string(&mut stream, &mut response)?;
    if response.starts_with("OK") {
        println!("{}", response.trim());
        Ok(())
    } else {
        Err(response.trim().to_owned().into())
    }
}
