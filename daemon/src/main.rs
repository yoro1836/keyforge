mod config;
mod core;
mod pipeline;
mod plugin;
mod uhid;

use crate::uhid::Mirror;
use config::Config;
use core::*;
use mlua::Lua;
use pipeline::{EmitEvent, Event, Pipeline, Side};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Shared processing context: pipeline, config values, pending releases.
/// Output goes to the UHID [`Mirror`], passed separately.
struct ProcCtx<'a> {
    pipeline: &'a Pipeline,
    values: &'a HashMap<String, String>,
    pending: &'a mut Vec<(Instant, EmitEvent)>,
}
fn runtime_dir() -> PathBuf {
    for key in ["KEYFORGE_RUNTIME_DIR", "TMPDIR"] {
        if let Some(dir) = env::var_os(key)
            && !dir.is_empty()
        {
            return PathBuf::from(dir);
        }
    }
    if cfg!(target_os = "android") {
        PathBuf::from("/data/local/tmp")
    } else {
        env::temp_dir()
    }
}

fn main() {
    let mut config_path = PathBuf::from("/sdcard/.keyforge/keyforge.conf");
    let mut allow_device_hide = false;
    let mut hidden_state_path: Option<PathBuf> = None;
    let mut restore_hidden_state: Option<PathBuf> = None;
    let mut pidfile_path: Option<PathBuf> = None;
    let mut foreground = false;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                if let Some(path) = args.next() {
                    config_path = PathBuf::from(path);
                }
            }
            "--allow-device-hide" => allow_device_hide = true,
            "--foreground" => foreground = true,
            "--hidden-state" => {
                if let Some(path) = args.next() {
                    hidden_state_path = Some(PathBuf::from(path));
                }
            }
            "--pidfile" => {
                if let Some(path) = args.next() {
                    pidfile_path = Some(PathBuf::from(path));
                }
            }
            "--restore-hidden-state" => {
                if let Some(path) = args.next() {
                    restore_hidden_state = Some(PathBuf::from(path));
                }
            }
            _ => {}
        }
    }

    if let Some(state_path) = restore_hidden_state {
        match Device::restore_hidden_state(&state_path) {
            Ok(true) => println!("keyforge: physical device node restored"),
            Ok(false) => println!("keyforge: no live hidden device to restore"),
            Err(error) => {
                eprintln!("keyforge: failed to restore physical device node: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if let Some(state_path) = hidden_state_path.as_deref() {
        match Device::restore_hidden_state(state_path) {
            Ok(true) => eprintln!("keyforge: restored stale physical device node"),
            Ok(false) => {}
            Err(error) => {
                eprintln!("keyforge: failed to restore stale physical device node: {error}");
                std::process::exit(1);
            }
        }
    }

    let mut cfg = Config::load(&config_path);
    if cfg.hide_device && !allow_device_hide {
        eprintln!("keyforge: device hiding ignored outside KernelSU or Magisk");
    }

    // Detach from the caller (double fork, like encored) so the daemon always
    // ends up owned by init — never by a WebUI app or any short-lived shell.
    // The final child writes its own pidfile; the invoking script never has to
    // trust `$!`.
    if !foreground {
        unsafe {
            let pid = fork();
            if pid < 0 {
                eprintln!("keyforge: fork failed");
                _exit(1);
            }
            if pid > 0 {
                _exit(0);
            }
            if setsid() < 0 {
                eprintln!("keyforge: setsid failed");
                _exit(1);
            }
            let pid = fork();
            if pid < 0 {
                eprintln!("keyforge: fork failed");
                _exit(1);
            }
            if pid > 0 {
                _exit(0);
            }
        }
        if let Some(pidfile) = pidfile_path.as_deref()
            && let Err(error) = fs::write(pidfile, format!("{}\n", std::process::id()))
        {
            eprintln!("keyforge: failed to write pidfile {pidfile:?}: {error}");
            std::process::exit(1);
        }
    }

    let lua = Lua::new();
    let mut pipeline = Pipeline::new();
    // /sdcard (FUSE) may not be mounted yet when the daemon starts from
    // service.sh; keep the pipeline empty and retry until the dir is readable.
    let mut plugins_ready = fs::read_dir(&cfg.plugin_dir).is_ok();
    if plugins_ready {
        let _ = plugin::load_plugins(
            &lua,
            &cfg.plugin_dir,
            &mut pipeline,
            &cfg.values,
            &cfg.plugin_order,
        );
    } else {
        eprintln!(
            "keyforge: plugin dir not reachable yet: {}; will retry",
            cfg.plugin_dir
        );
    }
    // Physical source selection is Lua-driven too: scripts call
    // uh.source(vid, pid); the request is persisted here and picked up below.
    let source_ctl = crate::uhid::ensure_source(&lua, cfg.vid, cfg.pid);
    // Scripts declare UHID devices via the `uh` global; drain their kernel
    // queues every loop so OUTPUT/GET_REPORT never pile up unread.
    let mut uh_registry = crate::uhid::registry(&lua);
    let mut dev = Device::new(hidden_state_path);
    let mut mirror: Option<Mirror> = None;
    let ev_size = std::mem::size_of::<InputEvent>();
    let runtime_dir = runtime_dir();
    let raw_file_l = runtime_dir.join(RAW_FILE_L);
    let raw_file_r = runtime_dir.join(RAW_FILE_R);
    let mut pending_releases: Vec<(Instant, EmitEvent)> = Vec::new();

    // inotify for device hotplug only
    let ifd = unsafe { inotify_init1(IN_NONBLOCK) };
    let mut have_inotify = false;
    if ifd >= 0
        && let Ok(cpath) = std::ffi::CString::new(INPUT_DIR)
    {
        unsafe {
            inotify_add_watch(ifd, cpath.as_ptr(), IN_CREATE | IN_DELETE);
        }
        have_inotify = true;
    }

    let epfd = unsafe { epoll_create1(0) };
    if epfd < 0 {
        eprintln!("keyforge: epoll_create1 failed");
        std::process::exit(1);
    }

    // Register inotify fd (level-triggered)
    let mut ep_if = EpollEvent {
        events: EPOLLIN,
        data: 0,
    };
    if have_inotify {
        ep_if.data = ifd as u64;
        unsafe {
            epoll_ctl(epfd, EPOLL_CTL_ADD, ifd, &mut ep_if);
        }
    }

    let mut ep_dev = EpollEvent {
        events: EPOLLIN,
        data: 0,
    };

    // Find device, wait 1s, then create virtual device
    connect_device(
        &mut dev,
        cfg.vid,
        cfg.pid,
        cfg.hide_device && allow_device_hide,
        &mut mirror,
    );
    // epoll returns the opaque data stored at registration time.  Without
    // setting it here, the initial device's events carry data=0 and never
    // match dev.fd below (the reconnect path already did this correctly).
    ep_dev.data = dev.fd as u64;
    unsafe {
        epoll_ctl(epfd, EPOLL_CTL_ADD, dev.fd, &mut ep_dev);
    }
    let mut have_dev = true;

    let mut last_cfg_check = Instant::now();
    let mut force_cfg_check = false;
    loop {
        // Config reload check (every ~500ms via timeout, or on inotify wake)
        let now = Instant::now();
        if force_cfg_check || now.duration_since(last_cfg_check).as_millis() >= 500 {
            force_cfg_check = false;
            last_cfg_check = now;

            // Check if physical device is still alive (handles silent disconnect)
            if have_dev && !Device::is_alive(dev.fd) {
                eprintln!("keyforge: device gone (alive check failed)");
                unsafe {
                    epoll_ctl(epfd, EPOLL_CTL_DEL, dev.fd, &mut ep_dev);
                }
                dev.deinit();
                if let Some(mirror) = mirror.take() {
                    mirror.destroy();
                }
                have_dev = false;
                pending_releases.clear();
            }

            // Lua-requested source switch: persist first so the normal reload
            // path below reconnects on this same tick.
            if let Some((vid, pid)) = source_ctl.take_request() {
                eprintln!("keyforge: source switch requested: vid={vid:04x} pid={pid:04x}");
                if let Err(error) = Config::persist_source(&config_path, vid, pid) {
                    eprintln!("keyforge: failed to persist source: {error}");
                }
            }

            let fresh = Config::load(&config_path);

            let vid_changed = fresh.vid != cfg.vid || fresh.pid != cfg.pid;
            let hide_changed = fresh.hide_device != cfg.hide_device;
            let settings_changed = fresh.values != cfg.values
                || fresh.plugin_dir != cfg.plugin_dir
                || fresh.plugin_order != cfg.plugin_order;

            if settings_changed {
                pipeline = Pipeline::new();
                let _ = plugin::load_plugins(
                    &lua,
                    &fresh.plugin_dir,
                    &mut pipeline,
                    &fresh.values,
                    &fresh.plugin_order,
                );
                plugins_ready = fs::read_dir(&fresh.plugin_dir).is_ok();
            } else if !plugins_ready && fs::read_dir(&fresh.plugin_dir).is_ok() {
                eprintln!("keyforge: plugin dir became reachable; loading plugins");
                let _ = plugin::load_plugins(
                    &lua,
                    &fresh.plugin_dir,
                    &mut pipeline,
                    &fresh.values,
                    &fresh.plugin_order,
                );
                plugins_ready = true;
            }
            if vid_changed && have_dev {
                unsafe {
                    epoll_ctl(epfd, EPOLL_CTL_DEL, dev.fd, &mut ep_dev);
                }
                dev.deinit();
                if let Some(mirror) = mirror.take() {
                    mirror.destroy();
                }
                have_dev = false;
                pending_releases.clear();
            } else if hide_changed && have_dev {
                let should_hide = fresh.hide_device && allow_device_hide;
                if let Err(error) = dev.set_hidden(should_hide) {
                    eprintln!("keyforge: failed to update physical device visibility: {error}");
                    unsafe {
                        epoll_ctl(epfd, EPOLL_CTL_DEL, dev.fd, &mut ep_dev);
                    }
                    dev.deinit();
                    if let Some(mirror) = mirror.take() {
                        mirror.destroy();
                    }
                    have_dev = false;
                    pending_releases.clear();
                } else if should_hide {
                    eprintln!("keyforge: physical device hidden from Android");
                } else {
                    eprintln!("keyforge: physical device visible to Android");
                }
            }
            if hide_changed && fresh.hide_device && !allow_device_hide {
                eprintln!("keyforge: device hiding ignored outside KernelSU or Magisk");
            }
            cfg = fresh;
            source_ctl.set_current(cfg.vid, cfg.pid);
        }

        // Auto-connect if no device
        if !have_dev {
            connect_device(
                &mut dev,
                cfg.vid,
                cfg.pid,
                cfg.hide_device && allow_device_hide,
                &mut mirror,
            );
            ep_dev.data = dev.fd as u64;
            unsafe {
                epoll_ctl(epfd, EPOLL_CTL_ADD, dev.fd, &mut ep_dev);
            }
            have_dev = true;
        }

        // Flush pending releases
        let mut pctx = ProcCtx {
            pipeline: &pipeline,
            values: &cfg.values,
            pending: &mut pending_releases,
        };
        let out = match mirror.as_mut() {
            Some(mirror) => mirror,
            None => continue,
        };
        flush_pending_releases(&mut pctx, out);
        if uh_registry.is_none() {
            uh_registry = crate::uhid::registry(&lua);
        }
        if let Some(registry) = uh_registry.as_ref() {
            registry.pump();
        }

        // epoll_wait with timeout for periodic config checks
        let timeout: i32 = if pctx.pending.is_empty() { 500 } else { 50 };
        let mut events = [EpollEvent::default(); 2];
        if unsafe { epoll_wait(epfd, events.as_mut_ptr(), 2, timeout) } <= 0 {
            continue;
        }

        let mut fd_ready = false;
        let mut fd_hup = false;
        for ev in &events {
            if have_inotify && ev.data == ifd as u64 {
                let mut buf = [0u8; 4096];
                unsafe { while read(ifd, buf.as_mut_ptr(), buf.len()) > 0 {} }
                force_cfg_check = true;
            } else if have_dev && ev.data == dev.fd as u64 {
                fd_ready = true;
                if ev.events & (EPOLLHUP | EPOLLERR) != 0 {
                    fd_hup = true;
                }
            }
        }
        if !have_dev || (!fd_ready && !fd_hup) {
            continue;
        }

        // Read and process events
        let mut disconnected = false;
        loop {
            let mut iev = InputEvent::default();
            let rb = unsafe { read(dev.fd, &mut iev as *mut _ as *mut u8, ev_size) };
            if rb != ev_size as isize {
                // rb == 0 (EOF / device gone), HUP, non-EAGAIN error, or device
                // silently gone (level-triggered epoll keeps reporting fd ready but
                // read returns EAGAIN) → disconnect
                if rb == 0
                    || fd_hup
                    || (rb < 0 && get_errno() != EAGAIN)
                    || (rb < 0 && have_dev && !Device::is_alive(dev.fd))
                {
                    disconnected = true;
                }
                break;
            }
            match iev.type_ as i32 {
                EV_ABS => match iev.code as u32 {
                    ABS_X => {
                        dev.lx = iev.value;
                        dev.ld = true;
                    }
                    ABS_Y => {
                        dev.ly = iev.value;
                        dev.ld = true;
                    }
                    ABS_RX => {
                        dev.rx = iev.value;
                        dev.rd = true;
                    }
                    ABS_RY => {
                        dev.ry = iev.value;
                        dev.rd = true;
                    }
                    ABS_Z => {
                        process_trigger(iev.value, Side::Left, &mut pctx, out);
                    }
                    ABS_RZ => {
                        process_trigger(iev.value, Side::Right, &mut pctx, out);
                    }
                    _ => {}
                },
                EV_KEY => {
                    let mut e = Event::Button {
                        code: iev.code,
                        pressed: iev.value != 0,
                    };
                    let (emits, dropped) = pipeline.run(&mut e, &cfg.values);
                    flush_emits(&mut pctx, out, &emits);
                    if !dropped {
                        out.key(e.code(), e.pressed());
                    }
                }
                EV_SYN if iev.code as u32 == SYN_REPORT => {
                    let _ = fs::write(&raw_file_l, format!("{} {}", dev.lx, dev.ly));
                    let _ = fs::write(&raw_file_r, format!("{} {}", dev.rx, dev.ry));
                    if dev.ld {
                        process_stick(Side::Left, dev.lx, dev.ly, ABS_X, ABS_Y, &mut pctx, out);
                        dev.ld = false;
                    }
                    if dev.rd {
                        process_stick(Side::Right, dev.rx, dev.ry, ABS_RX, ABS_RY, &mut pctx, out);
                        dev.rd = false;
                    }
                }
                _ => {}
            }
        }
        if let Err(error) = out.flush() {
            eprintln!("keyforge: mirror flush failed: {error}");
        }
        if disconnected {
            unsafe {
                epoll_ctl(epfd, EPOLL_CTL_DEL, dev.fd, &mut ep_dev);
            }
            dev.deinit();
            if let Some(mirror) = mirror.take() {
                mirror.destroy();
            }
            have_dev = false;
            pending_releases.clear();
        }
    }
}

/// Find and grab the physical device, create its UHID virtual mirror, then
/// remove the physical event node so Android's EventHub unregisters it.
fn connect_device(
    dev: &mut Device,
    vid: u16,
    pid: u16,
    hide_device: bool,
    mirror: &mut Option<Mirror>,
) {
    loop {
        if let Some((fd, path)) = Device::find_device(vid, pid) {
            dev.fd = fd;
            dev.path = Some(path.clone());
            eprintln!(
                "keyforge: controller detected at {} (vid={:04x} pid={:04x}), waiting 1s before activation",
                path.display(),
                vid,
                pid
            );
            std::thread::sleep(Duration::from_millis(1000));
            if hide_device {
                if let Err(error) = dev.set_hidden(true) {
                    eprintln!("keyforge: physical device isolation failed: {error}; retrying");
                    dev.deinit();
                    std::thread::sleep(Duration::from_millis(1000));
                    continue;
                }
                eprintln!("keyforge: physical device removed from Android EventHub");
            }
            let (codes, abs) = Device::read_caps(dev.fd);
            match Mirror::create(
                "KeyForge Virtual Controller",
                vid,
                0x02d1,
                codes,
                crate::uhid::mirror_axes(&abs),
            ) {
                Ok(created) => {
                    *mirror = Some(created);
                    eprintln!("keyforge: virtual device created");
                    return;
                }
                Err(error) => {
                    eprintln!("keyforge: virtual device creation failed: {error}; retrying");
                    dev.deinit();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}
/// Flush expired pending key releases into the mirror state.
fn flush_pending_releases(pctx: &mut ProcCtx, out: &mut Mirror) {
    if pctx.pending.is_empty() {
        return;
    }
    let now = Instant::now();
    let mut i = 0;
    while i < pctx.pending.len() {
        if pctx.pending[i].0 <= now {
            let emit = pctx.pending.swap_remove(i).1;
            apply_emit(out, &emit);
        } else {
            i += 1;
        }
    }
    if let Err(error) = out.flush() {
        eprintln!("keyforge: mirror flush failed: {error}");
    }
}

/// Apply one emit (or release) to the mirror state.
fn apply_emit(out: &mut Mirror, emit: &EmitEvent) {
    match emit.ev_type as i32 {
        EV_KEY => out.key(emit.code, emit.value != 0),
        EV_ABS => out.abs(emit.code as u32, emit.value),
        _ => {}
    }
}
/// Write emitted events from plugins into the mirror, scheduling holds.
fn flush_emits(pctx: &mut ProcCtx, out: &mut Mirror, emits: &[EmitEvent]) {
    for emit in emits {
        apply_emit(out, emit);
        if let Some(ms) = emit.hold_ms
            && emit.value == 1
            && emit.ev_type == EV_KEY as u16
        {
            pctx.pending.push((
                Instant::now() + Duration::from_millis(ms),
                EmitEvent {
                    ev_type: emit.ev_type,
                    code: emit.code,
                    value: 0,
                    hold_ms: None,
                },
            ));
        }
    }
}

/// Process a trigger event through the pipeline into the mirror.
fn process_trigger(value: i32, side: Side, pctx: &mut ProcCtx, out: &mut Mirror) {
    let mut e = Event::Trigger { value, side };
    let (emits, dropped) = pctx.pipeline.run(&mut e, pctx.values);
    flush_emits(pctx, out, &emits);
    if dropped {
        return;
    }
    let code = match side {
        Side::Left => ABS_Z,
        Side::Right => ABS_RZ,
    };
    out.abs(code, e.value());
}

/// Process a stick event through the pipeline into the mirror.
fn process_stick(
    side: Side,
    x: i32,
    y: i32,
    code_x: u32,
    code_y: u32,
    pctx: &mut ProcCtx,
    out: &mut Mirror,
) {
    let mut e = Event::Stick { x, y, side };
    let (emits, dropped) = pctx.pipeline.run(&mut e, pctx.values);
    if !dropped {
        out.abs(code_x, e.x());
        out.abs(code_y, e.y());
    }
    flush_emits(pctx, out, &emits);
}
