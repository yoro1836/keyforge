// FFI types, ioctl constants, and low-level device handling.
use serde::{Deserialize, Serialize};
use std::ffi::{CString, c_char};
use std::fs;
use std::io::{self, ErrorKind};
use std::mem;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Raw FFI
// ---------------------------------------------------------------------------
unsafe extern "C" {
    pub fn _exit(code: i32) -> !;
    pub fn fork() -> i32;
    pub fn open(path: *const c_char, flags: i32, mode: u32) -> i32;
    pub fn close(fd: i32) -> i32;
    pub fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn ioctl(fd: i32, request: u32, ...) -> i32;
    pub fn epoll_create1(flags: i32) -> i32;
    pub fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut EpollEvent) -> i32;
    pub fn epoll_wait(epfd: i32, events: *mut EpollEvent, maxevents: i32, timeout: i32) -> i32;
    pub fn inotify_init1(flags: i32) -> i32;
    pub fn inotify_add_watch(fd: i32, pathname: *const c_char, mask: u32) -> i32;
    pub fn chown(path: *const c_char, owner: u32, group: u32) -> i32;
    pub fn mknod(path: *const c_char, mode: u32, dev: u64) -> i32;
    pub fn setsid() -> i32;
}

// ---------------------------------------------------------------------------
// ioctl helpers
// ---------------------------------------------------------------------------
const fn ioc(dir: u32, ty: u8, nr: u8, size: u8) -> u32 {
    (dir << 30) | ((ty as u32) << 8) | (nr as u32) | ((size as u32) << 16)
}
const fn ior(ty: u8, nr: u8, size: u8) -> u32 {
    ioc(2, ty, nr, size)
}
const fn iow(ty: u8, nr: u8, size: u8) -> u32 {
    ioc(1, ty, nr, size)
}

// ---------------------------------------------------------------------------
// Kernel ABI structs
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TimeVal {
    pub tv_sec: isize,
    pub tv_usec: isize,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct InputEvent {
    pub time: TimeVal,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

pub const EPOLL_CTL_ADD: i32 = 1;
pub const EPOLL_CTL_DEL: i32 = 2;
pub const EPOLLIN: u32 = 0x001;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLERR: u32 = 0x008;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------
pub const INPUT_DIR: &str = "/dev/input";

pub const EV_SYN: i32 = 0x00;
pub const EV_KEY: i32 = 0x01;
pub const EV_ABS: i32 = 0x03;
pub const SYN_REPORT: u32 = 0;
pub const ABS_X: u32 = 0x00;
pub const ABS_Y: u32 = 0x01;
pub const ABS_Z: u32 = 0x02;
pub const ABS_RZ: u32 = 0x05;
pub const ABS_RX: u32 = 0x03;
pub const ABS_RY: u32 = 0x04;
pub const KEY_CNT: usize = 768;
pub const ABS_CNT: usize = 0x40;

pub const EVIOCGID: u32 = ior(b'E', 0x02, mem::size_of::<InputId>() as u8);
pub const EVIOCGRAB: u32 = iow(b'E', 0x90, mem::size_of::<i32>() as u8);
#[allow(dead_code)]
pub const fn eviocgbit(ev: u8, len: u8) -> u32 {
    ioc(2, b'E', 0x20 + ev, len)
}

pub const fn eviocgname(len: u8) -> u32 {
    ioc(2, b'E', 0x06, len)
}

/// Visible input node for source selection (no grab, read-only probe).
#[derive(Debug, Clone)]
pub struct InputInfo {
    pub name: String,
    pub vid: u16,
    pub pid: u16,
    pub handler: String,
}

/// List /dev/input/event* nodes with vendor/product/name, sorted by handler.
pub fn list_inputs() -> Vec<InputInfo> {
    let mut out = Vec::new();
    let dir = match fs::read_dir(INPUT_DIR) {
        Ok(dir) => dir,
        Err(_) => return out,
    };
    let mut entries: Vec<_> = dir.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let handler = entry.file_name().to_string_lossy().into_owned();
        if !handler.starts_with("event") {
            continue;
        }
        let path = entry.path();
        let cpath = match CString::new(path.to_string_lossy().as_bytes()) {
            Ok(path) => path,
            Err(_) => continue,
        };
        unsafe {
            let fd = open(cpath.as_ptr(), O_RDONLY | O_NONBLOCK, 0);
            if fd < 0 {
                continue;
            }
            let mut id = InputId::default();
            let mut name = [0u8; 256];
            let ok = do_ioctl_ptr(fd, EVIOCGID, &mut id) == 0
                && do_ioctl_ptr(fd, eviocgname(u8::MAX), name.as_mut_ptr()) >= 0;
            close(fd);
            if !ok {
                continue;
            }
            let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            out.push(InputInfo {
                name: String::from_utf8_lossy(&name[..len]).into_owned(),
                vid: id.vendor,
                pid: id.product,
                handler,
            });
        }
    }
    out
}

pub const IN_NONBLOCK: i32 = 0o4000;
pub const IN_CREATE: u32 = 0x0000_0100;
pub const IN_DELETE: u32 = 0x0000_0200;

pub const O_RDONLY: i32 = 0o0;
pub const O_NONBLOCK: i32 = 0o4000;
pub const EAGAIN: i32 = 11;

pub const RAW_FILE_L: &str = "keyforge_raw_L";
pub const RAW_FILE_R: &str = "keyforge_raw_R";
pub const PLUGIN_MANIFEST: &str = "/sdcard/.keyforge/manifest.json";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
pub fn get_errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

pub unsafe fn do_ioctl(fd: i32, req: u32, arg: usize) -> i32 {
    unsafe { ioctl(fd, req, arg) }
}

pub unsafe fn do_ioctl_ptr<T>(fd: i32, req: u32, arg: *mut T) -> i32 {
    unsafe { ioctl(fd, req, arg as usize) }
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------
#[derive(Clone, Debug, Deserialize, Serialize)]
struct HiddenDeviceState {
    path: PathBuf,
    hidden_path: PathBuf,
    dev: u64,
    ino: u64,
    rdev: u64,
    input_id: InputId,
    mode: u32,
    uid: u32,
    gid: u32,
}

#[derive(Debug, Deserialize)]
struct PermissionHiddenDeviceState {
    path: PathBuf,
    mode: u32,
    dev: u64,
    ino: u64,
    rdev: u64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PersistedHiddenDeviceState {
    Isolated(HiddenDeviceState),
    Permission(PermissionHiddenDeviceState),
}

fn metadata_if_exists(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn input_id_for_fd(fd: i32) -> io::Result<InputId> {
    let mut input_id = InputId::default();
    if unsafe { do_ioctl_ptr(fd, EVIOCGID, &mut input_id) } == 0 {
        Ok(input_id)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn input_id_for_path(path: &Path) -> io::Result<Option<InputId>> {
    let cpath = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error))?;
    let fd = unsafe { open(cpath.as_ptr(), O_RDONLY | O_NONBLOCK, 0) };
    if fd < 0 {
        return Ok(None);
    }
    let input_id = input_id_for_fd(fd).ok();
    unsafe {
        close(fd);
    }
    Ok(input_id)
}

const S_IFMT: u32 = 0o170000;

impl HiddenDeviceState {
    fn capture(path: &Path, hidden_path: PathBuf, input_id: InputId) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            hidden_path,
            dev: metadata.dev(),
            ino: metadata.ino(),
            rdev: metadata.rdev(),
            input_id,
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
        })
    }

    fn same_device(&self, metadata: &fs::Metadata) -> bool {
        self.dev == metadata.dev() && self.rdev == metadata.rdev()
    }

    fn save(&self, state_path: &Path) -> io::Result<()> {
        let temporary = state_path.with_extension("tmp");
        let encoded = serde_json::to_vec(self)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
        fs::write(&temporary, encoded)?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        fs::rename(temporary, state_path)
    }

    fn mknod_node(&self, at: &Path, permissions: u32) -> io::Result<()> {
        let cpath = CString::new(at.to_string_lossy().as_bytes())
            .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error))?;
        if unsafe {
            mknod(
                cpath.as_ptr(),
                (self.mode & S_IFMT) | permissions,
                self.rdev,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn chown_node(&self, at: &Path) -> io::Result<()> {
        let cpath = CString::new(at.to_string_lossy().as_bytes())
            .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error))?;
        if unsafe { chown(cpath.as_ptr(), self.uid, self.gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Link the live node to the private name (hard link, or a fresh `mknod`
    /// node with the same major/minor when the filesystem refuses links).
    fn create_hidden_node(&self) -> io::Result<()> {
        if fs::hard_link(&self.path, &self.hidden_path).is_ok() {
            let _ = fs::set_permissions(
                &self.hidden_path,
                fs::Permissions::from_mode((self.mode & S_IFMT) | 0o600),
            );
            return Ok(());
        }
        self.mknod_node(&self.hidden_path, 0o600)
    }

    /// Recreate the original node from the private name, restoring its
    /// original permissions and owner (critical when recreated via `mknod`).
    fn restore_original_node(&self) -> io::Result<()> {
        if fs::hard_link(&self.hidden_path, &self.path).is_ok() {
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode & 0o7777));
            self.chown_node(&self.path)?;
            return Ok(());
        }
        self.mknod_node(&self.path, self.mode & 0o7777)?;
        self.chown_node(&self.path)
    }

    fn remove_hidden_link(&self) -> io::Result<()> {
        if let Some(metadata) = metadata_if_exists(&self.hidden_path)?
            && self.same_device(&metadata)
        {
            fs::remove_file(&self.hidden_path)?;
        }
        Ok(())
    }

    fn discard(&self) -> io::Result<()> {
        self.remove_hidden_link()
    }

    fn restore(&self, validate_input: bool) -> io::Result<bool> {
        if metadata_if_exists(&self.path)?.is_some() {
            self.remove_hidden_link()?;
            return Ok(false);
        }

        let Some(hidden_metadata) = metadata_if_exists(&self.hidden_path)? else {
            return Ok(false);
        };
        if !self.same_device(&hidden_metadata) {
            return Ok(false);
        }

        if validate_input && input_id_for_path(&self.hidden_path)? != Some(self.input_id) {
            self.remove_hidden_link()?;
            return Ok(false);
        }

        self.restore_original_node()?;
        if validate_input && input_id_for_path(&self.path)? != Some(self.input_id) {
            fs::remove_file(&self.path)?;
            self.remove_hidden_link()?;
            return Ok(false);
        }
        fs::remove_file(&self.hidden_path)?;
        Ok(true)
    }
}

impl PermissionHiddenDeviceState {
    fn restore(&self) -> io::Result<bool> {
        let Some(metadata) = metadata_if_exists(&self.path)? else {
            return Ok(false);
        };
        if self.dev != metadata.dev() || self.ino != metadata.ino() || self.rdev != metadata.rdev()
        {
            return Ok(false);
        }
        fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode))?;
        Ok(true)
    }
}

impl PersistedHiddenDeviceState {
    fn load(state_path: &Path) -> io::Result<Self> {
        let encoded = fs::read(state_path)?;
        serde_json::from_slice(&encoded)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))
    }

    fn restore(&self, validate_input: bool) -> io::Result<bool> {
        match self {
            Self::Isolated(state) => state.restore(validate_input),
            Self::Permission(state) => state.restore(),
        }
    }
}

pub struct Device {
    pub fd: i32,
    pub path: Option<PathBuf>,
    pub lx: i32,
    pub ly: i32,
    pub rx: i32,
    pub ry: i32,
    pub ld: bool,
    pub rd: bool,
    hide_state_path: Option<PathBuf>,
    hidden: Option<HiddenDeviceState>,
}

impl Device {
    pub fn new(hide_state_path: Option<PathBuf>) -> Self {
        Device {
            fd: -1,
            path: None,
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
            ld: false,
            rd: false,
            hide_state_path,
            hidden: None,
        }
    }

    fn restore_hidden_state_with_validation(
        state_path: &Path,
        validate_input: bool,
    ) -> io::Result<bool> {
        if !state_path.exists() {
            return Ok(false);
        }
        let state = PersistedHiddenDeviceState::load(state_path)?;
        let restored = state.restore(validate_input)?;
        match fs::remove_file(state_path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(restored)
    }

    pub fn restore_hidden_state(state_path: &Path) -> io::Result<bool> {
        Self::restore_hidden_state_with_validation(state_path, true)
    }

    pub fn set_hidden(&mut self, hide: bool) -> io::Result<()> {
        if !hide {
            let restored = if let Some(state) = self.hidden.as_ref() {
                if Self::is_alive(self.fd) {
                    state.restore(true).map(|_| ())
                } else {
                    state.discard()
                }
            } else if let Some(state_path) = self.hide_state_path.as_ref()
                && state_path.exists()
            {
                Self::restore_hidden_state(state_path).map(|_| ())
            } else {
                Ok(())
            };
            if restored.is_ok()
                && self.hidden.is_some()
                && let Some(state_path) = self.hide_state_path.as_ref()
            {
                match fs::remove_file(state_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            self.hidden = None;
            return restored;
        }

        if self.hidden.is_some() {
            return Ok(());
        }
        let path = self.path.clone().ok_or_else(|| {
            io::Error::new(ErrorKind::NotFound, "no physical input device is connected")
        })?;
        let state_path = self.hide_state_path.clone().ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "hidden-device state path was not provided",
            )
        })?;

        if state_path.exists() {
            Self::restore_hidden_state(&state_path)?;
        }

        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidInput, "input device path has no filename")
        })?;
        let hidden_path =
            Path::new("/dev").join(format!(".keyforge-input-{}", file_name.to_string_lossy()));
        if let Some(hidden_metadata) = metadata_if_exists(&hidden_path)? {
            let path_metadata = fs::metadata(&path)?;
            if hidden_metadata.dev() == path_metadata.dev()
                && hidden_metadata.ino() == path_metadata.ino()
                && hidden_metadata.rdev() == path_metadata.rdev()
            {
                fs::remove_file(&hidden_path)?;
            } else {
                return Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    format!(
                        "hidden device path already exists: {}",
                        hidden_path.display()
                    ),
                ));
            }
        }

        let input_id = input_id_for_fd(self.fd)?;
        let state = HiddenDeviceState::capture(&path, hidden_path, input_id)?;
        state.save(&state_path)?;
        if let Err(error) = state.create_hidden_node() {
            let _ = fs::remove_file(&state_path);
            return Err(error);
        }
        if let Err(error) = fs::remove_file(&state.path) {
            let _ = fs::remove_file(&state.hidden_path);
            let _ = fs::remove_file(&state_path);
            return Err(error);
        }
        self.hidden = Some(state);
        Ok(())
    }

    pub fn deinit(&mut self) {
        let _ = self.set_hidden(false);
        unsafe {
            if self.fd >= 0 {
                do_ioctl(self.fd, EVIOCGRAB, 0);
                close(self.fd);
                self.fd = -1;
            }
        }
        self.path = None;
    }

    /// Read the grabbed device's capabilities: sorted KEY codes and ABS codes.
    /// Used to build the UHID mirror descriptor (replaces the old uinput copy).
    pub fn read_caps(fd: i32) -> (Vec<u16>, Vec<u32>) {
        let mut keys = Vec::new();
        let key_bytes = KEY_CNT.div_ceil(8);
        let mut kbuf: Vec<u8> = vec![0u8; key_bytes];
        unsafe {
            if do_ioctl_ptr(
                fd,
                eviocgbit(EV_KEY as u8, key_bytes as u8),
                kbuf.as_mut_ptr(),
            ) >= 0
            {
                for i in 0..KEY_CNT {
                    if (kbuf[i / 8] >> (i % 8)) & 1 != 0 {
                        keys.push(i as u16);
                    }
                }
            }
        }
        let mut axes = Vec::new();
        let abs_bytes = ABS_CNT.div_ceil(8);
        let mut abuf: Vec<u8> = vec![0u8; abs_bytes];
        unsafe {
            if do_ioctl_ptr(
                fd,
                eviocgbit(EV_ABS as u8, abs_bytes as u8),
                abuf.as_mut_ptr(),
            ) >= 0
            {
                for i in 0..ABS_CNT as u32 {
                    if (abuf[i as usize / 8] >> (i as usize % 8)) & 1 != 0 {
                        axes.push(i);
                    }
                }
            }
        }
        (keys, axes)
    }

    pub fn find_device(vid: u16, pid: u16) -> Option<(i32, PathBuf)> {
        let dir = fs::read_dir(INPUT_DIR).ok()?;
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("event") {
                continue;
            }
            let path = entry.path();
            let cpath = match CString::new(path.to_string_lossy().as_bytes()) {
                Ok(path) => path,
                Err(_) => continue,
            };
            unsafe {
                let fd = open(cpath.as_ptr(), O_RDONLY | O_NONBLOCK, 0);
                if fd < 0 {
                    continue;
                }
                let mut id = InputId::default();
                if do_ioctl_ptr(fd, EVIOCGID, &mut id) == 0
                    && id.vendor == vid
                    && id.product == pid
                    && do_ioctl(fd, EVIOCGRAB, 1) == 0
                {
                    return Some((fd, path));
                }
                close(fd);
            }
        }
        None
    }

    /// Check if the physical device is still connected by doing a no-op ioctl.
    /// Returns false if the fd is invalid or the device is gone.
    pub fn is_alive(fd: i32) -> bool {
        if fd < 0 {
            return false;
        }
        let mut id = InputId::default();
        unsafe { do_ioctl_ptr(fd, EVIOCGID, &mut id) == 0 }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.deinit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("keyforge-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn isolate_test_node(device: &Path, hidden: &Path, state_path: &Path) -> HiddenDeviceState {
        let state =
            HiddenDeviceState::capture(device, hidden.to_path_buf(), InputId::default()).unwrap();
        state.save(state_path).unwrap();
        fs::hard_link(device, hidden).unwrap();
        fs::remove_file(device).unwrap();
        state
    }

    #[test]
    fn isolated_node_restores_the_original_link() {
        let dir = test_dir("restore-link");
        let device = dir.join("event7");
        let hidden = dir.join(".keyforge-event7");
        let state_path = dir.join("hidden.json");
        fs::write(&device, b"device").unwrap();
        fs::set_permissions(&device, fs::Permissions::from_mode(0o640)).unwrap();
        let original_inode = fs::metadata(&device).unwrap().ino();

        isolate_test_node(&device, &hidden, &state_path);
        assert!(!device.exists());
        assert!(hidden.exists());

        assert!(Device::restore_hidden_state_with_validation(&state_path, false).unwrap());
        let restored = fs::metadata(&device).unwrap();
        assert_eq!(restored.ino(), original_inode);
        assert_eq!(restored.mode() & 0o7777, 0o640);
        assert!(!hidden.exists());
        assert!(!state_path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn isolated_node_never_replaces_a_new_device_path() {
        let dir = test_dir("replaced-node");
        let device = dir.join("event7");
        let hidden = dir.join(".keyforge-event7");
        let state_path = dir.join("hidden.json");
        fs::write(&device, b"original").unwrap();
        fs::set_permissions(&device, fs::Permissions::from_mode(0o640)).unwrap();
        isolate_test_node(&device, &hidden, &state_path);

        fs::write(&device, b"replacement").unwrap();
        fs::set_permissions(&device, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(!Device::restore_hidden_state_with_validation(&state_path, false).unwrap());
        assert_eq!(fs::read(&device).unwrap(), b"replacement");
        assert_eq!(fs::metadata(&device).unwrap().mode() & 0o7777, 0o600);
        assert!(!hidden.exists());
        assert!(!state_path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mknod_fallback_recreates_a_node() {
        let dir = test_dir("mknod-fallback");
        let device = dir.join("event7");
        fs::write(&device, b"device").unwrap();
        let target = dir.join("recreated");
        let state =
            HiddenDeviceState::capture(&device, dir.join(".keyforge-event7"), InputId::default())
                .unwrap();

        state.mknod_node(&target, state.mode & 0o7777).unwrap();
        let metadata = fs::metadata(&target).unwrap();
        assert_eq!(metadata.mode() & 0o7777, state.mode & 0o7777);
        assert_eq!(metadata.mode() & S_IFMT, state.mode & S_IFMT);
        assert_eq!(metadata.rdev(), state.rdev);

        fs::remove_file(&target).unwrap();
        fs::remove_file(&device).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn disconnected_node_discards_the_private_link() {
        let dir = test_dir("discard-link");
        let device = dir.join("event7");
        let hidden = dir.join(".keyforge-event7");
        let state_path = dir.join("hidden.json");
        fs::write(&device, b"device").unwrap();
        let state = isolate_test_node(&device, &hidden, &state_path);

        state.discard().unwrap();
        fs::remove_file(&state_path).unwrap();
        assert!(!device.exists());
        assert!(!hidden.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migrates_permission_only_hidden_state() {
        let dir = test_dir("legacy-permissions");
        let device = dir.join("event7");
        let state_path = dir.join("hidden.json");
        fs::write(&device, b"device").unwrap();
        fs::set_permissions(&device, fs::Permissions::from_mode(0o640)).unwrap();
        let metadata = fs::metadata(&device).unwrap();
        let legacy_state = serde_json::json!({
            "path": device,
            "mode": 0o640,
            "dev": metadata.dev(),
            "ino": metadata.ino(),
            "rdev": metadata.rdev(),
        });
        fs::write(&state_path, serde_json::to_vec(&legacy_state).unwrap()).unwrap();
        fs::set_permissions(&device, fs::Permissions::from_mode(0o000)).unwrap();

        assert!(Device::restore_hidden_state_with_validation(&state_path, false).unwrap());
        assert_eq!(fs::metadata(&device).unwrap().mode() & 0o7777, 0o640);
        assert!(!state_path.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
