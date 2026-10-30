//! The virtual controller games see. It always has the exact controls of the
//! target controller (the xpad Xbox 360 pad, or the hid-nintendo Pro
//! Controller), whatever the physical controller reports, so games and SDL
//! assign every button and axis the same way they would for the real thing.

use crate::mapping::Layout;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, Device, EventSummary, EventType, FFEffect,
    FFEffectCode, FFEffectKind, InputEvent, InputId, KeyCode, UInputCode, UinputAbsSetup,
    uinput::VirtualDevice,
};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
const STICKS: [AbsoluteAxisCode; 4] = [
    AbsoluteAxisCode::ABS_X,
    AbsoluteAxisCode::ABS_Y,
    AbsoluteAxisCode::ABS_RX,
    AbsoluteAxisCode::ABS_RY,
];
const SHARED_KEYS: [KeyCode; 11] = [
    KeyCode::BTN_SOUTH,
    KeyCode::BTN_EAST,
    KeyCode::BTN_NORTH,
    KeyCode::BTN_WEST,
    KeyCode::BTN_TL,
    KeyCode::BTN_TR,
    KeyCode::BTN_SELECT,
    KeyCode::BTN_START,
    KeyCode::BTN_MODE,
    KeyCode::BTN_THUMBL,
    KeyCode::BTN_THUMBR,
];
/// Rumble effects a game may have loaded at once (xpad allows 16 too).
const MAX_RUMBLE_EFFECTS: u32 = 16;
/// Trigger travel past which an analog trigger counts as a ZL/ZR press.
const TRIGGER_PRESS_FRACTION: f64 = 0.4;

struct Spec {
    name: &'static str,
    id: InputId,
    keys: &'static [KeyCode],
    stick: AbsInfo,
    /// Analog triggers on ABS_Z/ABS_RZ (Xbox); `None` = digital ZL/ZR (Switch).
    trigger: Option<AbsInfo>,
}

fn spec(target: Layout) -> Spec {
    match target {
        // What the kernel's xpad driver creates for a wired Xbox 360 pad.
        Layout::Xbox => Spec {
            name: "Microsoft X-Box 360 pad",
            id: InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0114),
            keys: &[],
            stick: AbsInfo::new(0, -32768, 32767, 16, 128, 0),
            trigger: Some(AbsInfo::new(0, 0, 255, 0, 0, 0)),
        },
        // What hid-nintendo creates for a Pro Controller.
        Layout::Switch => Spec {
            name: "Nintendo Switch Pro Controller",
            id: InputId::new(BusType::BUS_USB, 0x057e, 0x2009, 0x8111),
            keys: &[KeyCode::BTN_TL2, KeyCode::BTN_TR2, KeyCode::BTN_Z],
            stick: AbsInfo::new(0, -32767, 32767, 250, 500, 0),
            trigger: None,
        },
    }
}

pub struct VirtualPad {
    device: VirtualDevice,
    spec: Spec,
    source_ranges: HashMap<u16, (i32, i32)>,
    /// Generic HID pads without RX/RY usually put the right stick on Z/RZ.
    z_is_right_stick: bool,
    /// The source reports analog triggers. Many such controllers (DualShock 4,
    /// DualSense) also send a digital L2/R2 press; that one is then ignored so
    /// the two don't fight over the same trigger.
    analog_triggers: bool,
    dpad: [bool; 4],
    trigger_pressed: [bool; 2],
    /// Rumble effects games uploaded to the virtual pad, by effect id, as
    /// uploaded to the physical controller.
    rumble: HashMap<i16, FFEffect>,
}

impl VirtualPad {
    pub fn create(source: &Device, target: Layout) -> std::io::Result<Self> {
        let spec = spec(target);
        let keys = SHARED_KEYS
            .iter()
            .chain(spec.keys)
            .copied()
            .collect::<AttributeSet<KeyCode>>();
        let mut builder = VirtualDevice::builder()?
            .name(spec.name)
            .input_id(InputId::new(
                spec.id.bus_type(),
                spec.id.vendor(),
                spec.id.product(),
                spec.id.version(),
            ))
            .with_keys(&keys)
            .map_err(|error| context("set the virtual controller's buttons", error))?;
        for axis in STICKS {
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, spec.stick))?;
        }
        if let Some(trigger) = spec.trigger {
            for axis in [AbsoluteAxisCode::ABS_Z, AbsoluteAxisCode::ABS_RZ] {
                builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, trigger))?;
            }
        }
        let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
        for axis in [AbsoluteAxisCode::ABS_HAT0X, AbsoluteAxisCode::ABS_HAT0Y] {
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, hat))?;
        }

        let source_ranges = source
            .get_absinfo()?
            .map(|(axis, info)| (axis.0, (info.minimum(), info.maximum())))
            .collect::<HashMap<_, _>>();
        let z_is_right_stick = !source_ranges.contains_key(&AbsoluteAxisCode::ABS_RX.0)
            && source_ranges.contains_key(&AbsoluteAxisCode::ABS_Z.0);
        let analog_triggers = (!z_is_right_stick
            && source_ranges.contains_key(&AbsoluteAxisCode::ABS_Z.0))
            || source_ranges.contains_key(&AbsoluteAxisCode::ABS_BRAKE.0)
            || source_ranges.contains_key(&AbsoluteAxisCode::ABS_GAS.0);
        // Offer rumble only when the physical controller can rumble.
        if source
            .supported_ff()
            .is_some_and(|effects| effects.contains(FFEffectCode::FF_RUMBLE))
        {
            builder = builder
                .with_ff(
                    &[FFEffectCode::FF_RUMBLE]
                        .into_iter()
                        .collect::<AttributeSet<_>>(),
                )
                .map_err(|error| context("enable rumble", error))?
                .with_ff_effects_max(MAX_RUMBLE_EFFECTS);
        }
        let device = builder
            .build()
            .map_err(|error| context("create the virtual controller", error))?;
        set_nonblocking(device.as_raw_fd())?;
        Ok(Self {
            device,
            spec,
            source_ranges,
            z_is_right_stick,
            analog_triggers,
            dpad: [false; 4],
            trigger_pressed: [false; 2],
            rumble: HashMap::new(),
        })
    }

    pub fn as_raw_fd(&self) -> i32 {
        self.device.as_raw_fd()
    }

    /// Handles rumble requests games sent to the virtual pad: uploads,
    /// removals, and play/stop, passing each on to the physical controller.
    pub fn forward_rumble(&mut self, physical: &mut Device) -> std::io::Result<()> {
        loop {
            let events = match self.device.fetch_events() {
                Ok(events) => events.collect::<Vec<_>>(),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            };
            if events.is_empty() {
                return Ok(());
            }
            for event in events {
                match event.destructure() {
                    // Every request is answered and errors don't stop the batch:
                    // an unanswered upload or erase leaves the game waiting.
                    EventSummary::UInput(event, UInputCode::UI_FF_UPLOAD, _) => {
                        let Ok(mut upload) = self.device.process_ff_upload(event) else {
                            continue;
                        };
                        let id = upload.effect_id();
                        let data = upload.effect();
                        let result = if !matches!(data.kind, FFEffectKind::Rumble { .. }) {
                            Err(-libc::EINVAL)
                        } else if let Some(effect) = self.rumble.get_mut(&id) {
                            effect.update(data).map_err(errno)
                        } else {
                            physical
                                .upload_ff_effect(data)
                                .map_err(errno)
                                .map(|effect| {
                                    self.rumble.insert(id, effect);
                                })
                        };
                        upload.set_retval(result.err().unwrap_or(0));
                    }
                    EventSummary::UInput(event, UInputCode::UI_FF_ERASE, _) => {
                        let Ok(erase) = self.device.process_ff_erase(event) else {
                            continue;
                        };
                        if let Ok(id) = i16::try_from(erase.effect_id()) {
                            self.rumble.remove(&id);
                        }
                    }
                    EventSummary::ForceFeedback(_, code, count) => {
                        if let Ok(id) = i16::try_from(code.0)
                            && let Some(effect) = self.rumble.get_mut(&id)
                        {
                            // A controller dropping out mid-rumble isn't worth
                            // stopping for; its worker ends on the next read.
                            let _ = if count > 0 {
                                effect.play(count)
                            } else {
                                effect.stop()
                            };
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Forwards one batch of (already face-button-mapped) physical events.
    pub fn emit(&mut self, events: impl IntoIterator<Item = InputEvent>) -> std::io::Result<()> {
        let mut output = Vec::new();
        for event in events {
            self.translate(event, &mut output);
        }
        if output.is_empty() {
            return Ok(());
        }
        self.device.emit(&output)
    }

    fn translate(&mut self, event: InputEvent, output: &mut Vec<InputEvent>) {
        let (code, value) = (event.code(), event.value());
        match event.event_type() {
            EventType::KEY => self.translate_key(KeyCode(code), value, output),
            EventType::ABSOLUTE => self.translate_axis(AbsoluteAxisCode(code), value, output),
            _ => {}
        }
    }

    fn translate_key(&mut self, key: KeyCode, value: i32, output: &mut Vec<InputEvent>) {
        let dpad = [
            KeyCode::BTN_DPAD_UP,
            KeyCode::BTN_DPAD_DOWN,
            KeyCode::BTN_DPAD_LEFT,
            KeyCode::BTN_DPAD_RIGHT,
        ];
        if let Some(index) = dpad.iter().position(|button| *button == key) {
            self.dpad[index] = value != 0;
            let [up, down, left, right] = self.dpad.map(i32::from);
            output.push(abs(AbsoluteAxisCode::ABS_HAT0Y, down - up));
            output.push(abs(AbsoluteAxisCode::ABS_HAT0X, right - left));
            return;
        }
        let trigger = match key {
            KeyCode::BTN_TL2 => Some(AbsoluteAxisCode::ABS_Z),
            KeyCode::BTN_TR2 => Some(AbsoluteAxisCode::ABS_RZ),
            _ => None,
        };
        if trigger.is_some() && self.analog_triggers {
            return;
        }
        if let (Some(axis), Some(info)) = (trigger, self.spec.trigger) {
            let level = if value != 0 {
                info.maximum()
            } else {
                info.minimum()
            };
            output.push(abs(axis, level));
            return;
        }
        if SHARED_KEYS.contains(&key) || self.spec.keys.contains(&key) {
            output.push(InputEvent::new(EventType::KEY.0, key.0, value));
        }
    }

    fn translate_axis(&mut self, axis: AbsoluteAxisCode, value: i32, output: &mut Vec<InputEvent>) {
        let stick_axis = match axis {
            AbsoluteAxisCode::ABS_Z if self.z_is_right_stick => AbsoluteAxisCode::ABS_RX,
            AbsoluteAxisCode::ABS_RZ if self.z_is_right_stick => AbsoluteAxisCode::ABS_RY,
            other => other,
        };
        if STICKS.contains(&stick_axis) {
            let stick = self.spec.stick;
            output.push(abs(
                stick_axis,
                self.rescale(axis, value, stick.minimum(), stick.maximum()),
            ));
            return;
        }
        let trigger = match axis {
            AbsoluteAxisCode::ABS_Z | AbsoluteAxisCode::ABS_BRAKE => {
                Some((0, AbsoluteAxisCode::ABS_Z))
            }
            AbsoluteAxisCode::ABS_RZ | AbsoluteAxisCode::ABS_GAS => {
                Some((1, AbsoluteAxisCode::ABS_RZ))
            }
            AbsoluteAxisCode::ABS_HAT0X | AbsoluteAxisCode::ABS_HAT0Y => {
                output.push(abs(axis, value.signum()));
                return;
            }
            _ => None,
        };
        let Some((side, output_axis)) = trigger else {
            return;
        };
        match self.spec.trigger {
            Some(info) => {
                output.push(abs(
                    output_axis,
                    self.rescale(axis, value, info.minimum(), info.maximum()),
                ));
            }
            None => {
                let (minimum, maximum) = self.source_range(axis);
                let travel = f64::from(value - minimum) / f64::from((maximum - minimum).max(1));
                let pressed = travel > TRIGGER_PRESS_FRACTION;
                if pressed != self.trigger_pressed[side] {
                    self.trigger_pressed[side] = pressed;
                    let key = if side == 0 {
                        KeyCode::BTN_TL2
                    } else {
                        KeyCode::BTN_TR2
                    };
                    output.push(InputEvent::new(EventType::KEY.0, key.0, i32::from(pressed)));
                }
            }
        }
    }

    fn source_range(&self, axis: AbsoluteAxisCode) -> (i32, i32) {
        self.source_ranges.get(&axis.0).copied().unwrap_or((0, 255))
    }

    fn rescale(&self, axis: AbsoluteAxisCode, value: i32, minimum: i32, maximum: i32) -> i32 {
        let Some(&(source_min, source_max)) = self.source_ranges.get(&axis.0) else {
            return value.clamp(minimum, maximum);
        };
        rescale(value, source_min, source_max, minimum, maximum)
    }
}

fn rescale(value: i32, source_min: i32, source_max: i32, minimum: i32, maximum: i32) -> i32 {
    if source_max <= source_min {
        return value.clamp(minimum, maximum);
    }
    let value = i64::from(value.clamp(source_min, source_max));
    let scaled = i64::from(minimum)
        + (value - i64::from(source_min)) * (i64::from(maximum) - i64::from(minimum))
            / (i64::from(source_max) - i64::from(source_min));
    scaled as i32
}

fn errno(error: std::io::Error) -> i32 {
    -error.raw_os_error().unwrap_or(libc::EIO)
}

fn set_nonblocking(fd: i32) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn context(action: &str, error: std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), format!("cannot {action}: {error}"))
}

fn abs(axis: AbsoluteAxisCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::ABSOLUTE.0, axis.0, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_have_the_real_controllers_identity() {
        let xbox = spec(Layout::Xbox);
        assert_eq!(xbox.name, "Microsoft X-Box 360 pad");
        assert_eq!((xbox.id.vendor(), xbox.id.product()), (0x045e, 0x028e));
        assert!(xbox.keys.is_empty());

        let switch = spec(Layout::Switch);
        assert_eq!(switch.name, "Nintendo Switch Pro Controller");
        assert_eq!((switch.id.vendor(), switch.id.product()), (0x057e, 0x2009));
        assert!(!switch.name.contains("Swap"));
    }

    #[test]
    fn rescales_between_ranges() {
        assert_eq!(rescale(0, 0, 255, -32768, 32767), -32768);
        assert_eq!(rescale(255, 0, 255, -32768, 32767), 32767);
        assert_eq!(rescale(-32767, -32767, 32767, -32768, 32767), -32768);
        assert_eq!(rescale(1023, 0, 1023, 0, 255), 255);
        assert_eq!(rescale(5000, 0, 1023, 0, 255), 255);
        assert_eq!(rescale(7, 5, 5, 0, 255), 7);
    }
}

/// End-to-end check against the kernel: needs write access to /dev/uinput.
/// Run with `cargo test -- --ignored`.
#[cfg(test)]
mod uinput_tests {
    use super::*;
    use std::time::Duration;

    fn open_created(device: &mut VirtualDevice) -> Device {
        let node = device
            .enumerate_dev_nodes_blocking()
            .unwrap()
            .find_map(|node| {
                node.ok()
                    .filter(|path| path.to_string_lossy().contains("event"))
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let device = Device::open(node).unwrap();
        device.set_nonblocking(true).unwrap();
        device
    }

    fn read_all(device: &mut Device) -> Vec<InputEvent> {
        std::thread::sleep(Duration::from_millis(100));
        device
            .fetch_events()
            .map(|events| events.collect())
            .unwrap_or_default()
    }

    #[test]
    #[ignore]
    fn pro_controller_becomes_an_exact_xbox_360_pad() {
        let keys = SHARED_KEYS
            .iter()
            .chain(&[KeyCode::BTN_Z, KeyCode::BTN_TL2, KeyCode::BTN_TR2])
            .copied()
            .collect::<AttributeSet<KeyCode>>();
        let stick = AbsInfo::new(0, -32767, 32767, 0, 0, 0);
        let mut builder = VirtualDevice::builder()
            .unwrap()
            .name("Test Fake Pro Controller")
            .input_id(InputId::new(BusType::BUS_BLUETOOTH, 0x057e, 0x2009, 0x8001))
            .with_keys(&keys)
            .unwrap();
        for axis in STICKS {
            builder = builder
                .with_absolute_axis(&UinputAbsSetup::new(axis, stick))
                .unwrap();
        }
        let mut fake = builder.build().unwrap();
        let source = open_created(&mut fake);

        let mut pad = VirtualPad::create(&source, Layout::Xbox).unwrap();
        let mut output = open_created(&mut pad.device);
        assert_eq!(output.name(), Some("Microsoft X-Box 360 pad"));
        let node = pad
            .device
            .enumerate_dev_nodes_blocking()
            .unwrap()
            .find_map(|node| {
                node.ok()
                    .filter(|path| path.to_string_lossy().contains("event"))
            })
            .unwrap();
        assert!(crate::identity::is_virtual_device(&node));
        let output_keys = output.supported_keys().unwrap().iter().collect::<Vec<_>>();
        assert_eq!(output_keys.len(), SHARED_KEYS.len());
        assert!(!output_keys.contains(&KeyCode::BTN_Z));
        assert!(!output_keys.contains(&KeyCode::BTN_TL2));

        pad.emit([
            InputEvent::new(EventType::KEY.0, KeyCode::BTN_TL2.0, 1),
            InputEvent::new(EventType::KEY.0, KeyCode::BTN_Z.0, 1),
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, 32767),
            InputEvent::new(EventType::KEY.0, KeyCode::BTN_SOUTH.0, 1),
        ])
        .unwrap();
        let events = read_all(&mut output)
            .into_iter()
            .filter(|event| event.event_type() != EventType::SYNCHRONIZATION)
            .map(|event| (event.event_type(), event.code(), event.value()))
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![
                (EventType::ABSOLUTE, AbsoluteAxisCode::ABS_Z.0, 255),
                (EventType::ABSOLUTE, AbsoluteAxisCode::ABS_X.0, 32767),
                (EventType::KEY, KeyCode::BTN_SOUTH.0, 1),
            ]
        );
    }

    #[test]
    #[ignore]
    fn rumble_from_a_game_reaches_the_physical_controller() {
        use evdev::{FFEffectData, FFReplay, FFTrigger};
        use std::sync::mpsc;
        use std::time::Instant;

        let keys = SHARED_KEYS
            .iter()
            .copied()
            .collect::<AttributeSet<KeyCode>>();
        let fake = VirtualDevice::builder()
            .unwrap()
            .name("Test Fake Rumble Pad")
            .with_keys(&keys)
            .unwrap()
            .with_ff(
                &[FFEffectCode::FF_RUMBLE]
                    .into_iter()
                    .collect::<AttributeSet<_>>(),
            )
            .unwrap()
            .with_ff_effects_max(4)
            .build()
            .unwrap();
        let mut fake = fake;
        let mut source = open_created(&mut fake);

        // The fake controller's "firmware": accepts uploads, reports plays.
        let (played_tx, played_rx) = mpsc::channel();
        let deadline = Instant::now() + Duration::from_secs(5);
        let fake_thread = std::thread::spawn(move || {
            set_nonblocking(fake.as_raw_fd()).unwrap();
            while Instant::now() < deadline {
                let events = fake
                    .fetch_events()
                    .map(|e| e.collect::<Vec<_>>())
                    .unwrap_or_default();
                for event in events {
                    match event.destructure() {
                        EventSummary::UInput(event, UInputCode::UI_FF_UPLOAD, _) => {
                            let upload = fake.process_ff_upload(event).unwrap();
                            if let FFEffectKind::Rumble {
                                strong_magnitude, ..
                            } = upload.effect().kind
                            {
                                let _ = played_tx.send(("upload", i32::from(strong_magnitude)));
                            }
                        }
                        EventSummary::UInput(event, UInputCode::UI_FF_ERASE, _) => {
                            fake.process_ff_erase(event).unwrap();
                        }
                        EventSummary::ForceFeedback(_, _, count) => {
                            let _ = played_tx.send(("play", count));
                        }
                        _ => {}
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let mut pad = VirtualPad::create(&source, Layout::Xbox).unwrap();
        let mut game_side = open_created(&mut pad.device);
        assert!(
            game_side
                .supported_ff()
                .unwrap()
                .contains(FFEffectCode::FF_RUMBLE)
        );

        let game = std::thread::spawn(move || {
            let mut effect = game_side
                .upload_ff_effect(FFEffectData {
                    direction: 0,
                    trigger: FFTrigger::default(),
                    replay: FFReplay {
                        length: 200,
                        delay: 0,
                    },
                    kind: FFEffectKind::Rumble {
                        strong_magnitude: 0xc000,
                        weak_magnitude: 0x4000,
                    },
                })
                .unwrap();
            effect.play(1).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });

        let mut seen = Vec::new();
        // Keep serving until the game has also erased its effect on exit.
        while Instant::now() < deadline && !(game.is_finished() && seen.len() >= 2) {
            pad.forward_rumble(&mut source).unwrap();
            while let Ok(item) = played_rx.try_recv() {
                seen.push(item);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        game.join().unwrap();
        // Upload and play reach the controller; when the game exits and
        // erases the effect, the controller's rumble is stopped too.
        assert_eq!(seen[..2], [("upload", 0xc000), ("play", 1)]);
        assert!(seen[2..].iter().all(|item| *item == ("play", 0)));
        assert!(seen.len() > 2);
        drop(pad);
        fake_thread.join().unwrap();
    }
}
