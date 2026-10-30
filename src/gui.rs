use crate::SOCKET_PATH;
use crate::update::{self, Preferences};
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    Application, ApplicationWindow, Button, CheckButton, DropDown, Label, Orientation, StringList,
};
use std::cell::RefCell;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

#[derive(Clone, Debug)]
struct Controller {
    id: String,
    name: String,
    connected: bool,
    conversion: Option<Conversion>,
    active: bool,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct Conversion {
    source: String,
    target: String,
}

#[derive(Debug, Default)]
struct ServiceSnapshot {
    message: Option<String>,
    warning: Option<String>,
    devices: Vec<Controller>,
    profiles: Vec<Controller>,
    candidates: Vec<String>,
    errors: Vec<String>,
    repair_required: bool,
    /// The running service's version; None for services older than 0.2.0.
    version: Option<String>,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = Application::builder().application_id("io.github.switchswap.SwitchSwap");
    // Debug builds open their own window instead of raising an installed copy.
    if cfg!(debug_assertions) {
        builder = builder.flags(gtk::gio::ApplicationFlags::NON_UNIQUE);
    }
    let application = builder.build();
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
    let setup = Label::new(Some(
        "Select a controller, then choose which controller games should see. Buttons keep their physical positions: the bottom face button is always the confirm button in Xbox-style games.",
    ));
    setup.set_wrap(true);
    setup.set_xalign(0.0);
    let install_service = Button::with_label("Install/update system service…");
    let controller_model = StringList::new(&[]);
    let controller_dropdown =
        DropDown::new(Some(controller_model.clone()), None::<gtk::Expression>);
    controller_dropdown.set_hexpand(true);
    let xbox = Button::with_label("Play as Xbox controller");
    let switch = Button::with_label("Play as Nintendo Switch controller");
    let swap_letters = CheckButton::with_label(
        "Also swap A/B and X/Y (only if this controller's buttons come out in the wrong places)",
    );
    let undo = Button::with_label("Undo selected controller");
    let action_message = Rc::new(RefCell::new(String::new()));
    let pending_child = Rc::new(RefCell::new(None::<Child>));
    let controllers = Rc::new(RefCell::new(Vec::<Controller>::new()));
    let executable = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok());

    {
        let action_message = Rc::clone(&action_message);
        let pending_child = Rc::clone(&pending_child);
        let executable = executable.clone();
        install_service.connect_clicked(move |_| {
            start_admin_request(
                executable.as_ref(),
                "install-service",
                &[],
                &pending_child,
                &action_message,
            );
        });
    }

    for (button, target) in [(xbox.clone(), "xbox"), (switch.clone(), "switch")] {
        let swap_letters = swap_letters.clone();
        let action_message = Rc::clone(&action_message);
        let pending_child = Rc::clone(&pending_child);
        let executable = executable.clone();
        let controllers = Rc::clone(&controllers);
        let dropdown = controller_dropdown.clone();
        button.connect_clicked(move |_| {
            let Some(controller) = selected_controller(&dropdown, &controllers) else {
                *action_message.borrow_mut() = "Select a connected controller first.".to_owned();
                return;
            };
            if !controller.connected {
                *action_message.borrow_mut() =
                    "The selected controller is disconnected. Reconnect it to change its layout."
                        .to_owned();
                return;
            }
            // The service swaps face buttons whenever source and target differ.
            let source = match (swap_letters.is_active(), target) {
                (false, target) => target,
                (true, "xbox") => "switch",
                (true, _) => "xbox",
            };
            start_admin_request(
                executable.as_ref(),
                "apply",
                &[source, target, &controller.id],
                &pending_child,
                &action_message,
            );
        });
    }

    {
        let action_message = Rc::clone(&action_message);
        let pending_child = Rc::clone(&pending_child);
        let executable = executable.clone();
        let controllers = Rc::clone(&controllers);
        let dropdown = controller_dropdown.clone();
        undo.connect_clicked(move |_| {
            let Some(controller) = selected_controller(&dropdown, &controllers) else {
                *action_message.borrow_mut() =
                    "Select a controller with a saved layout first.".to_owned();
                return;
            };
            if controller.conversion.is_none() {
                *action_message.borrow_mut() =
                    "The selected controller has no saved remapping to undo.".to_owned();
                return;
            }
            start_admin_request(
                executable.as_ref(),
                "disable",
                &[&controller.id],
                &pending_child,
                &action_message,
            );
        });
    }

    let content = gtk::Box::new(Orientation::Vertical, 12);
    content.set_margin_top(20);
    content.set_margin_bottom(20);
    content.set_margin_start(20);
    content.set_margin_end(20);
    let brand = gtk::Box::new(Orientation::Horizontal, 10);
    let logo = app_logo();
    brand.append(&logo);
    let title = Label::new(Some("Switch Swap"));
    title.add_css_class("title-1");
    brand.append(&title);
    content.append(&brand);
    let preferences = Rc::new(RefCell::new(Preferences::load()));
    let updates = UpdateBanner::new(Rc::clone(&preferences));
    content.append(&updates.container);
    content.append(&setup);
    content.append(&install_service);
    content.append(&status);
    let controller_heading = Label::new(Some("Controller"));
    controller_heading.set_xalign(0.0);
    controller_heading.add_css_class("heading");
    content.append(&controller_heading);
    content.append(&controller_dropdown);
    content.append(&details);
    content.append(&xbox);
    content.append(&switch);
    content.append(&swap_letters);
    content.append(&undo);
    let footer = Label::new(Some(
        "Only the selected controller is changed, and its choice is remembered for next time. Restart games that were already open.",
    ));
    footer.set_wrap(true);
    footer.set_xalign(0.0);
    content.append(&footer);
    let help_row = gtk::Box::new(Orientation::Horizontal, 12);
    help_row.append(&updates.toggle);
    let report_bug = Button::with_label("Report a problem…");
    report_bug.set_hexpand(true);
    report_bug.set_halign(gtk::Align::End);
    help_row.append(&report_bug);
    content.append(&help_row);
    {
        let action_message = Rc::clone(&action_message);
        report_bug.connect_clicked(move |button| {
            let button = button.clone();
            let action_message = Rc::clone(&action_message);
            button.set_sensitive(false);
            glib::spawn_future_local(async move {
                let report = gtk::gio::spawn_blocking(crate::report::generate)
                    .await
                    .unwrap_or_default();
                button.clipboard().set_text(&report);
                *action_message.borrow_mut() =
                    "A bug report is copied. Paste it into the form that opened in your browser."
                        .to_owned();
                if let Err(error) = gtk::gio::AppInfo::launch_default_for_uri(
                    crate::report::ISSUES_URL,
                    None::<&gtk::gio::AppLaunchContext>,
                ) {
                    *action_message.borrow_mut() = format!(
                        "A bug report is copied. Open {} and paste it there ({error}).",
                        crate::report::ISSUES_URL
                    );
                }
                button.set_sensitive(true);
            });
        });
    }

    let window = ApplicationWindow::builder()
        .application(application)
        .title("Switch Swap")
        .default_width(460)
        .default_height(500)
        .child(&content)
        .build();
    window.set_icon_name(Some("io.github.switchswap.SwitchSwap"));
    window.present();
    updates.check();
    {
        // While the window stays open, look again once a day.
        let updates = updates.clone();
        glib::timeout_add_seconds_local(24 * 60 * 60, move || {
            updates.check();
            glib::ControlFlow::Continue
        });
    }

    let status_label = status.clone();
    let details_label = details.clone();
    let setup_label = setup.clone();
    let install_button = install_service.clone();
    let dropdown = controller_dropdown.clone();
    let model = controller_model.clone();
    let controller_list = Rc::clone(&controllers);
    let xbox_button = xbox.clone();
    let switch_button = switch.clone();
    let undo_button = undo.clone();
    let action_message = Rc::clone(&action_message);
    let pending_child = Rc::clone(&pending_child);
    glib::timeout_add_local(Duration::from_secs(1), move || {
        check_pending_request(&pending_child, &action_message);

        match read_status() {
            Ok(snapshot) => {
                let previous_id = selected_controller(&dropdown, &controller_list)
                    .map(|controller| controller.id);
                let (choices, mut labels) = controller_choices(&snapshot);
                if labels.is_empty() {
                    labels.push("No compatible controllers currently detected".to_owned());
                }
                // Replacing the list closes an open dropdown, so only do it on change.
                let current = (0..model.n_items())
                    .filter_map(|index| model.string(index))
                    .map(String::from)
                    .collect::<Vec<_>>();
                if current != labels {
                    model.splice(
                        0,
                        model.n_items(),
                        &labels.iter().map(String::as_str).collect::<Vec<_>>(),
                    );
                }
                let selected = previous_id
                    .and_then(|id| choices.iter().position(|controller| controller.id == id))
                    .or_else(|| choices.iter().position(|controller| controller.connected));
                *controller_list.borrow_mut() = choices;
                dropdown.set_selected(
                    selected.map_or(gtk::INVALID_LIST_POSITION, |index| index as u32),
                );

                let action_text = action_message.borrow();
                let status_text = if action_text.is_empty() {
                    "System service is running.".to_owned()
                } else {
                    action_text.clone()
                };
                status_label.set_text(&status_text);
                let selected = selected_controller(&dropdown, &controller_list);
                let mut detail_lines = Vec::new();
                if let Some(message) = snapshot.message {
                    detail_lines.push(message);
                }
                if let Some(warning) = snapshot.warning {
                    detail_lines.push(warning);
                }
                if snapshot.repair_required {
                    detail_lines.push(
                        "The service is blocked from opening input devices. Install/update it to repair the systemd device-access rule."
                            .to_owned(),
                    );
                }
                if let Some(controller) = &selected
                    && let Some(error) = &controller.error
                {
                    detail_lines.push(format!("{}: {error}", controller.name));
                }
                detail_lines.extend(snapshot.candidates);
                detail_lines.extend(snapshot.errors);
                details_label.set_text(&detail_lines.join("\n"));
                // Only offer to replace an older service: replacing a newer one
                // with this window's copy would downgrade it.
                let window_version = env!("CARGO_PKG_VERSION");
                let outdated = snapshot
                    .version
                    .as_deref()
                    .is_none_or(|service| update::is_newer(window_version, service));
                let window_outdated = snapshot
                    .version
                    .as_deref()
                    .is_some_and(|service| update::is_newer(service, window_version));
                setup_label.set_text(if snapshot.repair_required {
                    "The system service needs an update before it can read controllers. Click Update system service."
                } else if outdated {
                    "The system service is from an older version of Switch Swap. Click Update system service to finish updating."
                } else if window_outdated {
                    "This window is from an older version than the one installed. Close it and open Switch Swap from your app menu."
                } else {
                    "Pick your controller, then choose which controller games should see."
                });
                install_button.set_label("Update system service…");
                install_button.set_visible(snapshot.repair_required || outdated);
                dropdown.set_sensitive(true);
                let connected = selected
                    .as_ref()
                    .is_some_and(|controller| controller.connected);
                let has_profile = selected
                    .as_ref()
                    .is_some_and(|controller| controller.conversion.is_some());
                xbox_button.set_sensitive(connected);
                switch_button.set_sensitive(connected);
                undo_button.set_sensitive(has_profile);
            }
            Err(error) => {
                status_label.set_text(&format!("{error} {}", action_message.borrow()));
                details_label.set_text(
                    "The installer will request administrator approval before copying files or enabling the system service.",
                );
                setup_label.set_text(
                    "First-time setup — install the service with administrator approval, then connect and select a controller.",
                );
                install_button.set_label("Install or update system service…");
                install_button.set_visible(true);
                dropdown.set_sensitive(false);
                xbox_button.set_sensitive(false);
                switch_button.set_sensitive(false);
                undo_button.set_sensitive(false);
            }
        }
        glib::ControlFlow::Continue
    });
}

/// "A new version is out" banner and the setting that turns the check off.
#[derive(Clone)]
struct UpdateBanner {
    container: gtk::Box,
    message: Label,
    toggle: CheckButton,
    offered: Rc<RefCell<Option<String>>>,
    preferences: Rc<RefCell<Preferences>>,
}

impl UpdateBanner {
    fn new(preferences: Rc<RefCell<Preferences>>) -> Self {
        let container = gtk::Box::new(Orientation::Vertical, 6);
        container.add_css_class("card");
        container.set_visible(false);
        let message = Label::new(None);
        message.set_wrap(true);
        message.set_xalign(0.0);
        let buttons = gtk::Box::new(Orientation::Horizontal, 6);
        let open = Button::with_label("Open download page");
        open.add_css_class("suggested-action");
        let skip = Button::with_label("Skip this version");
        buttons.append(&open);
        buttons.append(&skip);
        container.append(&message);
        container.append(&buttons);
        let toggle = CheckButton::with_label("Tell me when a new version is out");
        toggle.set_active(preferences.borrow().check_for_updates);
        let banner = Self {
            container,
            message,
            toggle,
            offered: Rc::new(RefCell::new(None)),
            preferences,
        };

        open.connect_clicked(|_| {
            if let Err(error) = gtk::gio::AppInfo::launch_default_for_uri(
                update::RELEASES_URL,
                None::<&gtk::gio::AppLaunchContext>,
            ) {
                eprintln!("Cannot open {}: {error}", update::RELEASES_URL);
            }
        });
        {
            let banner = banner.clone();
            skip.connect_clicked(move |_| {
                let version = banner.offered.borrow().clone();
                banner.preferences.borrow_mut().skipped_version = version;
                banner.save();
                banner.container.set_visible(false);
            });
        }
        {
            let banner = banner.clone();
            banner.toggle.clone().connect_toggled(move |toggle| {
                banner.preferences.borrow_mut().check_for_updates = toggle.is_active();
                banner.save();
                if toggle.is_active() {
                    banner.check();
                } else {
                    banner.container.set_visible(false);
                }
            });
        }
        banner
    }

    fn save(&self) {
        if let Err(error) = self.preferences.borrow().save() {
            eprintln!("Cannot save Switch Swap settings: {error}");
        }
    }

    /// Asks GitHub in the background; shows the banner if a newer version is out.
    fn check(&self) {
        if !self.preferences.borrow().check_for_updates {
            return;
        }
        let banner = self.clone();
        glib::spawn_future_local(async move {
            let Ok(Some(version)) = gtk::gio::spawn_blocking(update::newer_version).await else {
                return;
            };
            if !banner.preferences.borrow().should_offer(&version) {
                return;
            }
            banner.message.set_text(&format!(
                "Switch Swap {version} is out (you have {}). Download it and install it the same way as before.",
                env!("CARGO_PKG_VERSION")
            ));
            *banner.offered.borrow_mut() = Some(version);
            banner.container.set_visible(true);
        });
    }
}

fn start_admin_request(
    executable: Option<&PathBuf>,
    action: &str,
    arguments: &[&str],
    pending_child: &Rc<RefCell<Option<Child>>>,
    action_message: &Rc<RefCell<String>>,
) {
    if pending_child.borrow().is_some() {
        *action_message.borrow_mut() =
            "An administrator request is already in progress.".to_owned();
        return;
    }
    let Some(executable) = executable else {
        *action_message.borrow_mut() =
            "Cannot determine the application executable path.".to_owned();
        return;
    };

    match Command::new("pkexec")
        .arg(executable)
        .arg(action)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => {
            *pending_child.borrow_mut() = Some(child);
            *action_message.borrow_mut() = if action == "install-service" {
                "Review the administrator prompt to install the service…".to_owned()
            } else {
                "Waiting for administrator authorization…".to_owned()
            };
        }
        Err(error) => {
            *action_message.borrow_mut() = format!(
                "Cannot start pkexec: {error}. Install polkit and its desktop authentication agent."
            );
        }
    }
}

fn app_logo() -> gtk::Image {
    let image = gtk::Image::new();
    let logo = gtk::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(include_bytes!(
        "../packaging/io.github.switchswap.SwitchSwap.svg"
    )));
    match logo {
        Ok(pixbuf) => image.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&pixbuf))),
        Err(error) => {
            eprintln!("Cannot load the embedded Switch Swap icon: {error}");
            image.set_icon_name(Some("io.github.switchswap.SwitchSwap"));
        }
    }
    image.set_pixel_size(56);
    image
}

fn check_pending_request(
    pending_child: &Rc<RefCell<Option<Child>>>,
    action_message: &Rc<RefCell<String>>,
) {
    let child_status = pending_child.borrow_mut().as_mut().map(Child::try_wait);
    match child_status {
        Some(Ok(Some(_))) => {
            if let Some(child) = pending_child.borrow_mut().take() {
                match child.wait_with_output() {
                    Ok(output) => {
                        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                        let message = if output.status.success() {
                            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                            if stdout.is_empty() {
                                "Administrator request completed.".to_owned()
                            } else {
                                stdout
                            }
                        } else if detail.is_empty() {
                            format!("Administrator request ended with {}.", output.status)
                        } else {
                            format!("Administrator request failed: {detail}")
                        };
                        *action_message.borrow_mut() = message;
                    }
                    Err(error) => {
                        *action_message.borrow_mut() =
                            format!("Cannot read administrator request result: {error}");
                    }
                }
            }
        }
        Some(Ok(None)) | None => {}
        Some(Err(error)) => {
            *action_message.borrow_mut() = format!("Cannot check administrator request: {error}");
            pending_child.borrow_mut().take();
        }
    }
}

fn selected_controller(
    dropdown: &DropDown,
    controllers: &Rc<RefCell<Vec<Controller>>>,
) -> Option<Controller> {
    controllers
        .borrow()
        .get(dropdown.selected() as usize)
        .cloned()
}

fn controller_choices(snapshot: &ServiceSnapshot) -> (Vec<Controller>, Vec<String>) {
    let mut choices = snapshot.devices.clone();
    for mut profile in snapshot.profiles.clone() {
        if let Some(connected) = choices.iter_mut().find(|device| device.id == profile.id) {
            connected.conversion = profile.conversion.take();
        } else {
            choices.push(profile);
        }
    }
    choices.sort_by(|left, right| {
        right
            .connected
            .cmp(&left.connected)
            .then_with(|| left.name.cmp(&right.name))
    });

    let labels = choices
        .iter()
        .map(|controller| {
            let layout = match &controller.conversion {
                Some(conversion) => {
                    if conversion.source == conversion.target {
                        format!("plays as {}", layout_name(&conversion.target))
                    } else {
                        format!(
                            "plays as {}, A/B and X/Y swapped",
                            layout_name(&conversion.target)
                        )
                    }
                }
                None => "not remapped".to_owned(),
            };
            let connection = if controller.connected {
                if controller.active {
                    "active"
                } else {
                    "connected"
                }
            } else {
                "saved, disconnected"
            };
            format!("{} — {layout} ({connection})", controller.name)
        })
        .collect();
    (choices, labels)
}

fn layout_name(layout: &str) -> &str {
    match layout {
        "switch" => "Nintendo Switch",
        _ => "Xbox",
    }
}

fn read_status() -> Result<ServiceSnapshot, String> {
    // Debug builds can talk to a stand-in service, for screenshots and testing.
    let socket_path = if cfg!(debug_assertions) {
        std::env::var("SWITCH_SWAP_TEST_SOCKET").unwrap_or_else(|_| SOCKET_PATH.to_owned())
    } else {
        SOCKET_PATH.to_owned()
    };
    let mut stream = UnixStream::connect(&socket_path).map_err(|error| {
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

fn parse_status(body: &str) -> Result<ServiceSnapshot, String> {
    let mut snapshot = ServiceSnapshot::default();
    let mut current_protocol = false;
    for line in body.lines().skip(1) {
        if line == "protocol=4" {
            current_protocol = true;
        } else if let Some(value) = line.strip_prefix("message=") {
            snapshot.message = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("warning=") {
            snapshot.warning = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("device=") {
            let fields = value.splitn(4, '|').collect::<Vec<_>>();
            if fields.len() != 4 {
                return Err("The service returned a malformed controller entry.".to_owned());
            }
            snapshot.devices.push(Controller {
                id: fields[0].to_owned(),
                name: fields[1].to_owned(),
                connected: true,
                conversion: None,
                active: fields[2] == "true",
                error: (!fields[3].is_empty()).then(|| fields[3].to_owned()),
            });
        } else if let Some(value) = line.strip_prefix("profile=") {
            let fields = value.splitn(4, '|').collect::<Vec<_>>();
            if fields.len() != 4 {
                return Err("The service returned a malformed saved profile.".to_owned());
            }
            snapshot.profiles.push(Controller {
                id: fields[0].to_owned(),
                name: fields[3].to_owned(),
                connected: false,
                conversion: Some(Conversion {
                    source: fields[1].to_owned(),
                    target: fields[2].to_owned(),
                }),
                active: false,
                error: None,
            });
        } else if let Some(value) = line.strip_prefix("candidate=") {
            let fields = value.splitn(2, '|').collect::<Vec<_>>();
            if fields.len() == 2 {
                snapshot
                    .candidates
                    .push(format!("{} — {}", fields[0], fields[1]));
            }
        } else if let Some(value) = line.strip_prefix("error=") {
            snapshot.errors.push(value.to_owned());
        } else if let Some(value) = line.strip_prefix("repair_required=") {
            snapshot.repair_required = value == "true";
        } else if let Some(value) = line.strip_prefix("version=") {
            snapshot.version = Some(value.to_owned());
        }
    }
    if !current_protocol {
        return Err(
            "The system service is from an older version of Switch Swap. Click Update system service to finish updating."
                .to_owned(),
        );
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parser_lists_devices_profiles_and_diagnostics() {
        let snapshot = parse_status(
            "OK\nprotocol=4\nmessage=Select one controller\nwarning=Legacy setting disabled\n\
             repair_required=true\n\
             device=abcd|Controller One|true|\n\
             profile=abcd|switch|xbox|Controller One\n\
             candidate=Controller Two (event3)|missing ABS_X\n",
        )
        .unwrap();
        assert_eq!(snapshot.devices.len(), 1);
        assert_eq!(snapshot.devices[0].id, "abcd");
        assert_eq!(
            snapshot.profiles[0].conversion.as_ref().unwrap().source,
            "switch"
        );
        assert_eq!(
            snapshot.profiles[0].conversion.as_ref().unwrap().target,
            "xbox"
        );
        assert!(snapshot.candidates[0].contains("missing ABS_X"));
        assert!(snapshot.repair_required);
    }

    #[test]
    fn status_parser_rejects_the_legacy_global_service_protocol() {
        let error = parse_status("OK\nprotocol=3\n").unwrap_err();
        assert!(error.contains("older version of Switch Swap"));
    }

    #[test]
    fn controller_choices_keep_profiles_individual() {
        let snapshot = ServiceSnapshot {
            devices: vec![Controller {
                id: "one".to_owned(),
                name: "Xbox".to_owned(),
                connected: true,
                conversion: None,
                active: false,
                error: None,
            }],
            profiles: vec![Controller {
                id: "two".to_owned(),
                name: "Switch".to_owned(),
                connected: false,
                conversion: Some(Conversion {
                    source: "xbox".to_owned(),
                    target: "switch".to_owned(),
                }),
                active: false,
                error: None,
            }],
            ..ServiceSnapshot::default()
        };
        let (choices, labels) = controller_choices(&snapshot);
        assert_eq!(choices.len(), 2);
        assert!(choices[0].connected);
        assert!(labels[0].contains("not remapped"));
        assert!(!choices[1].connected);
        assert!(labels[1].contains("saved, disconnected"));
        assert!(labels[1].contains("plays as Nintendo Switch, A/B and X/Y swapped"));
    }
}
