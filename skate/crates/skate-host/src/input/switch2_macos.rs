//! Switch 2 Pro Controller over USB: a fixed command sequence on the vendor
//! bulk interface (1) makes it stream 64-byte input reports on its HID
//! interface, read here through hidapi. Commands, report layout and stick
//! calibration follow SDL's `SDL_hidapi_switch2.c` (zlib); buttons map by
//! position, as SDL's database maps Nintendo pads, so B is XInput's A.
use super::{SLOTS, Source, State, publish};
use std::time::Duration;

const VENDOR: u16 = 0x057e;
const PRO: u16 = 0x2069;
const INTERFACE: u8 = 1;
const REPORT_FORMAT: u8 = 0x05;

const INIT: [&[u8]; 10] = [
    &[0x07, 0x91, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
    &[
        0x0c, 0x91, 0x00, 0x02, 0x00, 0x04, 0x00, 0x00, 0x27, 0x00, 0x00, 0x00,
    ],
    &[0x11, 0x91, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
    &[
        0x0a, 0x91, 0x00, 0x08, 0x00, 0x14, 0x00, 0x00, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0x35, 0x00, 0x46, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ],
    &[
        0x0c, 0x91, 0x00, 0x04, 0x00, 0x04, 0x00, 0x00, 0x27, 0x00, 0x00, 0x00,
    ],
    &[0x01, 0x91, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x00],
    &[0x01, 0x91, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
    &[
        0x08, 0x91, 0x00, 0x02, 0x00, 0x04, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    ],
    &[
        0x03, 0x91, 0x00, 0x0a, 0x00, 0x04, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
    ],
    &[
        0x03, 0x91, 0x00, 0x0d, 0x00, 0x08, 0x00, 0x00, 0x01, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff,
    ],
];

/// (report byte, bit, XInput button bit).
const BUTTONS: [(usize, u8, u16); 14] = [
    (5, 0x04, 0x1000),
    (5, 0x08, 0x2000),
    (5, 0x01, 0x4000),
    (5, 0x02, 0x8000),
    (5, 0x40, 0x0200),
    (6, 0x01, 0x0020),
    (6, 0x02, 0x0010),
    (6, 0x04, 0x0080),
    (6, 0x08, 0x0040),
    (7, 0x02, 0x0001),
    (7, 0x01, 0x0002),
    (7, 0x08, 0x0004),
    (7, 0x04, 0x0008),
    (7, 0x40, 0x0100),
];

pub(super) fn is_switch2(vendor: Option<u16>, product: Option<u16>) -> bool {
    vendor == Some(VENDOR) && product == Some(PRO)
}

/// One 12-bit axis: its rest value and its reach on each side.
#[derive(Clone, Copy)]
struct Axis {
    neutral: f32,
    max: f32,
    min: f32,
}

impl Axis {
    const UNCALIBRATED: Axis = Axis {
        neutral: 2048.0,
        max: 2048.0,
        min: 2048.0,
    };

    fn map(self, raw: u16) -> i16 {
        let offset = f32::from(raw) - self.neutral;
        let unit = offset / if offset < 0.0 { self.min } else { self.max };
        (unit.clamp(-1.0, 1.0) * 32767.0).round() as i16
    }
}

type Stick = [Axis; 2];

fn twelve_bit(low: u8, high: u8, upper_nibble: bool) -> u16 {
    if upper_nibble {
        (u16::from(low) >> 4) | (u16::from(high) << 4)
    } else {
        u16::from(low) | ((u16::from(high) & 0x0f) << 8)
    }
}

/// Nine bytes: neutral, max and min, each an x/y pair of 12-bit values.
fn parse_stick(data: &[u8]) -> Option<Stick> {
    let pair = |i: usize| {
        (
            twelve_bit(data[i], data[i + 1], false),
            twelve_bit(data[i + 1], data[i + 2], true),
        )
    };
    let (neutral, max, min) = (pair(0), pair(3), pair(6));
    let axis = |n: u16, x: u16, m: u16| {
        (n != 0 && x != 0 && m != 0).then(|| Axis {
            neutral: f32::from(n),
            max: f32::from(x),
            min: f32::from(m),
        })
    };
    Some([
        axis(neutral.0, max.0, min.0)?,
        axis(neutral.1, max.1, min.1)?,
    ])
}

fn decode(report: &[u8], sticks: &[Stick; 2]) -> Option<State> {
    if report.len() < 64 || report[0] != REPORT_FORMAT {
        return None;
    }
    let buttons = BUTTONS
        .iter()
        .filter(|&&(byte, bit, _)| report[byte] & bit != 0)
        .fold(0, |all, &(_, _, xinput)| all | xinput);
    let trigger = |byte: usize| if report[byte] & 0x80 != 0 { 255 } else { 0 };
    let stick = |at: usize, cal: &Stick| {
        [
            cal[0].map(twelve_bit(report[at], report[at + 1], false)),
            cal[1].map(twelve_bit(report[at + 1], report[at + 2], true)),
        ]
    };
    Some(State {
        buttons,
        triggers: [trigger(7), trigger(5)],
        left: stick(11, &sticks[0]),
        right: stick(14, &sticks[1]),
    })
}

struct Bulk {
    handle: rusb::DeviceHandle<rusb::Context>,
    out: u8,
    input: u8,
}

impl Bulk {
    fn open() -> Option<Self> {
        use rusb::UsbContext;
        let context = rusb::Context::new().ok()?;
        let device = context.devices().ok()?.iter().find(|d| {
            d.device_descriptor()
                .is_ok_and(|d| is_switch2(Some(d.vendor_id()), Some(d.product_id())))
        })?;
        let config = device.config_descriptor(0).ok()?;
        let (mut out, mut input) = (None, None);
        for endpoint in config
            .interfaces()
            .flat_map(|i| i.descriptors())
            .filter(|d| d.interface_number() == INTERFACE)
            .flat_map(|d| d.endpoint_descriptors().collect::<Vec<_>>())
            .filter(|e| e.transfer_type() == rusb::TransferType::Bulk)
        {
            match endpoint.direction() {
                rusb::Direction::Out => out = Some(endpoint.address()),
                rusb::Direction::In => input = Some(endpoint.address()),
            }
        }
        let handle = device.open().ok()?;
        if let Err(error) = handle.claim_interface(INTERFACE) {
            bevy::log::warn!("skate: Switch 2 Pro Controller: claim interface: {error}");
            return None;
        }
        Some(Self {
            handle,
            out: out?,
            input: input?,
        })
    }

    fn send(&self, data: &[u8]) -> bool {
        self.handle
            .write_bulk(self.out, data, Duration::from_secs(1))
            .is_ok()
    }

    fn receive(&self, buffer: &mut [u8]) -> usize {
        let mut total = 0;
        for chunk in buffer.chunks_mut(64) {
            let read = self
                .handle
                .read_bulk(self.input, chunk, Duration::from_millis(100))
                .unwrap_or(0);
            total += read;
            if read < chunk.len() {
                break;
            }
        }
        total
    }

    fn flash(&self, address: u32) -> Option<[u8; 0x40]> {
        let mut command = [
            0x02, 0x91, 0x00, 0x01, 0x00, 0x08, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        command[12..].copy_from_slice(&address.to_le_bytes());
        let mut reply = [0; 0x50];
        (self.send(&command) && self.receive(&mut reply) == reply.len())
            .then(|| reply[0x10..].try_into().unwrap())
    }

    /// The factory calibration, replaced by a user one carrying its magic.
    fn stick(&self, factory: u32, user: u32) -> Stick {
        let user = self
            .flash(user)
            .filter(|b| b[..2] == [0xb2, 0xa1])
            .and_then(|b| parse_stick(&b[2..]));
        user.or_else(|| self.flash(factory).and_then(|b| parse_stick(&b[0x28..])))
            .unwrap_or([Axis::UNCALIBRATED; 2])
    }
}

fn release() {
    let mut slots = SLOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for slot in slots.iter_mut() {
        if slot.is_some_and(|s| s.source == Source::Switch2) {
            *slot = None;
        }
    }
}

fn session(bulk: &Bulk) {
    let sticks = [bulk.stick(0x13080, 0x1fc040), bulk.stick(0x130c0, 0x1fc080)];
    let mut reply = [0; 64];
    for command in INIT {
        if !bulk.send(&command[..usize::from(command[5]) + 8]) {
            return;
        }
        bulk.receive(&mut reply);
    }
    let Some(device) = hidapi::HidApi::new()
        .ok()
        .and_then(|api| api.open(VENDOR, PRO).ok())
    else {
        return;
    };
    let mut report = [0; 64];
    let mut silent = 0;
    while silent < 10 {
        match device.read_timeout(&mut report, 100) {
            Ok(read) => match decode(&report[..read], &sticks) {
                Some(state) => {
                    silent = 0;
                    let mut slots = SLOTS
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    publish(&mut slots, Source::Switch2, state, || {
                        bevy::log::info!(
                            "skate: gamepad 'Switch 2 Pro Controller' ({VENDOR:04x}:{PRO:04x}, USB)"
                        );
                    });
                }
                None => silent += 1,
            },
            Err(_) => return,
        }
    }
}

pub(super) fn run() {
    loop {
        if let Some(bulk) = Bulk::open() {
            session(&bulk);
            release();
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
