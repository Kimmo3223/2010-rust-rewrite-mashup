//! Linux device transport over evdev (`/dev/input/event*`).
//!
//! Produces the same packet the Windows XInput transport does. With the
//! kernel's `xpad` driver the values are bit-identical to XInput: sticks are
//! the raw signed 16-bit axes (Y reported inverted by `xpad`, undone here with
//! the same bitwise NOT the driver applies) and triggers are the raw 0..=255
//! bytes. Other drivers (xpadneo, hid-microsoft over Bluetooth, hid-playstation,
//! hid-nintendo, …) report different ranges, which are rescaled linearly onto
//! the XInput ranges; no deadzone or curve is applied, so the TU3 converter
//! still sees unfiltered axes.
//!
//! Pads are assigned to slots 0..=3 in the order they are discovered.
//! Empty slots rescan `/dev/input` at most once a second, so hot-plugging
//! works, and a pad that returns `ENODEV` frees its slot.
use super::{CapabilityCache, DeviceError, DevicePacket};
use skate_core::input::xbox::XboxState;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---- <linux/input.h> and <linux/input-event-codes.h> ------------------------

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_GAS: u16 = 0x09;
const ABS_BRAKE: u16 = 0x0a;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;
const ABS_COUNT: usize = 0x12;

const BTN_SOUTH: u16 = 0x130;
const BTN_TL2: u16 = 0x138;
const BTN_TR2: u16 = 0x139;
const KEY_MAX: usize = 0x2ff;

/// evdev key code → XInput `wButtons` bit. `BTN_X`/`BTN_Y` carry the Xbox
/// labels, which is what `xpad` and xpadneo emit for the X and Y buttons.
/// The Guide button is left out, as `XInputGetState` never reports it.
const BUTTONS: [(u16, u16); 14] = [
    (0x130, 0x1000), // BTN_A      → XINPUT_GAMEPAD_A
    (0x131, 0x2000), // BTN_B      → B
    (0x133, 0x4000), // BTN_X      → X
    (0x134, 0x8000), // BTN_Y      → Y
    (0x136, 0x0100), // BTN_TL     → LEFT_SHOULDER
    (0x137, 0x0200), // BTN_TR     → RIGHT_SHOULDER
    (0x13a, 0x0020), // BTN_SELECT → BACK
    (0x13b, 0x0010), // BTN_START  → START
    (0x13d, 0x0040), // BTN_THUMBL → LEFT_THUMB
    (0x13e, 0x0080), // BTN_THUMBR → RIGHT_THUMB
    (0x220, 0x0001), // BTN_DPAD_UP    (xpad dpad_to_buttons=1)
    (0x221, 0x0002), // BTN_DPAD_DOWN
    (0x222, 0x0004), // BTN_DPAD_LEFT
    (0x223, 0x0008), // BTN_DPAD_RIGHT
];
const DPAD_UP: u16 = 0x0001;
const DPAD_DOWN: u16 = 0x0002;
const DPAD_LEFT: u16 = 0x0004;
const DPAD_RIGHT: u16 = 0x0008;

/// `XINPUT_DEVSUBTYPE_GAMEPAD`.
const SUBTYPE_GAMEPAD: u8 = 1;

const EVENT_SIZE: usize = 24; // struct input_event on 64-bit targets
const O_NONBLOCK: i32 = 0o4000;
const ENODEV: i32 = 19;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}
const _: () = assert!(size_of::<AbsInfo>() == 24);

unsafe extern "C" {
    fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int;
}

/// `_IOC(_IOC_READ, 'E', nr, size)` for the asm-generic ioctl layout used by
/// every architecture this module is compiled for.
const fn eviocg(nr: u32, size: usize) -> std::ffi::c_ulong {
    ((2u64 << 30) | ((size as u64) << 16) | ((b'E' as u64) << 8) | nr as u64) as std::ffi::c_ulong
}

fn ioctl_read(file: &File, request: std::ffi::c_ulong, out: &mut [u8]) -> bool {
    // SAFETY: every request built by `eviocg` encodes `out.len()` as its size,
    // so the kernel writes at most `out.len()` bytes into the buffer.
    unsafe { ioctl(file.as_raw_fd(), request, out.as_mut_ptr()) >= 0 }
}

fn bit(bits: &[u8], index: usize) -> bool {
    bits.get(index / 8)
        .is_some_and(|byte| byte & (1 << (index % 8)) != 0)
}

// ---- mapping (pure, unit-tested) --------------------------------------------

/// Which evdev axes feed which XInput field, fixed per device when it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    right: [u16; 2],
    /// Analog triggers, or `None` for pads with digital L2/R2 only.
    triggers: Option<[u16; 2]>,
}

impl Layout {
    fn from_axes(has: impl Fn(u16) -> bool) -> Self {
        // hid-generic Xbox pads over old Bluetooth firmware put the right
        // stick on Z/RZ and the triggers on BRAKE/GAS; xpad, xpadneo,
        // hid-playstation and most others use RX/RY + Z/RZ.
        let right = if has(ABS_RX) && has(ABS_RY) {
            [ABS_RX, ABS_RY]
        } else {
            [ABS_Z, ABS_RZ]
        };
        let triggers = if has(ABS_BRAKE) && has(ABS_GAS) {
            Some([ABS_BRAKE, ABS_GAS])
        } else if right != [ABS_Z, ABS_RZ] && has(ABS_Z) && has(ABS_RZ) {
            Some([ABS_Z, ABS_RZ])
        } else {
            None
        };
        Self { right, triggers }
    }
}

/// Raw axis value → XInput stick axis. The full i16 range passes through
/// untouched; anything else is mapped linearly onto it. `invert` applies the
/// bitwise NOT `xpad` uses for Y, which maps -32768↔32767 exactly.
fn stick(value: i32, info: &AbsInfo, invert: bool) -> i16 {
    let raw = if info.minimum == i16::MIN as i32 && info.maximum == i16::MAX as i32 {
        value as i16
    } else if info.maximum > info.minimum {
        let span = i64::from(info.maximum) - i64::from(info.minimum);
        let offset = (i64::from(value) - i64::from(info.minimum)).clamp(0, span);
        (i64::from(i16::MIN) + (offset * 65535 + span / 2) / span) as i16
    } else {
        0
    };
    if invert { !raw } else { raw }
}

/// Raw axis value → XInput trigger byte. 0..=255 passes through untouched.
fn trigger(value: i32, info: &AbsInfo) -> u8 {
    if info.minimum == 0 && info.maximum == 255 {
        return value.clamp(0, 255) as u8;
    }
    if info.maximum <= info.minimum {
        return 0;
    }
    let span = i64::from(info.maximum) - i64::from(info.minimum);
    let offset = (i64::from(value) - i64::from(info.minimum)).clamp(0, span);
    ((offset * 255 + span / 2) / span) as u8
}

/// Everything the device has told us so far, in evdev terms.
#[derive(Clone, Copy, Debug)]
struct Raw {
    buttons: u16,
    digital_triggers: [bool; 2],
    abs: [AbsInfo; ABS_COUNT],
}

impl Raw {
    fn key(&mut self, code: u16, pressed: bool) {
        if let Some(&(_, mask)) = BUTTONS.iter().find(|(key, _)| *key == code) {
            if pressed {
                self.buttons |= mask;
            } else {
                self.buttons &= !mask;
            }
        } else if code == BTN_TL2 {
            self.digital_triggers[0] = pressed;
        } else if code == BTN_TR2 {
            self.digital_triggers[1] = pressed;
        }
    }

    fn axis(&mut self, code: u16, value: i32) {
        if let Some(info) = self.abs.get_mut(usize::from(code)) {
            info.value = value;
        }
    }

    fn state(&self, layout: &Layout) -> XboxState {
        let abs = |code: u16| &self.abs[usize::from(code)];
        let mut buttons = self.buttons;
        let hat_x = abs(ABS_HAT0X).value;
        let hat_y = abs(ABS_HAT0Y).value;
        if hat_y < 0 {
            buttons |= DPAD_UP;
        }
        if hat_y > 0 {
            buttons |= DPAD_DOWN;
        }
        if hat_x < 0 {
            buttons |= DPAD_LEFT;
        }
        if hat_x > 0 {
            buttons |= DPAD_RIGHT;
        }
        let triggers = match layout.triggers {
            Some(codes) => codes.map(|code| trigger(abs(code).value, abs(code))),
            None => self
                .digital_triggers
                .map(|pressed| if pressed { 255 } else { 0 }),
        };
        let axis = |code: u16, invert: bool| stick(abs(code).value, abs(code), invert);
        XboxState {
            buttons,
            triggers,
            left: [axis(ABS_X, false), axis(ABS_Y, true)],
            right: [axis(layout.right[0], false), axis(layout.right[1], true)],
        }
    }
}

fn same(a: &XboxState, b: &XboxState) -> bool {
    a.buttons == b.buttons && a.triggers == b.triggers && a.left == b.left && a.right == b.right
}

/// The event-stream state machine, separate from the file so it can be tested.
struct Decoder {
    layout: Layout,
    published: Raw,
    pending: Raw,
    number: u32,
    /// Between `SYN_DROPPED` and the next `SYN_REPORT`: events are discarded
    /// and the full state is re-read from the device afterwards.
    dropped: bool,
}

enum Step {
    Continue,
    Resync,
}

impl Decoder {
    fn new(layout: Layout, raw: Raw) -> Self {
        Self {
            layout,
            published: raw,
            pending: raw,
            number: 0,
            dropped: false,
        }
    }

    fn event(&mut self, kind: u16, code: u16, value: i32) -> Step {
        match (kind, code) {
            (EV_SYN, SYN_DROPPED) => {
                self.dropped = true;
                Step::Continue
            }
            (EV_SYN, SYN_REPORT) if self.dropped => {
                self.dropped = false;
                Step::Resync
            }
            (EV_SYN, SYN_REPORT) => {
                self.publish(self.pending);
                Step::Continue
            }
            _ if self.dropped => Step::Continue,
            (EV_KEY, _) => {
                self.pending.key(code, value != 0);
                Step::Continue
            }
            (EV_ABS, _) => {
                self.pending.axis(code, value);
                Step::Continue
            }
            _ => Step::Continue,
        }
    }

    /// Like XInput's `dwPacketNumber`, the number moves only when the
    /// reported state actually changes.
    fn publish(&mut self, raw: Raw) {
        if !same(
            &raw.state(&self.layout),
            &self.published.state(&self.layout),
        ) {
            self.number = self.number.wrapping_add(1);
        }
        self.published = raw;
        self.pending = raw;
    }

    fn packet(&self) -> DevicePacket {
        DevicePacket {
            number: self.number,
            state: self.published.state(&self.layout),
            subtype: SUBTYPE_GAMEPAD,
        }
    }
}

// ---- devices ----------------------------------------------------------------

struct Pad {
    path: PathBuf,
    file: File,
    decoder: Decoder,
}

impl Pad {
    /// Opens `path` if it is a gamepad: it has the south face button and a
    /// left stick. Keyboards, mice, touchpads and motion-sensor nodes don't.
    fn open(path: &Path) -> Option<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(path)
            .ok()?;
        let mut keys = [0u8; KEY_MAX / 8 + 1];
        let mut axes = [0u8; ABS_COUNT.div_ceil(8)];
        if !ioctl_read(
            &file,
            eviocg(0x20 + u32::from(EV_KEY), keys.len()),
            &mut keys,
        ) || !ioctl_read(
            &file,
            eviocg(0x20 + u32::from(EV_ABS), axes.len()),
            &mut axes,
        ) || !bit(&keys, usize::from(BTN_SOUTH))
            || !bit(&axes, usize::from(ABS_X))
            || !bit(&axes, usize::from(ABS_Y))
        {
            return None;
        }
        let layout = Layout::from_axes(|code| bit(&axes, usize::from(code)));
        let mut pad = Self {
            path: path.to_owned(),
            file,
            decoder: Decoder::new(
                layout,
                Raw {
                    buttons: 0,
                    digital_triggers: [false; 2],
                    abs: [AbsInfo::default(); ABS_COUNT],
                },
            ),
        };
        let raw = pad.read_state()?;
        pad.decoder = Decoder::new(layout, raw);
        Some(pad)
    }

    /// The device's complete current state, straight from the kernel.
    fn read_state(&self) -> Option<Raw> {
        let mut raw = Raw {
            buttons: 0,
            digital_triggers: [false; 2],
            abs: [AbsInfo::default(); ABS_COUNT],
        };
        let mut axes = [0u8; ABS_COUNT.div_ceil(8)];
        if !ioctl_read(
            &self.file,
            eviocg(0x20 + u32::from(EV_ABS), axes.len()),
            &mut axes,
        ) {
            return None;
        }
        for code in 0..ABS_COUNT {
            if bit(&axes, code) {
                let mut info = [0u8; size_of::<AbsInfo>()];
                if !ioctl_read(
                    &self.file,
                    eviocg(0x40 + code as u32, info.len()),
                    &mut info,
                ) {
                    return None;
                }
                // SAFETY: `AbsInfo` is six plain i32s, valid for any bytes.
                raw.abs[code] = unsafe { std::ptr::read_unaligned(info.as_ptr().cast()) };
            }
        }
        let mut keys = [0u8; KEY_MAX / 8 + 1];
        if !ioctl_read(&self.file, eviocg(0x18, keys.len()), &mut keys) {
            return None;
        }
        for &(code, _) in &BUTTONS {
            raw.key(code, bit(&keys, usize::from(code)));
        }
        raw.key(BTN_TL2, bit(&keys, usize::from(BTN_TL2)));
        raw.key(BTN_TR2, bit(&keys, usize::from(BTN_TR2)));
        Some(raw)
    }

    /// Drains every queued event. `Err` means the pad is gone.
    fn pump(&mut self) -> Result<(), ()> {
        let mut buffer = [0u8; EVENT_SIZE * 64];
        loop {
            let length = match self.file.read(&mut buffer) {
                Ok(0) => return Err(()),
                Ok(length) => length,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.raw_os_error() == Some(ENODEV) => return Err(()),
                Err(_) => return Err(()),
            };
            for event in buffer[..length].chunks_exact(EVENT_SIZE) {
                let kind = u16::from_ne_bytes([event[16], event[17]]);
                let code = u16::from_ne_bytes([event[18], event[19]]);
                let value = i32::from_ne_bytes([event[20], event[21], event[22], event[23]]);
                if let Step::Resync = self.decoder.event(kind, code, value) {
                    let raw = self.read_state().ok_or(())?;
                    self.decoder.publish(raw);
                }
            }
        }
    }
}

struct Pads {
    slots: [Option<Pad>; 4],
    last_scan: Option<Instant>,
}

static PADS: Mutex<Pads> = Mutex::new(Pads {
    slots: [None, None, None, None],
    last_scan: None,
});

impl Pads {
    fn scan(&mut self) {
        let Ok(entries) = std::fs::read_dir("/dev/input") else {
            return;
        };
        let mut nodes: Vec<(u32, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let number = entry
                    .file_name()
                    .to_str()?
                    .strip_prefix("event")?
                    .parse()
                    .ok()?;
                Some((number, entry.path()))
            })
            .collect();
        nodes.sort();
        for (_, path) in nodes {
            if self.slots.iter().flatten().any(|pad| pad.path == path) {
                continue;
            }
            let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none()) else {
                return;
            };
            if let Some(pad) = Pad::open(&path) {
                *slot = Some(pad);
            }
        }
    }
}

pub(super) fn poll(index: usize, cache: &mut CapabilityCache) -> Result<DevicePacket, DeviceError> {
    let mut pads = PADS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if pads.slots[index].is_none() {
        let now = Instant::now();
        if pads
            .last_scan
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
        {
            pads.last_scan = Some(now);
            pads.scan();
        }
    }
    let Some(pad) = pads.slots[index].as_mut() else {
        cache.invalidate();
        return Err(DeviceError::Disconnected);
    };
    if pad.pump().is_err() {
        pads.slots[index] = None;
        cache.invalidate();
        return Err(DeviceError::Disconnected);
    }
    Ok(pad.decoder.packet())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(minimum: i32, maximum: i32) -> AbsInfo {
        AbsInfo {
            minimum,
            maximum,
            ..AbsInfo::default()
        }
    }

    fn xpad() -> Raw {
        let mut abs = [AbsInfo::default(); ABS_COUNT];
        for code in [ABS_X, ABS_Y, ABS_RX, ABS_RY] {
            abs[usize::from(code)] = info(-32768, 32767);
        }
        for code in [ABS_Z, ABS_RZ] {
            abs[usize::from(code)] = info(0, 255);
        }
        for code in [ABS_HAT0X, ABS_HAT0Y] {
            abs[usize::from(code)] = info(-1, 1);
        }
        Raw {
            buttons: 0,
            digital_triggers: [false; 2],
            abs,
        }
    }

    fn has(codes: &'static [u16]) -> impl Fn(u16) -> bool {
        move |code| codes.contains(&code)
    }

    #[test]
    fn xpad_axes_are_bit_identical_to_xinput() {
        let full = info(-32768, 32767);
        for value in [-32768, -1, 0, 1, 12345, 32767] {
            assert_eq!(stick(value, &full, false), value as i16);
            // xpad reports ~y; undoing it restores XInput's value exactly.
            assert_eq!(stick(!value, &full, true), value as i16);
        }
        let byte = info(0, 255);
        for value in [0, 1, 128, 255] {
            assert_eq!(trigger(value, &byte), value as u8);
        }
    }

    #[test]
    fn other_ranges_rescale_onto_xinput_ranges() {
        let ds = info(0, 255); // hid-playstation sticks
        assert_eq!(stick(0, &ds, false), i16::MIN);
        assert_eq!(stick(255, &ds, false), i16::MAX);
        assert_eq!(stick(0, &ds, true), i16::MAX); // stick pushed up
        let bt = info(0, 65535); // Xbox Bluetooth sticks
        assert_eq!(stick(0, &bt, false), i16::MIN);
        assert_eq!(stick(65535, &bt, false), i16::MAX);
        assert_eq!(stick(32768, &bt, false), 0);
        let wide = info(0, 1023); // xpadneo / Bluetooth triggers
        assert_eq!(trigger(0, &wide), 0);
        assert_eq!(trigger(1023, &wide), 255);
        assert_eq!(trigger(512, &wide), 128);
        assert_eq!(trigger(5000, &wide), 255);
    }

    #[test]
    fn layouts() {
        let xpad = Layout::from_axes(has(&[ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ]));
        assert_eq!(
            xpad,
            Layout {
                right: [ABS_RX, ABS_RY],
                triggers: Some([ABS_Z, ABS_RZ])
            }
        );
        let bt = Layout::from_axes(has(&[ABS_X, ABS_Y, ABS_Z, ABS_RZ, ABS_GAS, ABS_BRAKE]));
        assert_eq!(
            bt,
            Layout {
                right: [ABS_Z, ABS_RZ],
                triggers: Some([ABS_BRAKE, ABS_GAS])
            }
        );
        let digital = Layout::from_axes(has(&[ABS_X, ABS_Y, ABS_RX, ABS_RY]));
        assert_eq!(
            digital,
            Layout {
                right: [ABS_RX, ABS_RY],
                triggers: None
            }
        );
    }

    #[test]
    fn events_publish_on_syn_report_and_number_tracks_changes() {
        let layout = Layout::from_axes(has(&[ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ]));
        let mut decoder = Decoder::new(layout, xpad());
        decoder.event(EV_KEY, 0x130, 1);
        decoder.event(EV_ABS, ABS_RZ, 200);
        decoder.event(EV_ABS, ABS_Y, !1000);
        assert_eq!(
            decoder.packet().number,
            0,
            "nothing published before SYN_REPORT"
        );
        decoder.event(EV_SYN, SYN_REPORT, 0);
        let packet = decoder.packet();
        assert_eq!(packet.number, 1);
        assert_eq!(packet.state.buttons, 0x1000);
        assert_eq!(packet.state.triggers, [0, 200]);
        assert_eq!(packet.state.left, [0, 1000]);
        assert_eq!(packet.subtype, SUBTYPE_GAMEPAD);
        decoder.event(EV_SYN, SYN_REPORT, 0);
        assert_eq!(
            decoder.packet().number,
            1,
            "unchanged state keeps its number"
        );
        decoder.event(EV_ABS, ABS_HAT0X, -1);
        decoder.event(EV_ABS, ABS_HAT0Y, 1);
        decoder.event(EV_KEY, 0x130, 0);
        decoder.event(EV_SYN, SYN_REPORT, 0);
        let packet = decoder.packet();
        assert_eq!(packet.number, 2);
        assert_eq!(packet.state.buttons, DPAD_LEFT | DPAD_DOWN);
    }

    #[test]
    fn syn_dropped_discards_until_report_then_resyncs() {
        let layout = Layout::from_axes(has(&[ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ]));
        let mut decoder = Decoder::new(layout, xpad());
        decoder.event(EV_SYN, SYN_DROPPED, 0);
        decoder.event(EV_KEY, 0x130, 1);
        assert!(matches!(decoder.event(EV_SYN, SYN_REPORT, 0), Step::Resync));
        assert_eq!(
            decoder.packet().state.buttons,
            0,
            "events inside the drop are discarded"
        );
    }

    #[test]
    fn digital_triggers_read_as_full_pull() {
        let layout = Layout::from_axes(has(&[ABS_X, ABS_Y, ABS_RX, ABS_RY]));
        let mut decoder = Decoder::new(layout, xpad());
        decoder.event(EV_KEY, BTN_TR2, 1);
        decoder.event(EV_SYN, SYN_REPORT, 0);
        assert_eq!(decoder.packet().state.triggers, [0, 255]);
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // Values from the C macros on x86_64.
        assert_eq!(eviocg(0x20 + 1, 96), 0x8060_4521); // EVIOCGBIT(EV_KEY, 96)
        assert_eq!(eviocg(0x40, 24), 0x8018_4540); // EVIOCGABS(ABS_X)
        assert_eq!(eviocg(0x18, 96), 0x8060_4518); // EVIOCGKEY(96)
    }
}
