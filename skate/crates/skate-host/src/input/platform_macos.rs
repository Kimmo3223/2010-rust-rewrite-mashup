//! macOS transport: gilrs (IOKit HID, with SDL's mapping database) rebuilt into
//! XInput-shaped `XboxState`. gilrs's default filters (deadzones, jitter) are
//! off, so the TU3 converter still sees the stick as the pad reports it; only
//! the scale differs: gilrs hands out -1..1 floats, mapped back onto i16 here.
//!
//! A gilrs instance is owned by one thread that pumps its events and publishes
//! a snapshot per slot; `poll` only reads the snapshots.
use super::{CapabilityCache, DeviceError, DevicePacket};
use gilrs::{Axis, Button, GamepadId, GilrsBuilder};
use skate_core::input::xbox::XboxState;
use std::sync::{Mutex, Once};
use std::time::Duration;

/// `XINPUT_DEVSUBTYPE_GAMEPAD`.
const SUBTYPE_GAMEPAD: u8 = 1;

/// gilrs button → XInput button bit (same table as the evdev transport).
const BUTTONS: &[(Button, u16)] = &[
    (Button::DPadUp, 0x0001),
    (Button::DPadDown, 0x0002),
    (Button::DPadLeft, 0x0004),
    (Button::DPadRight, 0x0008),
    (Button::Start, 0x0010),
    (Button::Select, 0x0020),
    (Button::LeftThumb, 0x0040),
    (Button::RightThumb, 0x0080),
    (Button::LeftTrigger, 0x0100),
    (Button::RightTrigger, 0x0200),
    (Button::South, 0x1000),
    (Button::East, 0x2000),
    (Button::West, 0x4000),
    (Button::North, 0x8000),
];

/// `XboxState`'s fields, copyable so the snapshots can live in a static.
#[derive(Clone, Copy, PartialEq, Eq)]
struct State {
    buttons: u16,
    triggers: [u8; 2],
    left: [i16; 2],
    right: [i16; 2],
}

#[derive(Clone, Copy)]
struct Slot {
    id: GamepadId,
    number: u32,
    state: State,
}

static SLOTS: Mutex<[Option<Slot>; 4]> = Mutex::new([None; 4]);
static START: Once = Once::new();

/// -1..1 → XInput's full i16 stick range. gilrs reports up as positive, as XInput does.
fn stick(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn trigger(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn read(pad: &gilrs::Gamepad<'_>) -> State {
    let mut buttons = 0;
    for &(button, bit) in BUTTONS {
        if pad.is_pressed(button) {
            buttons |= bit;
        }
    }
    let analog = |button: Button| pad.button_data(button).map_or(0.0, |data| data.value());
    State {
        buttons,
        triggers: [
            trigger(analog(Button::LeftTrigger2)),
            trigger(analog(Button::RightTrigger2)),
        ],
        left: [
            stick(pad.value(Axis::LeftStickX)),
            stick(pad.value(Axis::LeftStickY)),
        ],
        right: [
            stick(pad.value(Axis::RightStickX)),
            stick(pad.value(Axis::RightStickY)),
        ],
    }
}

fn pump() {
    let mut gilrs = match GilrsBuilder::new().with_default_filters(false).build() {
        Ok(gilrs) => gilrs,
        Err(error) => {
            bevy::log::warn!("skate: gamepad input unavailable: {error}");
            return;
        }
    };
    loop {
        while gilrs
            .next_event_blocking(Some(Duration::from_millis(4)))
            .is_some()
        {}
        let mut slots = SLOTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for slot in slots.iter_mut() {
            if slot.is_some_and(|s| gilrs.connected_gamepad(s.id).is_none()) {
                *slot = None;
            }
        }
        for (id, pad) in gilrs.gamepads() {
            let state = read(&pad);
            if let Some(slot) = slots.iter_mut().flatten().find(|s| s.id == id) {
                if slot.state != state {
                    slot.number = slot.number.wrapping_add(1);
                    slot.state = state;
                }
            } else if let Some(free) = slots.iter_mut().find(|s| s.is_none()) {
                bevy::log::info!(
                    "skate: gamepad '{}' ({:04x}:{:04x}, {:?} mapping)",
                    pad.name(),
                    pad.vendor_id().unwrap_or(0),
                    pad.product_id().unwrap_or(0),
                    pad.mapping_source()
                );
                *free = Some(Slot {
                    id,
                    number: 1,
                    state,
                });
            }
        }
    }
}

pub(super) fn poll(index: usize, cache: &mut CapabilityCache) -> Result<DevicePacket, DeviceError> {
    START.call_once(|| {
        std::thread::Builder::new()
            .name("skate-gamepad".into())
            .spawn(pump)
            .expect("spawn the gamepad thread");
    });
    let slots = SLOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(slot) = slots[index] else {
        cache.invalidate();
        return Err(DeviceError::Disconnected);
    };
    let State {
        buttons,
        triggers,
        left,
        right,
    } = slot.state;
    Ok(DevicePacket {
        number: slot.number,
        state: XboxState {
            buttons,
            triggers,
            left,
            right,
        },
        subtype: SUBTYPE_GAMEPAD,
    })
}
