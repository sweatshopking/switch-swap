use crate::SOCKET_PATH;
use gtk::glib;
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow, Button, Label, Orientation};
use std::cell::RefCell;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let application = Application::builder()
        .application_id("io.github.switchswap.SwitchSwap")
        .build();
    application.connect_activate(build_window);
    application.run();
    Ok(())
}

fn build_window(application: &Application) {
    let status = Label::new(Some("Checking the system remapping service…"));
    status.set_wrap(true);
    status.set_xalign(0.0);
    let details = Label::new(Some(""));
    details.set_wrap(true);
    details.set_xalign(0.0);

    let xbox = Button::with_label("Use Xbox button layout");
    let switch = Button::with_label("Use Nintendo Switch button layout");
    let undo = Button::with_label("Undo remapping");
    let action_message = Rc::new(RefCell::new(String::new()));
    let pending_child = Rc::new(RefCell::new(None::<Child>));
    let executable = std::env::current_exe().ok();

    for (button, action, label) in [
        (xbox.clone(), "apply", "xbox"),
        (switch.clone(), "apply", "switch"),
        (undo.clone(), "disable", ""),
    ] {
        let action_message = Rc::clone(&action_message);
        let pending_child = Rc::clone(&pending_child);
        let executable = executable.clone();
        button.connect_clicked(move |_| {
            if pending_child.borrow().is_some() {
                *action_message.borrow_mut() = "An administrator request is already in progress.".to_owned();
                return;
            }
            let Some(executable) = executable.as_ref() else {
                *action_message.borrow_mut() = "Cannot determine the application executable path.".to_owned();
                return;
            };

            let mut command = Command::new("pkexec");
            command.arg(executable).arg(action);
            if !label.is_empty() {
                command.arg(label);
            }
            match command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
                Ok(child) => {
                    *pending_child.borrow_mut() = Some(child);
                    *action_message.borrow_mut() =
                        "Waiting for administrator authorization…".to_owned();
                }
                Err(error) => {
                    *action_message.borrow_mut() = format!(
                        "Cannot start pkexec: {error}. Install polkit and its desktop authentication agent."
                    );
                }
            }
        });
    }

    let content = gtk::Box::new(Orientation::Vertical, 12);
    content.set_margin_top(20);
    content.set_margin_bottom(20);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.append(&Label::new(Some("Switch Swap")));
    content.append(&status);
    content.append(&details);
    content.append(&xbox);
    content.append(&switch);
    content.append(&undo);
    content.append(&Label::new(Some(
        "Applies to all users on this machine. Connecting and disconnecting controllers is handled automatically.",
    )));

    let window = ApplicationWindow::builder()
        .application(application)
        .title("Switch Swap")
        .default_width(420)
        .default_height(300)
        .child(&content)
        .build();
    window.present();

    let status_label = status.clone();
    let details_label = details.clone();
    glib::timeout_add_local(Duration::from_secs(1), move || {
        if let Some(child) = pending_child.borrow_mut().as_mut() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    let message = if exit.success() {
                        "Administrator request completed.".to_owned()
                    } else {
                        format!("Administrator request ended with {exit}.")
                    };
                    *action_message.borrow_mut() = message;
                    *pending_child.borrow_mut() = None;
                }
                Ok(None) => {}
                Err(error) => {
                    *action_message.borrow_mut() =
                        format!("Cannot check administrator request: {error}");
                    *pending_child.borrow_mut() = None;
                }
            }
        }

        match read_status() {
            Ok((service, controller_details)) => {
                status_label.set_text(&format!("{} {}", service, action_message.borrow()));
                details_label.set_text(&controller_details);
            }
            Err(error) => {
                status_label.set_text(&format!("{} {}", error, action_message.borrow()));
                details_label.set_text(
                    "If this is a fresh setup, run the included install.sh with sudo, then reopen the app.",
                );
            }
        }
        glib::ControlFlow::Continue
    });
}

fn read_status() -> Result<(String, String), String> {
    let mut stream = UnixStream::connect(SOCKET_PATH).map_err(|error| {
        format!("System service unavailable ({error}). Install and start switch-swap.service.")
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("Cannot set service timeout: {error}"))?;
    stream
        .write_all(b"STATUS\n")
        .map_err(|error| format!("Cannot query system service: {error}"))?;
    let mut body = String::new();
    BufReader::new(stream)
        .read_to_string(&mut body)
        .map_err(|error| format!("Cannot read system service status: {error}"))?;
    if !body.starts_with("OK") {
        return Err(body.trim().to_owned());
    }
    parse_status(&body)
}

fn parse_status(body: &str) -> Result<(String, String), String> {
    let mut enabled = false;
    let mut layout = "xbox";
    let mut devices = Vec::new();
    let mut message = None;
    for line in body.lines().skip(1) {
        if let Some(value) = line.strip_prefix("enabled=") {
            enabled = value == "true";
        } else if let Some(value) = line.strip_prefix("layout=") {
            layout = value;
        } else if let Some(value) = line.strip_prefix("message=") {
            message = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("device=") {
            let fields = value.splitn(3, '|').collect::<Vec<_>>();
            if fields.len() == 3 {
                let state = match (fields[1] == "true", fields[2].is_empty()) {
                    (true, _) => "active",
                    (false, true) => "waiting",
                    (false, false) => fields[2],
                };
                devices.push(format!("• {} — {state}", fields[0]));
            }
        }
    }

    let service = if enabled {
        format!(
            "Remapping is enabled ({}) system-wide.",
            if layout == "switch" {
                "Nintendo Switch"
            } else {
                "Xbox"
            }
        )
    } else {
        "Remapping is disabled; physical controller input is unchanged.".to_owned()
    };
    let mut details = if devices.is_empty() {
        "No compatible controllers detected. Check that the kernel driver exposes the device under /dev/input and that it has face buttons and X/Y axes.".to_owned()
    } else {
        devices.join("\n")
    };
    if let Some(message) = message {
        details.push('\n');
        details.push_str(&message);
    }
    Ok((service, details))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parser_reports_enabled_layout_and_devices() {
        let (service, details) =
            parse_status("OK\nenabled=true\nlayout=switch\ndevice=Controller One|true||\n")
                .unwrap();
        assert!(service.contains("Nintendo Switch"));
        assert!(details.contains("Controller One — active"));
    }

    #[test]
    fn status_parser_exposes_service_diagnostics() {
        let (service, details) =
            parse_status("OK\nenabled=false\nlayout=xbox\nmessage=Cannot scan /dev/input\n")
                .unwrap();
        assert!(service.contains("disabled"));
        assert!(details.contains("Cannot scan /dev/input"));
    }
}
