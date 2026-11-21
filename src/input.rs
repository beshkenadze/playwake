use serde_json::Value;

pub struct Controller {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    gilrs: Option<gilrs::Gilrs>,
    last: Option<Value>,
}

impl Controller {
    pub fn new() -> Self {
        Self {
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            gilrs: gilrs::Gilrs::new().ok(),
            last: None,
        }
    }

    pub fn poll(&mut self) -> Option<Value> {
        let state = self.state()?;
        if self.last.as_ref() == Some(&state) { return None; }
        self.last = Some(state.clone());
        Some(state)
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    fn state(&mut self) -> Option<Value> { None }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn state(&mut self) -> Option<Value> {
        use gilrs::{Axis, Button};
        use serde_json::json;
        let inputs = self.gilrs.as_mut()?;
        while inputs.next_event().is_some() {}
        let Some((_, gamepad)) = inputs.gamepads().find(|(_, pad)| pad.is_connected()) else {
            return Some(json!({"action":"gamepad","axes":[0,0,0,0],"buttons":0,"lt":0,"rt":0}));
        };
        let value = |axis| (gamepad.value(axis).clamp(-1.0, 1.0) * 32767.0).round() as i16;
        let trigger = |button| gamepad.button_data(button).map(|data|
            (data.value().clamp(0.0, 1.0) * 65535.0).round() as u16).unwrap_or(0);
        let axes = [value(Axis::LeftStickX), -value(Axis::LeftStickY),
            value(Axis::RightStickX), -value(Axis::RightStickY)];
        let buttons = [
            (Button::Mode, 2), (Button::Start, 4), (Button::Select, 8), (Button::South, 16),
            (Button::East, 32), (Button::West, 64), (Button::North, 128), (Button::DPadUp, 256),
            (Button::DPadDown, 512), (Button::DPadLeft, 1024), (Button::DPadRight, 2048),
            (Button::LeftTrigger, 4096), (Button::RightTrigger, 8192),
            (Button::LeftThumb, 16384), (Button::RightThumb, 32768)
        ].into_iter().filter(|(button, _)| gamepad.is_pressed(*button))
            .fold(0_u16, |mask, (_, bit)| mask | bit);
        Some(json!({"action":"gamepad","axes":axes,"buttons":buttons,
            "lt":trigger(Button::LeftTrigger2),"rt":trigger(Button::RightTrigger2)}))
    }
}
