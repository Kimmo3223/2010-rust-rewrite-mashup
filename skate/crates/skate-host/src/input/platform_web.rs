//! The browser's Gamepad API in the XInput shape the converter reads: a standard-mapping pad's
//! buttons, triggers and sticks as `XboxState`, with Y pointing up as XInput's does.
use super::{DeviceError, DevicePacket};
use skate_core::input::xbox::XboxState;
use wasm_bindgen::JsCast;

/// XInput button bits for standard-mapping buttons 0..=15 (A B X Y, LB RB, LT RT, Back Start,
/// LS RS, d-pad up down left right); the triggers are axes, not bits.
const BITS: [u16; 16] = [
    0x1000, 0x2000, 0x4000, 0x8000, 0x0100, 0x0200, 0, 0, 0x0020, 0x0010, 0x0040, 0x0080,
    0x0001, 0x0002, 0x0004, 0x0008,
];

pub(super) fn poll(index: usize) -> Result<DevicePacket, DeviceError> {
    let pads = web_sys::window()
        .and_then(|w| w.navigator().get_gamepads().ok())
        .ok_or(DeviceError::Disconnected)?;
    // Read by property, not checked by class, so a page can stand in a pad of its own.
    let pad = pads.get(index as u32);
    if pad.is_null() || pad.is_undefined() {
        return Err(DeviceError::Disconnected);
    }
    let pad = pad.unchecked_into::<web_sys::Gamepad>();
    if !pad.connected() {
        return Err(DeviceError::Disconnected);
    }
    let buttons = pad.buttons();
    let button = |i: u32| {
        let b = buttons.get(i);
        (!b.is_undefined()).then(|| b.unchecked_into::<web_sys::GamepadButton>())
    };
    let mut bits = 0u16;
    for (i, bit) in BITS.iter().enumerate() {
        if button(i as u32).is_some_and(|b| b.pressed()) {
            bits |= bit;
        }
    }
    let trigger = |i| button(i).map_or(0, |b| (b.value().clamp(0.0, 1.0) * 255.0).round() as u8);
    let axes = pad.axes();
    let axis = |i: u32, sign: f64| {
        let v = axes.get(i).as_f64().unwrap_or(0.0) * sign;
        (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
    };
    Ok(DevicePacket {
        number: pad.timestamp() as u32,
        state: XboxState {
            buttons: bits,
            triggers: [trigger(6), trigger(7)],
            left: [axis(0, 1.0), axis(1, -1.0)],
            right: [axis(2, 1.0), axis(3, -1.0)],
        },
        // XINPUT_DEVSUBTYPE_GAMEPAD.
        subtype: 1,
    })
}
