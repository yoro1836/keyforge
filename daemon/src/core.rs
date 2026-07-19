// FFI types, ioctl constants, and low-level device handling.
use std::ffi::CString;
use std::fs;
use std::mem;

// ---------------------------------------------------------------------------
// Raw FFI
// ---------------------------------------------------------------------------
unsafe extern "C" {
    pub fn open(path: *const u8, flags: i32, mode: u32) -> i32;
    pub fn close(fd: i32) -> i32;
    pub fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn ioctl(fd: i32, request: u32, ...) -> i32;
    pub fn epoll_create1(flags: i32) -> i32;
    pub fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut EpollEvent) -> i32;
    pub fn epoll_wait(epfd: i32, events: *mut EpollEvent, maxevents: i32, timeout: i32) -> i32;
    pub fn inotify_init1(flags: i32) -> i32;
    pub fn inotify_add_watch(fd: i32, pathname: *const u8, mask: u32) -> i32;
    pub fn __errno() -> *mut i32;
}

// ---------------------------------------------------------------------------
// ioctl helpers
// ---------------------------------------------------------------------------
const fn ioc(dir: u32, ty: u8, nr: u8, size: u8) -> u32 {
    (dir << 30) | ((ty as u32) << 8) | (nr as u32) | ((size as u32) << 16)
}
const fn io_u(ty: u8, nr: u8) -> u32          { ioc(0, ty, nr, 0) }
const fn ior(ty: u8, nr: u8, size: u8) -> u32 { ioc(2, ty, nr, size) }
const fn iow(ty: u8, nr: u8, size: u8) -> u32 { ioc(1, ty, nr, size) }

// ---------------------------------------------------------------------------
// Kernel ABI structs
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TimeVal { pub tv_sec: isize, pub tv_usec: isize }

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct InputEvent {
    pub time: TimeVal,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct InputId {
    pub bustype: u16, pub vendor: u16, pub product: u16, pub version: u16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct InputAbsInfo {
    pub value: i32, pub minimum: i32, pub maximum: i32,
    pub fuzz: i32, pub flat: i32, pub resolution: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct UinputAbsSetup {
    pub code: u16, __pad: [u8; 2], pub absinfo: InputAbsInfo,
}

#[repr(C)]
pub struct UinputSetup {
    pub id: InputId,
    pub name: [u8; UINPUT_MAX_NAME_SIZE],
    pub ff_effects_max: u32,
}

impl Default for UinputSetup {
    fn default() -> Self {
        UinputSetup { id: InputId::default(), name: [0u8; UINPUT_MAX_NAME_SIZE], ff_effects_max: 0 }
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct EpollEvent { pub events: u32, pub data: u64 }

pub const EPOLL_CTL_ADD: i32 = 1;
pub const EPOLL_CTL_DEL: i32 = 2;
pub const EPOLLIN: u32 = 0x001;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLERR: u32 = 0x008;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------
pub const INPUT_DIR: &str = "/dev/input";
pub const UINPUT_MAX_NAME_SIZE: usize = 80;

pub const EV_SYN: i32 = 0x00; pub const EV_KEY: i32 = 0x01; pub const EV_ABS: i32 = 0x03;
pub const SYN_REPORT: u32 = 0;
pub const ABS_X: u32 = 0x00; pub const ABS_Y: u32 = 0x01;
pub const ABS_Z: u32 = 0x02; pub const ABS_RZ: u32 = 0x05;
pub const ABS_RX: u32 = 0x03; pub const ABS_RY: u32 = 0x04;
pub const KEY_CNT: usize = 768; pub const ABS_CNT: usize = 0x40;
pub const BUS_USB: u16 = 0x03;

pub const EVIOCGID: u32  = ior(b'E', 0x02, mem::size_of::<InputId>() as u8);
pub const EVIOCGRAB: u32 = iow(b'E', 0x90, mem::size_of::<i32>() as u8);
#[allow(dead_code)]
pub const fn eviocgbit(ev: u8, len: u8) -> u32 { ioc(2, b'E', 0x20 + ev, len) }
#[allow(dead_code)]
pub const fn eviocgabs(abs: u8) -> u32          { ior(b'E', 0x40 + abs, mem::size_of::<InputAbsInfo>() as u8) }

pub const UI_DEV_CREATE: u32  = io_u(b'U', 1);
pub const UI_DEV_DESTROY: u32 = io_u(b'U', 2);
pub const UI_DEV_SETUP: u32   = iow(b'U', 3, mem::size_of::<UinputSetup>() as u8);
pub const UI_ABS_SETUP: u32   = iow(b'U', 4, mem::size_of::<UinputAbsSetup>() as u8);
pub const UI_SET_EVBIT: u32   = iow(b'U', 100, mem::size_of::<i32>() as u8);
pub const UI_SET_KEYBIT: u32  = iow(b'U', 101, mem::size_of::<i32>() as u8);
pub const UI_SET_ABSBIT: u32  = iow(b'U', 103, mem::size_of::<i32>() as u8);

pub const IN_NONBLOCK: i32 = 0o4000;
pub const IN_CREATE: u32      = 0x0000_0100;
pub const IN_DELETE: u32      = 0x0000_0200;

pub const O_RDONLY: i32 = 0o0; pub const O_WRONLY: i32 = 0o1; pub const O_NONBLOCK: i32 = 0o4000;
pub const EAGAIN: i32 = 11;

pub const RAW_FILE_L: &str = "/tmp/keyforge_raw_L";
pub const RAW_FILE_R: &str = "/tmp/keyforge_raw_R";
pub const PLUGIN_MANIFEST: &str = "/sdcard/.keyforge/manifest.json";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
pub fn get_errno() -> i32 { unsafe { *__errno() } }

pub unsafe fn do_ioctl(fd: i32, req: u32, arg: usize) -> i32 {
    unsafe { ioctl(fd, req, arg) }
}

pub unsafe fn do_ioctl_ptr<T>(fd: i32, req: u32, arg: *mut T) -> i32 {
    unsafe { ioctl(fd, req, arg as usize) }
}

pub unsafe fn write_ev(ufd: i32, ev: &InputEvent) {
    unsafe { write(ufd, ev as *const _ as *const u8, mem::size_of::<InputEvent>()); }
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------
pub struct Device {
    pub fd: i32,
    pub ufd: i32,
    pub lx: i32, pub ly: i32,
    pub rx: i32, pub ry: i32,
    pub ld: bool, pub rd: bool,
}

impl Device {
    pub fn new() -> Self { Device { fd: -1, ufd: -1, lx: 0, ly: 0, rx: 0, ry: 0, ld: false, rd: false } }

    pub fn deinit(&mut self) {
        unsafe {
            if self.ufd >= 0 { do_ioctl(self.ufd, UI_DEV_DESTROY, 0); close(self.ufd); self.ufd = -1; }
            if self.fd >= 0  { do_ioctl(self.fd, EVIOCGRAB, 0);       close(self.fd);  self.fd = -1; }
        }
    }

    pub fn init_u(&mut self, fd: i32, vid: u16) -> bool {
        unsafe {
            let cpath = CString::new("/dev/uinput").unwrap();
            let u = open(cpath.as_ptr(), O_WRONLY | O_NONBLOCK, 0);
            if u < 0 { return false; }
            do_ioctl(u, UI_SET_EVBIT, EV_KEY as usize);
            do_ioctl(u, UI_SET_EVBIT, EV_ABS as usize);
            do_ioctl(u, UI_SET_EVBIT, EV_SYN as usize);

            // Copy KEY bits from physical device (same as memo.c)
            let key_bytes = KEY_CNT.div_ceil(8);
            let mut kbuf: Vec<u8> = vec![0u8; key_bytes];
            if do_ioctl_ptr(fd, eviocgbit(EV_KEY as u8, key_bytes as u8), kbuf.as_mut_ptr()) >= 0 {
                for i in 0..KEY_CNT {
                    if (kbuf[i / 8] >> (i % 8)) & 1 != 0 {
                        do_ioctl(u, UI_SET_KEYBIT, i);
                    }
                }
            }

            // Copy ABS bits + absinfo from physical device (same as memo.c)
            let abs_bytes = ABS_CNT.div_ceil(8);
            let mut abuf: Vec<u8> = vec![0u8; abs_bytes];
            if do_ioctl_ptr(fd, eviocgbit(EV_ABS as u8, abs_bytes as u8), abuf.as_mut_ptr()) >= 0 {
                for i in 0..ABS_CNT as u32 {
                    if (abuf[i as usize / 8] >> (i as usize % 8)) & 1 != 0 {
                        let mut info = InputAbsInfo::default();
                        if do_ioctl_ptr(fd, eviocgabs(i as u8), &mut info) >= 0 {
                            do_ioctl(u, UI_SET_ABSBIT, i as usize);
                            // Override stick axes range (same as memo.c)
                            if i == ABS_X || i == ABS_Y || i == ABS_RX || i == ABS_RY {
                                info.minimum = -32767;
                                info.maximum = 32767;
                                info.flat = 0;
                                info.fuzz = 0;
                            }
                            let mut s = UinputAbsSetup { code: i as u16, __pad: [0; 2], absinfo: info };
                            do_ioctl_ptr(u, UI_ABS_SETUP, &mut s);
                        }
                    }
                }
            }

            // identity
            let mut us = UinputSetup::default();
            us.id.bustype = BUS_USB; us.id.vendor = vid; us.id.product = 0x02d1; us.id.version = 1;
            let label = b"KeyForge Virtual Controller";
            us.name[..label.len()].copy_from_slice(label);

            if do_ioctl_ptr(u, UI_DEV_SETUP, &mut us) < 0 || do_ioctl(u, UI_DEV_CREATE, 0) < 0 {
                close(u); return false;
            }
            self.ufd = u;
            true
        }
    }

    pub fn find_device(vid: u16, pid: u16) -> i32 {
        let dir = match fs::read_dir(INPUT_DIR) { Ok(d) => d, Err(_) => return -1 };
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("event") { continue; }
            let path = format!("{}/{}", INPUT_DIR, name_str);
            let cpath = match CString::new(path) { Ok(p) => p, Err(_) => continue };
            unsafe {
                let fd = open(cpath.as_ptr(), O_RDONLY | O_NONBLOCK, 0);
                if fd < 0 { continue; }
                let mut id = InputId::default();
                if do_ioctl_ptr(fd, EVIOCGID, &mut id) == 0
                    && id.vendor == vid && id.product == pid
                    && do_ioctl(fd, EVIOCGRAB, 1) == 0 { return fd; }
                close(fd);
            }
        }
        -1
    }

    /// Check if the physical device is still connected by doing a no-op ioctl.
    /// Returns false if the fd is invalid or the device is gone.
    pub fn is_alive(fd: i32) -> bool {
        if fd < 0 { return false; }
        let mut id = InputId::default();
        unsafe { do_ioctl_ptr(fd, EVIOCGID, &mut id) == 0 }
    }
}

impl Drop for Device {
    fn drop(&mut self) { self.deinit(); }
}
