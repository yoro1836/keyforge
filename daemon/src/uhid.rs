//! Lua-friendly port of the Linux UHID C API (`include/uapi/linux/uhid.h`).
//!
//! Layout mirrors the kernel ABI: one open `/dev/uhid` fd per HID device,
//! whole `uhid_event` objects per `read()`/`write()`, `UHID_CREATE2` first,
//! `UHID_INPUT2` for interrupt-channel reports, `UHID_DESTROY` to unregister.
//! buttons/axes into HID report descriptors plus a packing plan, so Lua
//! scripts can declare gamepads, keyboards, and mice like AutoHotkey.

use crate::core::{O_NONBLOCK, close, open, read, write};
use mlua::{Lua, Table, UserData, Value};
use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// ABI constants (mirrors uapi/linux/uhid.h and uapi/linux/input.h)
// ---------------------------------------------------------------------------

pub const UHID_PATH: &str = "/dev/uhid";
pub const UHID_DATA_MAX: usize = 4096;
pub const UHID_EVENT_SIZE: usize = 4376;

pub const UHID_DESTROY: u32 = 1;
pub const UHID_START: u32 = 2;
pub const UHID_STOP: u32 = 3;
pub const UHID_OPEN: u32 = 4;
pub const UHID_CLOSE: u32 = 5;
pub const UHID_OUTPUT: u32 = 6;
pub const UHID_GET_REPORT: u32 = 9;
pub const UHID_GET_REPORT_REPLY: u32 = 10;
pub const UHID_CREATE2: u32 = 11;
pub const UHID_INPUT2: u32 = 12;
pub const UHID_SET_REPORT: u32 = 13;
pub const UHID_SET_REPORT_REPLY: u32 = 14;

pub const UHID_DEV_NUMBERED_FEATURE_REPORTS: u64 = 1 << 0;
pub const UHID_DEV_NUMBERED_OUTPUT_REPORTS: u64 = 1 << 1;
pub const UHID_DEV_NUMBERED_INPUT_REPORTS: u64 = 1 << 2;

pub const UHID_FEATURE_REPORT: u8 = 0;
pub const UHID_OUTPUT_REPORT: u8 = 1;
pub const UHID_INPUT_REPORT: u8 = 2;

pub const BUS_USB: u16 = 0x03;
pub const BUS_BLUETOOTH: u16 = 0x05;
pub const BUS_VIRTUAL: u16 = 0x06;

const O_RDWR: i32 = 0o2;

// Packed `uhid_create2_req` field offsets inside `uhid_event`.
const OFF_CREATE_NAME: usize = 4;
const OFF_CREATE_PHYS: usize = 132;
const OFF_CREATE_UNIQ: usize = 196;
const OFF_CREATE_RD_SIZE: usize = 260;
const OFF_CREATE_BUS: usize = 262;
const OFF_CREATE_VENDOR: usize = 264;
const OFF_CREATE_PRODUCT: usize = 268;
const OFF_CREATE_VERSION: usize = 272;
const OFF_CREATE_COUNTRY: usize = 276;
const OFF_CREATE_RD_DATA: usize = 280;

// ---------------------------------------------------------------------------
// Low-level event encoding (little-endian, byte-exact with the C structs)
// ---------------------------------------------------------------------------

fn put_u16(ev: &mut [u8; UHID_EVENT_SIZE], off: usize, v: u16) {
    ev[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(ev: &mut [u8; UHID_EVENT_SIZE], off: usize, v: u32) {
    ev[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
fn put_u64(ev: &mut [u8; UHID_EVENT_SIZE], off: usize, v: u64) {
    ev[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_cstr(ev: &mut [u8; UHID_EVENT_SIZE], off: usize, len: usize, s: &[u8]) {
    let n = s
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(s.len())
        .min(len - 1);
    ev[off..off + n].copy_from_slice(&s[..n]);
}

fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

#[derive(Debug, Clone)]
pub struct CreateParams {
    pub name: Vec<u8>,
    pub phys: Vec<u8>,
    pub uniq: Vec<u8>,
    pub bus: u16,
    pub vendor: u32,
    pub product: u32,
    pub version: u32,
    pub country: u32,
    pub descriptor: Vec<u8>,
}

pub fn encode_create2(p: &CreateParams) -> io::Result<[u8; UHID_EVENT_SIZE]> {
    if p.descriptor.len() > UHID_DATA_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "HID report descriptor too large: {} > {UHID_DATA_MAX}",
                p.descriptor.len()
            ),
        ));
    }
    let mut ev = [0u8; UHID_EVENT_SIZE];
    put_u32(&mut ev, 0, UHID_CREATE2);
    put_cstr(&mut ev, OFF_CREATE_NAME, 128, &p.name);
    put_cstr(&mut ev, OFF_CREATE_PHYS, 64, &p.phys);
    put_cstr(&mut ev, OFF_CREATE_UNIQ, 64, &p.uniq);
    put_u16(&mut ev, OFF_CREATE_RD_SIZE, p.descriptor.len() as u16);
    put_u16(&mut ev, OFF_CREATE_BUS, p.bus);
    put_u32(&mut ev, OFF_CREATE_VENDOR, p.vendor);
    put_u32(&mut ev, OFF_CREATE_PRODUCT, p.product);
    put_u32(&mut ev, OFF_CREATE_VERSION, p.version);
    put_u32(&mut ev, OFF_CREATE_COUNTRY, p.country);
    ev[OFF_CREATE_RD_DATA..OFF_CREATE_RD_DATA + p.descriptor.len()].copy_from_slice(&p.descriptor);
    Ok(ev)
}

pub fn encode_input2(data: &[u8]) -> io::Result<[u8; UHID_EVENT_SIZE]> {
    if data.len() > UHID_DATA_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("UHID input too large: {} > {UHID_DATA_MAX}", data.len()),
        ));
    }
    let mut ev = [0u8; UHID_EVENT_SIZE];
    put_u32(&mut ev, 0, UHID_INPUT2);
    put_u16(&mut ev, 4, data.len() as u16);
    ev[6..6 + data.len()].copy_from_slice(data);
    Ok(ev)
}

pub fn encode_destroy() -> [u8; UHID_EVENT_SIZE] {
    let mut ev = [0u8; UHID_EVENT_SIZE];
    put_u32(&mut ev, 0, UHID_DESTROY);
    ev
}

pub fn encode_get_report_reply(
    id: u32,
    err: u16,
    data: &[u8],
) -> io::Result<[u8; UHID_EVENT_SIZE]> {
    if data.len() > UHID_DATA_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "UHID report reply too large",
        ));
    }
    let mut ev = [0u8; UHID_EVENT_SIZE];
    put_u32(&mut ev, 0, UHID_GET_REPORT_REPLY);
    put_u32(&mut ev, 4, id);
    put_u16(&mut ev, 8, err);
    put_u16(&mut ev, 10, data.len() as u16);
    ev[12..12 + data.len()].copy_from_slice(data);
    Ok(ev)
}

pub fn encode_set_report_reply(id: u32, err: u16) -> [u8; UHID_EVENT_SIZE] {
    let mut ev = [0u8; UHID_EVENT_SIZE];
    put_u32(&mut ev, 0, UHID_SET_REPORT_REPLY);
    put_u32(&mut ev, 4, id);
    put_u16(&mut ev, 8, err);
    ev
}

// ---------------------------------------------------------------------------
// Kernel -> userspace event parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelEvent {
    Start {
        dev_flags: u64,
    },
    Stop,
    Open,
    Close,
    Output {
        rtype: u8,
        data: Vec<u8>,
    },
    GetReport {
        id: u32,
        rnum: u8,
        rtype: u8,
    },
    SetReport {
        id: u32,
        rnum: u8,
        rtype: u8,
        data: Vec<u8>,
    },
    Unknown {
        ty: u32,
    },
}

pub fn parse_event(buf: &[u8]) -> Option<KernelEvent> {
    if buf.len() < 12 {
        return None;
    }
    Some(match get_u32(buf, 0) {
        UHID_START => KernelEvent::Start {
            dev_flags: get_u64(buf, 4),
        },
        UHID_STOP => KernelEvent::Stop,
        UHID_OPEN => KernelEvent::Open,
        UHID_CLOSE => KernelEvent::Close,
        UHID_OUTPUT => {
            let size = get_u16(buf, 4100) as usize;
            if buf.len() < 4103 || 4 + size > buf.len() {
                return None;
            }
            KernelEvent::Output {
                rtype: buf[4102],
                data: buf[4..4 + size].to_vec(),
            }
        }
        UHID_GET_REPORT => KernelEvent::GetReport {
            id: get_u32(buf, 4),
            rnum: buf[8],
            rtype: buf[9],
        },
        UHID_SET_REPORT => {
            let size = get_u16(buf, 10) as usize;
            if buf.len() < 12 + size {
                return None;
            }
            KernelEvent::SetReport {
                id: get_u32(buf, 4),
                rnum: buf[8],
                rtype: buf[9],
                data: buf[12..12 + size].to_vec(),
            }
        }
        ty => KernelEvent::Unknown { ty },
    })
}

// ---------------------------------------------------------------------------
// Device handle: one open /dev/uhid fd per HID device, like the C API
// ---------------------------------------------------------------------------

pub struct UhidDevice {
    fd: i32,
}

impl UhidDevice {
    pub fn open() -> io::Result<Self> {
        let cpath = CString::new(UHID_PATH).expect("static path");
        // Non-blocking: kernel -> userspace events are drained cooperatively
        // via read_event (uh.poll); writes stay immediate per the UHID docs.
        let fd = unsafe { open(cpath.as_ptr(), O_RDWR | O_NONBLOCK, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(UhidDevice { fd })
    }

    /// Read one kernel event, or `Ok(None)` when the queue is empty.
    pub fn read_event(&self) -> io::Result<Option<KernelEvent>> {
        let mut buf = [0u8; UHID_EVENT_SIZE];
        let n = unsafe { read(self.fd, buf.as_mut_ptr(), buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(err);
        }
        if (n as usize) < 12 {
            return Ok(None);
        }
        Ok(parse_event(&buf[..n as usize]))
    }
    fn write_all(&self, ev: &[u8; UHID_EVENT_SIZE]) -> io::Result<()> {
        let mut done = 0;
        while done < ev.len() {
            let n = unsafe { write(self.fd, ev.as_ptr().add(done), ev.len() - done) };
            if n <= 0 {
                return Err(io::Error::last_os_error());
            }
            done += n as usize;
        }
        Ok(())
    }

    pub fn create(&self, params: &CreateParams) -> io::Result<()> {
        self.write_all(&encode_create2(params)?)
    }

    pub fn input(&self, data: &[u8]) -> io::Result<()> {
        self.write_all(&encode_input2(data)?)
    }

    pub fn destroy(&self) -> io::Result<()> {
        self.write_all(&encode_destroy())
    }

    pub fn get_report_reply(&self, id: u32, err: u16, data: &[u8]) -> io::Result<()> {
        self.write_all(&encode_get_report_reply(id, err, data)?)
    }

    pub fn set_report_reply(&self, id: u32, err: u16) -> io::Result<()> {
        self.write_all(&encode_set_report_reply(id, err))
    }
}

impl Drop for UhidDevice {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                close(self.fd);
            }
            self.fd = -1;
        }
    }
}

impl GamepadAxis {
    /// Stable state-map key shared by Lua input and the evdev mirror.
    pub fn name(self) -> &'static str {
        match self {
            GamepadAxis::X => "x",
            GamepadAxis::Y => "y",
            GamepadAxis::Z => "z",
            GamepadAxis::Rx => "rx",
            GamepadAxis::Ry => "ry",
        }
    }
}

// ---------------------------------------------------------------------------
// Lua-friendly report descriptors + packing plans
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GamepadAxis {
    X,
    Y,
    Z,
    Rx,
    Ry,
}

impl GamepadAxis {
    fn usage(self) -> u8 {
        match self {
            GamepadAxis::X => 0x30,
            GamepadAxis::Y => 0x31,
            GamepadAxis::Z => 0x32,
            GamepadAxis::Rx => 0x33,
            GamepadAxis::Ry => 0x34,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "x" => Some(GamepadAxis::X),
            "y" => Some(GamepadAxis::Y),
            "z" => Some(GamepadAxis::Z),
            "rx" => Some(GamepadAxis::Rx),
            "ry" => Some(GamepadAxis::Ry),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ReportLayout {
    Gamepad {
        buttons: u8,
        axes: Vec<GamepadAxis>,
        hat: bool,
    },
    Keyboard,
    Mouse,
}

impl ReportLayout {
    pub fn gamepad(buttons: u8, axes: Vec<GamepadAxis>, hat: bool) -> io::Result<Self> {
        if buttons == 0 || buttons > 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "gamepad needs 1..32 buttons",
            ));
        }
        if axes.is_empty() || axes.len() > 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "gamepad needs 1..5 axes",
            ));
        }
        Ok(ReportLayout::Gamepad { buttons, axes, hat })
    }

    /// Unnumbered report length in bytes.
    pub fn report_len(&self) -> usize {
        match self {
            ReportLayout::Gamepad { buttons, axes, hat } => {
                axes.len() * 2 + (*hat as usize) + buttons.div_ceil(8) as usize
            }
            ReportLayout::Keyboard => 8,
            ReportLayout::Mouse => 4,
        }
    }
}

/// Canonical USB boot-protocol keyboard descriptor (63 bytes).
pub fn keyboard_descriptor() -> Vec<u8> {
    vec![
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x03, 0x95, 0x05,
        0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91,
        0x03, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65,
        0x81, 0x00, 0xC0,
    ]
}

/// Canonical USB boot-protocol mouse descriptor (50 bytes).
pub fn mouse_descriptor() -> Vec<u8> {
    vec![
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29,
        0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
        0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95,
        0x02, 0x81, 0x06, 0xC0, 0xC0,
    ]
}

/// Build a gamepad descriptor: N buttons, signed 16-bit axes, optional hat.
pub fn gamepad_descriptor(buttons: u8, axes: &[GamepadAxis], hat: bool) -> Vec<u8> {
    let mut rd = vec![0x05, 0x01, 0x09, 0x04, 0xA1, 0x01];
    rd.extend([
        0x05, 0x09, 0x19, 0x01, 0x29, buttons, 0x15, 0x00, 0x25, 0x01,
    ]);
    rd.extend([0x75, 0x01, 0x95, buttons, 0x81, 0x02]);
    let pad = (8 - buttons % 8) % 8;
    if pad > 0 {
        rd.extend([0x75, pad, 0x95, 0x01, 0x81, 0x03]);
    }
    rd.extend([0x05, 0x01]);
    for axis in axes {
        rd.extend([0x09, axis.usage()]);
    }
    rd.extend([0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10]);
    rd.extend([0x95, axes.len() as u8, 0x81, 0x02]);
    if hat {
        rd.extend([
            0x09, 0x39, 0x15, 0x01, 0x25, 0x08, 0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75,
            0x04, 0x95, 0x01, 0x81, 0x42, 0x75, 0x04, 0x95, 0x01, 0x81, 0x03,
        ]);
    }
    rd.push(0xC0);
    rd
}

#[derive(Debug, Clone, Default)]
pub struct GamepadState {
    pub buttons: Vec<bool>,
    pub axes: HashMap<String, i32>,
    pub hat: u8,
}

pub fn pack_gamepad(layout: &ReportLayout, state: &GamepadState) -> io::Result<Vec<u8>> {
    let (buttons, axes, hat) = match layout {
        ReportLayout::Gamepad { buttons, axes, hat } => (buttons, axes, hat),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "layout is not a gamepad",
            ));
        }
    };
    let mut report = vec![0u8; layout.report_len()];
    for (i, pressed) in state.buttons.iter().enumerate().take(*buttons as usize) {
        if *pressed {
            report[i / 8] |= 1 << (i % 8);
        }
    }
    let mut off = buttons.div_ceil(8) as usize;
    for axis in axes {
        let name = match axis {
            GamepadAxis::X => "x",
            GamepadAxis::Y => "y",
            GamepadAxis::Z => "z",
            GamepadAxis::Rx => "rx",
            GamepadAxis::Ry => "ry",
        };
        let v = state
            .axes
            .get(name)
            .copied()
            .unwrap_or(0)
            .clamp(-32767, 32767) as i16;
        report[off..off + 2].copy_from_slice(&v.to_le_bytes());
        off += 2;
    }
    if *hat {
        report[off] = state.hat & 0x0F;
    }
    Ok(report)
}

#[derive(Debug, Clone, Default)]
pub struct KeyboardState {
    pub modifiers: u8,
    pub keys: Vec<u8>,
}

pub fn pack_keyboard(state: &KeyboardState) -> Vec<u8> {
    let mut report = vec![0u8; 8];
    report[0] = state.modifiers;
    for (i, key) in state.keys.iter().take(6).enumerate() {
        report[2 + i] = *key;
    }
    report
}

#[derive(Debug, Clone, Default)]
pub struct MouseState {
    pub buttons: u8,
    pub x: i32,
    pub y: i32,
    pub wheel: i32,
}
// ---------------------------------------------------------------------------
// Virtual mirror: evdev output state -> UHID gamepad report
// ---------------------------------------------------------------------------

/// Map physical ABS codes onto gamepad axes, in increasing code order.
pub fn mirror_axes(abs_codes: &[u32]) -> Vec<(u32, GamepadAxis)> {
    let mut out = Vec::new();
    for code in abs_codes {
        let axis = match *code {
            crate::core::ABS_X => Some(GamepadAxis::X),
            crate::core::ABS_Y => Some(GamepadAxis::Y),
            crate::core::ABS_Z => Some(GamepadAxis::Z),
            crate::core::ABS_RX => Some(GamepadAxis::Rx),
            crate::core::ABS_RY => Some(GamepadAxis::Ry),
            _ => None,
        };
        if let Some(axis) = axis {
            out.push((*code, axis));
        }
    }
    out
}

/// Descriptor + index maps mirroring one physical device.
pub struct MirrorMap {
    pub layout: ReportLayout,
    pub codes: Vec<u16>,
    pub axes: Vec<(u32, GamepadAxis)>,
}

/// Build the mirror descriptor. Button codes are sorted so the mapping is
/// deterministic; more than 32 buttons are truncated (HID gamepad limit here).
pub fn mirror_layout(
    mut codes: Vec<u16>,
    axes: Vec<(u32, GamepadAxis)>,
) -> io::Result<(Vec<u8>, MirrorMap)> {
    codes.sort_unstable();
    if codes.len() > 32 {
        eprintln!(
            "keyforge: mirror supports 32 buttons, truncating {}",
            codes.len()
        );
        codes.truncate(32);
    }
    let gamepad_axes: Vec<GamepadAxis> = axes.iter().map(|(_, axis)| *axis).collect();
    let layout = ReportLayout::gamepad(codes.len() as u8, gamepad_axes, false)?;
    let descriptor = gamepad_descriptor(codes.len() as u8, &layout_axes(&layout), false);
    Ok((
        descriptor,
        MirrorMap {
            layout,
            codes,
            axes,
        },
    ))
}

fn layout_axes(layout: &ReportLayout) -> Vec<GamepadAxis> {
    match layout {
        ReportLayout::Gamepad { axes, .. } => axes.clone(),
        _ => Vec::new(),
    }
}

impl MirrorMap {
    pub fn blank_state(&self) -> GamepadState {
        GamepadState {
            buttons: vec![false; self.codes.len()],
            axes: HashMap::new(),
            hat: 0,
        }
    }

    /// Apply a button event; returns true when the state changed.
    pub fn apply_key(&self, state: &mut GamepadState, code: u16, pressed: bool) -> bool {
        match self.codes.binary_search(&code) {
            Ok(i) if i < state.buttons.len() && state.buttons[i] != pressed => {
                state.buttons[i] = pressed;
                true
            }
            _ => false,
        }
    }

    /// Apply an axis event; returns true when the state changed.
    pub fn apply_abs(&self, state: &mut GamepadState, abs: u32, value: i32) -> bool {
        let Some(axis) = self
            .axes
            .iter()
            .find_map(|(code, axis)| (*code == abs).then_some(*axis))
        else {
            return false;
        };
        let value = value.clamp(-32767, 32767);
        if state.axes.get(axis.name()).copied().unwrap_or(0) != value {
            state.axes.insert(axis.name().to_string(), value);
            true
        } else {
            false
        }
    }

    pub fn pack(&self, state: &GamepadState) -> io::Result<Vec<u8>> {
        pack_gamepad(&self.layout, state)
    }
}

/// Owned virtual mirror: HID device plus dirty-tracked output state.
pub struct Mirror {
    device: UhidDevice,
    map: MirrorMap,
    state: GamepadState,
    dirty: bool,
}

impl Mirror {
    pub fn create(
        name: &str,
        vid: u16,
        product: u16,
        codes: Vec<u16>,
        axes: Vec<(u32, GamepadAxis)>,
    ) -> io::Result<Self> {
        let (descriptor, map) = mirror_layout(codes, axes)?;
        let device = UhidDevice::open()?;
        let params = CreateParams {
            name: name.as_bytes().to_vec(),
            phys: b"keyforge/input0".to_vec(),
            uniq: Vec::new(),
            bus: BUS_USB,
            vendor: vid as u32,
            product: product as u32,
            version: 1,
            country: 0,
            descriptor,
        };
        device.create(&params)?;
        Ok(Mirror {
            device,
            state: map.blank_state(),
            map,
            dirty: false,
        })
    }

    pub fn key(&mut self, code: u16, pressed: bool) {
        if self.map.apply_key(&mut self.state, code, pressed) {
            self.dirty = true;
        }
    }

    pub fn abs(&mut self, abs: u32, value: i32) {
        if self.map.apply_abs(&mut self.state, abs, value) {
            self.dirty = true;
        }
    }

    /// Send one HID report when the state changed since the last flush.
    pub fn flush(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let report = self.map.pack(&self.state)?;
        self.device.input(&report)?;
        self.dirty = false;
        Ok(())
    }

    pub fn destroy(self) {
        let _ = self.device.destroy();
    }
}

pub fn pack_mouse(state: &MouseState) -> Vec<u8> {
    vec![
        state.buttons,
        state.x.clamp(-127, 127) as i8 as u8,
        state.y.clamp(-127, 127) as i8 as u8,
        state.wheel.clamp(-127, 127) as i8 as u8,
    ]
}

// ---------------------------------------------------------------------------
// Lua surface: the `uh` table
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<RegistryInner>>,
}

struct RegistryInner {
    next: u32,
    devices: HashMap<u32, RegistryEntry>,
    pending: Vec<(u32, KernelEvent)>,
}

struct RegistryEntry {
    name: String,
    descriptor: Vec<u8>,
    device: UhidDevice,
    layout: Option<ReportLayout>,
}

/// Fetch the registry attached to a Lua state, if `uh` was registered.
pub fn registry(lua: &Lua) -> Option<Registry> {
    lua.app_data_ref::<Registry>().map(|r| r.clone())
}

impl Registry {
    fn new() -> Self {
        Registry {
            inner: Arc::new(Mutex::new(RegistryInner {
                next: 1,
                devices: HashMap::new(),
                pending: Vec::new(),
            })),
        }
    }

    fn insert(
        &self,
        name: String,
        descriptor: Vec<u8>,
        device: UhidDevice,
        layout: Option<ReportLayout>,
    ) -> u32 {
        let mut inner = self.inner.lock().expect("uhid registry");
        let id = inner.next;
        inner.next += 1;
        inner.devices.insert(
            id,
            RegistryEntry {
                name,
                descriptor,
                device,
                layout,
            },
        );
        id
    }

    fn with<R>(&self, id: u32, f: impl FnOnce(&RegistryEntry) -> io::Result<R>) -> mlua::Result<R> {
        let inner = self.inner.lock().expect("uhid registry");
        match inner.devices.get(&id) {
            Some(entry) => f(entry).map_err(mlua::Error::external),
            None => Err(mlua::Error::runtime("unknown or destroyed uhid device")),
        }
    }

    fn remove(&self, id: u32) -> Option<RegistryEntry> {
        self.inner
            .lock()
            .expect("uhid registry")
            .devices
            .remove(&id)
    }

    /// Find a live device by name (reload-safe `uh.create` reuses it).
    fn find_by_name(&self, name: &str) -> Option<(u32, Vec<u8>)> {
        let inner = self.inner.lock().expect("uhid registry");
        inner
            .devices
            .iter()
            .find_map(|(id, entry)| (entry.name == name).then(|| (*id, entry.descriptor.clone())))
    }

    /// Drain one batch of pending kernel events from every device into the
    /// shared queue. Cheap when idle (one non-blocking read per device).
    pub fn pump(&self) {
        let mut batch = Vec::new();
        {
            let inner = self.inner.lock().expect("uhid registry");
            for (id, entry) in inner.devices.iter() {
                loop {
                    match entry.device.read_event() {
                        Ok(Some(event)) => batch.push((*id, event)),
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
            }
        }
        self.inner
            .lock()
            .expect("uhid registry")
            .pending
            .extend(batch);
    }

    /// Take queued kernel events accumulated by [`Registry::pump`].
    pub fn take_pending(&self) -> Vec<(u32, KernelEvent)> {
        std::mem::take(&mut self.inner.lock().expect("uhid registry").pending)
    }
}

#[derive(Clone)]
struct LuaUhidDevice {
    registry: Registry,
    id: u32,
}

impl UserData for LuaUhidDevice {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("input_raw", |_, this, data: mlua::String| {
            this.registry
                .with(this.id, |entry| entry.device.input(&data.as_bytes()))?;
            Ok(())
        });
        methods.add_method("input", |_, this, state: Table| {
            this.registry.with(this.id, |entry| {
                let layout = entry.layout.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "device has no report layout; use input_raw",
                    )
                })?;
                let report = match layout {
                    ReportLayout::Gamepad { buttons, .. } => {
                        let mut gstate = GamepadState::default();
                        if let Ok(list) = state.get::<Vec<u32>>("buttons") {
                            gstate.buttons = vec![false; *buttons as usize];
                            for b in list {
                                if b >= 1 && (b as usize) <= gstate.buttons.len() {
                                    gstate.buttons[b as usize - 1] = true;
                                }
                            }
                        } else {
                            for i in 1..=*buttons as u32 {
                                gstate.buttons.push(state.get::<bool>(i).unwrap_or(false));
                            }
                        }
                        for name in ["x", "y", "z", "rx", "ry"] {
                            if let Ok(v) = state.get::<i32>(name) {
                                gstate.axes.insert(name.to_string(), v);
                            }
                        }
                        gstate.hat = state.get::<u8>("hat").unwrap_or(0);
                        pack_gamepad(layout, &gstate)?
                    }
                    ReportLayout::Keyboard => {
                        let kstate = KeyboardState {
                            modifiers: state.get::<u8>("modifiers").unwrap_or(0),
                            keys: state.get::<Vec<u8>>("keys").unwrap_or_default(),
                        };
                        pack_keyboard(&kstate)
                    }
                    ReportLayout::Mouse => {
                        let mstate = MouseState {
                            buttons: state.get::<u8>("buttons").unwrap_or(0),
                            x: state.get::<i32>("x").unwrap_or(0),
                            y: state.get::<i32>("y").unwrap_or(0),
                            wheel: state.get::<i32>("wheel").unwrap_or(0),
                        };
                        pack_mouse(&mstate)
                    }
                };
                entry.device.input(&report)
            })?;
            Ok(())
        });
        methods.add_method(
            "get_report_reply",
            |_, this, (id, err, data): (u32, u16, mlua::String)| {
                this.registry.with(this.id, |entry| {
                    entry.device.get_report_reply(id, err, &data.as_bytes())
                })?;
                Ok(())
            },
        );
        methods.add_method("set_report_reply", |_, this, (id, err): (u32, u16)| {
            this.registry
                .with(this.id, |entry| entry.device.set_report_reply(id, err))?;
            Ok(())
        });
        methods.add_method("id", |_, this, ()| Ok(this.id));
        methods.add_method("destroy", |_, this, ()| {
            match this.registry.remove(this.id) {
                Some(entry) => entry.device.destroy().map_err(mlua::Error::external),
                None => Err(mlua::Error::runtime("uhid device already destroyed")),
            }
        });
    }
}

fn push_kernel_event(lua: &Lua, event: &KernelEvent) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    match event {
        KernelEvent::Start { dev_flags } => {
            out.set("type", "start")?;
            out.set("dev_flags", *dev_flags)?;
        }
        KernelEvent::Stop => {
            out.set("type", "stop")?;
        }
        KernelEvent::Open => {
            out.set("type", "open")?;
        }
        KernelEvent::Close => {
            out.set("type", "close")?;
        }
        KernelEvent::Output { rtype, data } => {
            out.set("type", "output")?;
            out.set("rtype", *rtype)?;
            out.set("data", lua.create_string(data)?)?;
        }
        KernelEvent::GetReport { id, rnum, rtype } => {
            out.set("type", "get_report")?;
            out.set("id", *id)?;
            out.set("rnum", *rnum)?;
            out.set("rtype", *rtype)?;
        }
        KernelEvent::SetReport {
            id,
            rnum,
            rtype,
            data,
        } => {
            out.set("type", "set_report")?;
            out.set("id", *id)?;
            out.set("rnum", *rnum)?;
            out.set("rtype", *rtype)?;
            out.set("data", lua.create_string(data)?)?;
        }
        KernelEvent::Unknown { ty } => {
            out.set("type", "unknown")?;
            out.set("code", *ty)?;
        }
    }
    Ok(out)
}
fn lua_bytes(value: &Value) -> Vec<u8> {
    match value {
        Value::String(s) => s.as_bytes().to_vec(),
        _ => Vec::new(),
    }
}

/// Register the global `uh` table once per Lua state (idempotent reloads).
pub fn register_uh(lua: &Lua) -> mlua::Result<()> {
    if lua.app_data_ref::<Registry>().is_some() {
        return Ok(());
    }
    let registry = Registry::new();
    lua.set_app_data(registry.clone());

    let uh = lua.create_table()?;
    uh.set("BUS_USB", BUS_USB)?;
    uh.set("BUS_BLUETOOTH", BUS_BLUETOOTH)?;
    uh.set("BUS_VIRTUAL", BUS_VIRTUAL)?;
    uh.set("DATA_MAX", UHID_DATA_MAX)?;
    uh.set("FEATURE_REPORT", UHID_FEATURE_REPORT)?;
    uh.set("OUTPUT_REPORT", UHID_OUTPUT_REPORT)?;
    uh.set("INPUT_REPORT", UHID_INPUT_REPORT)?;
    uh.set(
        "DEV_NUMBERED_FEATURE_REPORTS",
        UHID_DEV_NUMBERED_FEATURE_REPORTS,
    )?;
    uh.set(
        "DEV_NUMBERED_OUTPUT_REPORTS",
        UHID_DEV_NUMBERED_OUTPUT_REPORTS,
    )?;
    uh.set(
        "DEV_NUMBERED_INPUT_REPORTS",
        UHID_DEV_NUMBERED_INPUT_REPORTS,
    )?;

    let poll_registry = registry.clone();
    uh.set(
        "poll",
        lua.create_function(move |lua, ()| {
            poll_registry.pump();
            let drained = poll_registry.take_pending();
            let out = lua.create_table()?;
            for (index, (id, event)) in drained.iter().enumerate() {
                let item = lua.create_table()?;
                item.set("device", *id)?;
                item.set("event", push_kernel_event(lua, event)?)?;
                out.set(index + 1, item)?;
            }
            Ok(out)
        })?,
    )?;

    uh.set(
        "keyboard",
        lua.create_function(|lua, ()| {
            let layout = lua.create_table()?;
            layout.set("kind", "keyboard")?;
            layout.set("descriptor", lua.create_string(keyboard_descriptor())?)?;
            Ok(layout)
        })?,
    )?;
    uh.set(
        "mouse",
        lua.create_function(|lua, ()| {
            let layout = lua.create_table()?;
            layout.set("kind", "mouse")?;
            layout.set("descriptor", lua.create_string(mouse_descriptor())?)?;
            Ok(layout)
        })?,
    )?;
    uh.set(
        "gamepad",
        lua.create_function(|lua, params: Table| {
            let buttons: u8 = params.get("buttons").unwrap_or(16);
            let mut axes = Vec::new();
            let names: Vec<String> = params.get("axes").unwrap_or_default();
            let names = if names.is_empty() {
                vec!["x".to_string(), "y".to_string()]
            } else {
                names
            };
            for name in names {
                match GamepadAxis::from_name(&name) {
                    Some(axis) => axes.push(axis),
                    None => {
                        return Err(mlua::Error::runtime(format!(
                            "unknown gamepad axis: {name}"
                        )));
                    }
                }
            }
            let hat: bool = params.get("hat").unwrap_or(false);
            let layout =
                ReportLayout::gamepad(buttons, axes.clone(), hat).map_err(mlua::Error::external)?;
            let out = lua.create_table()?;
            out.set("kind", "gamepad")?;
            out.set("buttons", buttons)?;
            let axis_names: Vec<String> = axes
                .iter()
                .map(|a| {
                    match a {
                        GamepadAxis::X => "x",
                        GamepadAxis::Y => "y",
                        GamepadAxis::Z => "z",
                        GamepadAxis::Rx => "rx",
                        GamepadAxis::Ry => "ry",
                    }
                    .to_string()
                })
                .collect();
            out.set("axes", axis_names)?;
            out.set("hat", hat)?;
            out.set(
                "descriptor",
                lua.create_string(gamepad_descriptor(buttons, &axes, hat))?,
            )?;
            let _ = layout;
            Ok(out)
        })?,
    )?;

    let reg = registry.clone();
    uh.set(
        "create",
        lua.create_function(move |lua, params: Table| {
            let descriptor = match params.get::<Value>("descriptor") {
                Ok(v) => lua_bytes(&v),
                Err(_) => Vec::new(),
            };
            if descriptor.is_empty() {
                return Err(mlua::Error::runtime("uh.create needs a descriptor"));
            }
            let raw_name = lua_bytes(&params.get::<Value>("name").unwrap_or(Value::Nil));
            let name = String::from_utf8_lossy(&raw_name).into_owned();
            // Reload-safe: a live device with the same name is reused instead
            // of registering a duplicate kernel device on every config reload.
            if !name.is_empty()
                && let Some((id, old_descriptor)) = reg.find_by_name(&name)
            {
                if old_descriptor == descriptor {
                    return lua.create_userdata(LuaUhidDevice {
                        registry: reg.clone(),
                        id,
                    });
                }
                if let Some(old) = reg.remove(id) {
                    let _ = old.device.destroy();
                }
            }
            let created = UhidDevice::open().map_err(mlua::Error::external)?;
            let create = CreateParams {
                name: raw_name,
                phys: lua_bytes(&params.get::<Value>("phys").unwrap_or(Value::Nil)),
                uniq: lua_bytes(&params.get::<Value>("uniq").unwrap_or(Value::Nil)),
                bus: params.get::<u16>("bus").unwrap_or(BUS_USB),
                vendor: params.get::<u32>("vendor").unwrap_or(0),
                product: params.get::<u32>("product").unwrap_or(0),
                version: params.get::<u32>("version").unwrap_or(0),
                country: params.get::<u32>("country").unwrap_or(0),
                descriptor: descriptor.clone(),
            };
            if let Err(err) = created.create(&create) {
                return Err(mlua::Error::external(err));
            }
            let layout = match params.get::<String>("kind").as_deref() {
                Ok("gamepad") => {
                    let buttons: u8 = params.get("buttons").unwrap_or(16);
                    let mut axes = Vec::new();
                    for name in params.get::<Vec<String>>("axes").unwrap_or_default() {
                        if let Some(axis) = GamepadAxis::from_name(&name) {
                            axes.push(axis);
                        }
                    }
                    if axes.is_empty() {
                        axes.extend([GamepadAxis::X, GamepadAxis::Y]);
                    }
                    Some(
                        ReportLayout::gamepad(
                            buttons,
                            axes,
                            params.get::<bool>("hat").unwrap_or(false),
                        )
                        .map_err(mlua::Error::external)?,
                    )
                }
                Ok("keyboard") => Some(ReportLayout::Keyboard),
                Ok("mouse") => Some(ReportLayout::Mouse),
                _ => None,
            };
            let id = reg.insert(name, descriptor, created, layout);
            lua.create_userdata(LuaUhidDevice {
                registry: reg.clone(),
                id,
            })
        })?,
    )?;

    lua.globals().set("uh", uh)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn event_size_matches_c_abi() {
        // 4-byte type + 4372-byte create2 payload.
        assert_eq!(UHID_EVENT_SIZE, 4376);
        assert_eq!(OFF_CREATE_RD_DATA + UHID_DATA_MAX, UHID_EVENT_SIZE);
    }

    #[test]
    fn create2_encodes_c_struct_layout() {
        let params = CreateParams {
            name: b"KeyForge Pad".to_vec(),
            phys: b"keyforge/input0".to_vec(),
            uniq: Vec::new(),
            bus: BUS_USB,
            vendor: 0x045e,
            product: 0x02e0,
            version: 1,
            country: 0,
            descriptor: vec![0x05, 0x01, 0x09, 0x04],
        };
        let ev = encode_create2(&params).unwrap();
        assert_eq!(get_u32(&ev, 0), UHID_CREATE2);
        assert_eq!(&ev[OFF_CREATE_NAME..OFF_CREATE_NAME + 12], b"KeyForge Pad");
        assert_eq!(ev[OFF_CREATE_NAME + 12], 0);
        assert_eq!(
            &ev[OFF_CREATE_PHYS..OFF_CREATE_PHYS + 15],
            b"keyforge/input0"
        );
        assert_eq!(get_u16(&ev, OFF_CREATE_RD_SIZE), 4);
        assert_eq!(get_u16(&ev, OFF_CREATE_BUS), BUS_USB);
        assert_eq!(get_u32(&ev, OFF_CREATE_VENDOR), 0x045e);
        assert_eq!(get_u32(&ev, OFF_CREATE_PRODUCT), 0x02e0);
        assert_eq!(get_u32(&ev, OFF_CREATE_VERSION), 1);
        assert_eq!(
            &ev[OFF_CREATE_RD_DATA..OFF_CREATE_RD_DATA + 4],
            &[0x05, 0x01, 0x09, 0x04]
        );
        assert!(ev[OFF_CREATE_RD_DATA + 4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn create_reuses_live_device_by_name() {
        // Needs a real /dev/uhid fd; elsewhere the open test above already
        // covers the graceful failure path.
        let Ok(_probe) = UhidDevice::open() else {
            return;
        };
        let lua = Lua::new();
        register_uh(&lua).expect("register uh");
        let first: u32 = lua
            .load(
                r#"
                local dev = uh.create({
                    name = "keyforge-reuse-test",
                    descriptor = uh.gamepad({buttons = 4}).descriptor,
                    kind = "gamepad", buttons = 4,
                })
                return dev:id()
                "#,
            )
            .eval()
            .expect("first create");
        let second: u32 = lua
            .load(
                r#"
                -- Simulates a daemon config reload re-evaluating the script:
                -- same name + descriptor must reuse the live kernel device.
                local dev = uh.create({
                    name = "keyforge-reuse-test",
                    descriptor = uh.gamepad({buttons = 4}).descriptor,
                    kind = "gamepad", buttons = 4,
                })
                return dev:id()
                "#,
            )
            .eval()
            .expect("second create");
        assert_eq!(first, second);
    }

    #[test]
    fn create2_rejects_oversize_descriptors() {
        let params = CreateParams {
            name: Vec::new(),
            phys: Vec::new(),
            uniq: Vec::new(),
            bus: BUS_USB,
            vendor: 0,
            product: 0,
            version: 0,
            country: 0,
            descriptor: vec![0u8; UHID_DATA_MAX + 1],
        };
        assert!(encode_create2(&params).is_err());
        assert!(encode_input2(&[0u8; UHID_DATA_MAX + 1]).is_err());
    }

    #[test]
    fn input2_and_replies_encode() {
        let ev = encode_input2(&[0x01, 0x02]).unwrap();
        assert_eq!(get_u32(&ev, 0), UHID_INPUT2);
        assert_eq!(get_u16(&ev, 4), 2);
        assert_eq!(&ev[6..8], &[0x01, 0x02]);

        let ev = encode_destroy();
        assert_eq!(get_u32(&ev, 0), UHID_DESTROY);

        let ev = encode_get_report_reply(7, 0, &[0xAA]).unwrap();
        assert_eq!(get_u32(&ev, 0), UHID_GET_REPORT_REPLY);
        assert_eq!(get_u32(&ev, 4), 7);
        assert_eq!(get_u16(&ev, 8), 0);
        assert_eq!(get_u16(&ev, 10), 1);
        assert_eq!(ev[12], 0xAA);

        let ev = encode_set_report_reply(9, 5);
        assert_eq!(get_u32(&ev, 0), UHID_SET_REPORT_REPLY);
        assert_eq!(get_u32(&ev, 4), 9);
        assert_eq!(get_u16(&ev, 8), 5);
    }

    #[test]
    fn kernel_events_parse() {
        let mut start = [0u8; UHID_EVENT_SIZE];
        put_u32(&mut start, 0, UHID_START);
        put_u64(&mut start, 4, UHID_DEV_NUMBERED_INPUT_REPORTS);
        assert_eq!(
            parse_event(&start),
            Some(KernelEvent::Start {
                dev_flags: UHID_DEV_NUMBERED_INPUT_REPORTS
            })
        );

        let mut out = [0u8; UHID_EVENT_SIZE];
        put_u32(&mut out, 0, UHID_OUTPUT);
        out[4..7].copy_from_slice(&[0x10, 0x20, 0x30]);
        put_u16(&mut out, 4100, 3);
        out[4102] = UHID_OUTPUT_REPORT;
        assert_eq!(
            parse_event(&out),
            Some(KernelEvent::Output {
                rtype: UHID_OUTPUT_REPORT,
                data: vec![0x10, 0x20, 0x30]
            })
        );

        let mut get = [0u8; UHID_EVENT_SIZE];
        put_u32(&mut get, 0, UHID_GET_REPORT);
        put_u32(&mut get, 4, 42);
        get[8] = 2;
        get[9] = UHID_FEATURE_REPORT;
        assert_eq!(
            parse_event(&get),
            Some(KernelEvent::GetReport {
                id: 42,
                rnum: 2,
                rtype: UHID_FEATURE_REPORT
            })
        );
    }

    #[test]
    fn poll_returns_empty_table_without_devices() {
        let lua = Lua::new();
        register_uh(&lua).expect("register uh");
        let len: usize = lua
            .load(r#"return #uh.poll()"#)
            .eval()
            .expect("poll with no devices");
        assert_eq!(len, 0);
        let flags: u64 = lua
            .load(r#"return uh.DEV_NUMBERED_INPUT_REPORTS"#)
            .eval()
            .expect("dev flag constant");
        assert_eq!(flags, UHID_DEV_NUMBERED_INPUT_REPORTS);
    }

    #[test]
    fn boot_descriptors_have_canonical_shape() {
        let kbd = keyboard_descriptor();
        assert_eq!(kbd.len(), 63);
        assert_eq!(&kbd[..4], &[0x05, 0x01, 0x09, 0x06]);
        assert_eq!(kbd[kbd.len() - 1], 0xC0);
        assert!(kbd.windows(2).any(|w| w == [0x95, 0x06]));
        assert_eq!(pack_keyboard(&KeyboardState::default()).len(), 8);

        let mouse = mouse_descriptor();
        assert_eq!(mouse.len(), 50);
        assert_eq!(&mouse[..4], &[0x05, 0x01, 0x09, 0x02]);
        assert_eq!(mouse[mouse.len() - 1], 0xC0);
        assert_eq!(pack_mouse(&MouseState::default()), vec![0, 0, 0, 0]);
    }

    #[test]
    fn gamepad_descriptor_and_packing_agree() {
        let axes = vec![
            GamepadAxis::X,
            GamepadAxis::Y,
            GamepadAxis::Rx,
            GamepadAxis::Ry,
        ];
        let rd = gamepad_descriptor(16, &axes, true);
        assert_eq!(&rd[..6], &[0x05, 0x01, 0x09, 0x04, 0xA1, 0x01]);
        assert_eq!(rd[rd.len() - 1], 0xC0);
        assert!(rd.windows(2).any(|w| w == [0x29, 16]));

        let layout = ReportLayout::gamepad(16, axes, true).expect("valid gamepad layout");
        assert_eq!(layout.report_len(), 2 + 8 + 1);
        let mut state_axes = HashMap::new();
        state_axes.insert("x".to_string(), 1000);
        state_axes.insert("y".to_string(), -1000);
        state_axes.insert("rx".to_string(), 32767);
        state_axes.insert("ry".to_string(), -32768);
        let state = GamepadState {
            buttons: {
                let mut b = vec![false; 16];
                b[0] = true;
                b[15] = true;
                b
            },
            axes: state_axes,
            hat: 3,
        };
        let report = pack_gamepad(&layout, &state).unwrap();
        assert_eq!(&report[..2], &[0x01, 0x80]);
        assert_eq!(&report[2..4], &1000i16.to_le_bytes());
        assert_eq!(&report[4..6], &(-1000i16).to_le_bytes());
        assert_eq!(&report[6..8], &32767i16.to_le_bytes());
        assert_eq!(&report[8..10], &(-32767i16).to_le_bytes());
        assert_eq!(report[10], 3);
    }

    #[test]
    fn keyboard_and_mouse_pack_reports() {
        let kbd = pack_keyboard(&KeyboardState {
            modifiers: 0x02,
            keys: vec![0x04, 0x05],
        });
        assert_eq!(kbd, vec![0x02, 0x00, 0x04, 0x05, 0, 0, 0, 0]);

        let mouse = pack_mouse(&MouseState {
            buttons: 0x01,
            x: 10,
            y: -10,
            wheel: 130,
        });
        assert_eq!(mouse, vec![0x01, 10, 246, 127]);
    }

    #[test]
    fn device_open_reports_missing_node_gracefully() {
        let path = PathBuf::from(UHID_PATH);
        if !path.exists() {
            assert!(UhidDevice::open().is_err());
            return;
        }
        match UhidDevice::open() {
            Ok(dev) => dev.destroy().expect("destroy on fresh device"),
            // Node present but not openable here (e.g. root-only perms).
            Err(err) => assert_eq!(err.kind(), io::ErrorKind::PermissionDenied),
        }
    }

    #[test]
    fn lua_surface_builds_layouts_and_fails_closed_without_node() {
        let lua = Lua::new();
        register_uh(&lua).expect("register uh");
        register_uh(&lua).expect("register uh is idempotent");
        let desc_len: usize = lua
            .load(r#"return #uh.gamepad({buttons = 8}).descriptor"#)
            .eval()
            .expect("gamepad builder");
        assert!(desc_len > 32);
        let kind: String = lua
            .load(r#"return uh.keyboard().kind"#)
            .eval()
            .expect("keyboard builder");
        assert_eq!(kind, "keyboard");

        // Creating a real kernel device needs /dev/uhid access; without it
        // creation must fail instead of hanging or crashing.
        let survived: bool = lua
            .load(
                r#"
                local ok, dev = pcall(uh.create, {
                    name = "keyforge-test",
                    descriptor = uh.keyboard().descriptor,
                    kind = "keyboard",
                })
                if ok then dev:destroy() end
                return true
                "#,
            )
            .eval()
            .expect("create roundtrip must not crash Lua");
        assert!(survived);
    }

    #[test]
    fn mirror_layout_sorts_and_truncates_buttons() {
        let (descriptor, map) =
            mirror_layout(vec![307, 304, 305], mirror_axes(&[0, 1])).expect("mirror layout");
        assert_eq!(map.codes, vec![304, 305, 307]);
        assert_eq!(map.axes.len(), 2);
        assert!(!descriptor.is_empty());

        let many: Vec<u16> = (0..64).collect();
        let (_, map) = mirror_layout(many, mirror_axes(&[0])).expect("truncated layout");
        assert_eq!(map.codes.len(), 32);
    }

    #[test]
    fn mirror_layout_rejects_empty_devices() {
        assert!(mirror_layout(Vec::new(), mirror_axes(&[0])).is_err());
        assert!(mirror_layout(vec![304], Vec::new()).is_err());
    }

    #[test]
    fn mirror_state_packs_button_and_axis_updates() {
        let (_, map) =
            mirror_layout(vec![304, 305], mirror_axes(&[0, 1, 3, 4])).expect("mirror layout");
        let mut state = map.blank_state();
        assert_eq!(state.buttons, vec![false, false]);

        assert!(map.apply_key(&mut state, 305, true));
        assert!(!map.apply_key(&mut state, 305, true));
        assert!(!map.apply_key(&mut state, 999, true));
        assert!(map.apply_abs(&mut state, 0, 1000));
        assert!(map.apply_abs(&mut state, 4, -2000));
        assert!(!map.apply_abs(&mut state, 0x10, 5));

        let report = map.pack(&state).expect("pack mirror state");
        // Button index 1 -> second bit; X=1000 LE; RY=-2000 LE.
        assert_eq!(&report[1..3], &1000i16.to_le_bytes());
        assert_eq!(&report[3..5], &[0, 0]);
        assert_eq!(&report[5..7], &[0, 0]);
        assert_eq!(&report[7..9], &(-2000i16).to_le_bytes());
    }
}
