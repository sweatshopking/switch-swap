use evdev::{EventType, InputEvent, KeyCode};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Layout {
    #[default]
    Xbox,
    Switch,
}

impl Layout {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "xbox" => Some(Self::Xbox),
            "switch" => Some(Self::Switch),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Xbox => "xbox",
            Self::Switch => "switch",
        }
    }
}

pub fn map_event(event: InputEvent, layout: Layout) -> InputEvent {
    if layout != Layout::Switch || event.event_type() != EventType::KEY {
        return event;
    }

    let code = match event.code() {
        code if code == KeyCode::BTN_SOUTH.0 => KeyCode::BTN_EAST.0,
        code if code == KeyCode::BTN_EAST.0 => KeyCode::BTN_SOUTH.0,
        code if code == KeyCode::BTN_WEST.0 => KeyCode::BTN_NORTH.0,
        code if code == KeyCode::BTN_NORTH.0 => KeyCode::BTN_WEST.0,
        _ => return event,
    };

    InputEvent::new(event.event_type().0, code, event.value())
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::AbsoluteAxisCode;

    #[test]
    fn xbox_layout_leaves_face_buttons_unchanged() {
        for key in [
            KeyCode::BTN_SOUTH,
            KeyCode::BTN_EAST,
            KeyCode::BTN_WEST,
            KeyCode::BTN_NORTH,
        ] {
            let event = *evdev::KeyEvent::new(key, 1);
            assert_eq!(map_event(event, Layout::Xbox).code(), key.0);
        }
    }

    #[test]
    fn switch_layout_swaps_all_four_face_button_positions() {
        let pairs = [
            (KeyCode::BTN_SOUTH, KeyCode::BTN_EAST),
            (KeyCode::BTN_EAST, KeyCode::BTN_SOUTH),
            (KeyCode::BTN_WEST, KeyCode::BTN_NORTH),
            (KeyCode::BTN_NORTH, KeyCode::BTN_WEST),
        ];

        for (input, output) in pairs {
            for value in [0, 1, 2] {
                let event = *evdev::KeyEvent::new(input, value);
                let mapped = map_event(event, Layout::Switch);
                assert_eq!(mapped.event_type(), EventType::KEY);
                assert_eq!(mapped.code(), output.0);
                assert_eq!(mapped.value(), value);
            }
        }
    }

    #[test]
    fn unrelated_keys_and_axes_are_never_changed() {
        let key = *evdev::KeyEvent::new(KeyCode::BTN_START, 1);
        let axis = *evdev::AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_X, -123);

        assert_eq!(map_event(key, Layout::Switch), key);
        assert_eq!(map_event(axis, Layout::Switch), axis);
    }

    #[test]
    fn invalid_layout_names_are_rejected() {
        assert_eq!(Layout::parse("xbox"), Some(Layout::Xbox));
        assert_eq!(Layout::parse("switch"), Some(Layout::Switch));
        assert_eq!(Layout::parse("other"), None);
    }
}
