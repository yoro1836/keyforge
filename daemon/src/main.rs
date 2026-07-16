mod config;
mod core;
mod pipeline;
mod plugin;

use config::Config;
use core::*;
use pipeline::{Event, Pipeline, Side};
use mlua::Lua;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    let mut config_path = "/sdcard/.keyforge/keyforge.conf".to_string();
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--config" && i + 1 < args.len() { i += 1; config_path = args[i].clone(); }
        i += 1;
    }
    let config_path = Path::new(&config_path);
    let mut cfg = Config::load(config_path);
    let lua: &'static Lua = Box::leak(Box::new(Lua::new()));
    let mut pipeline = Pipeline::new();
    let _ = plugin::load_plugins(lua, &cfg.plugin_dir, &mut pipeline, &cfg.values);
    let mut dev = Device::new();
    let ev_size = std::mem::size_of::<InputEvent>();
    let mut pending_releases: Vec<(Instant, pipeline::EmitEvent)> = Vec::new();

    // inotify for device hotplug only
    let ifd = unsafe { inotify_init1(IN_NONBLOCK) };
    let mut have_inotify = false;
    if ifd >= 0 {
        if let Ok(cpath) = std::ffi::CString::new(INPUT_DIR) {
            unsafe { inotify_add_watch(ifd, cpath.as_ptr(), IN_CREATE | IN_DELETE); }
            have_inotify = true;
        }
    }

    let epfd = unsafe { epoll_create1(0) };
    if epfd < 0 { eprintln!("keyforge: epoll_create1 failed"); std::process::exit(1); }

    // Register inotify fd
    let mut ep_if = EpollEvent { events: EPOLLIN, data: 0 };
    if have_inotify {
        ep_if.data = ifd as u64;
        unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, ifd, &mut ep_if); }
    }

    let mut ep_dev = EpollEvent { events: EPOLLIN, data: 0 };

    // Find device, wait 1s, then create virtual device
    connect_device(&mut dev, cfg.vid, cfg.pid);
    unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, dev.fd, &mut ep_dev); }
    let mut have_dev = true;

    let mut last_cfg_check = Instant::now();
    let mut force_cfg_check = false;
    loop {
        // Config reload check (every ~500ms via timeout, or on inotify wake)
        let now = Instant::now();
        if force_cfg_check || now.duration_since(last_cfg_check).as_millis() >= 500 {
            force_cfg_check = false;
            last_cfg_check = now;
            let fresh = Config::load(config_path);
            let vid_changed = fresh.vid != cfg.vid || fresh.pid != cfg.pid;
            let settings_changed = fresh.values != cfg.values || fresh.plugin_dir != cfg.plugin_dir;
            if vid_changed || settings_changed {
                cfg = fresh;
                pipeline = Pipeline::new();
                let _ = plugin::load_plugins(lua, &cfg.plugin_dir, &mut pipeline, &cfg.values);
                if vid_changed && have_dev {
                    unsafe { epoll_ctl(epfd, EPOLL_CTL_DEL, dev.fd, &mut ep_dev); }
                    dev.deinit();
                    have_dev = false;
                }
            }
        }

        // Auto-connect if no device
        if !have_dev {
            connect_device(&mut dev, cfg.vid, cfg.pid);
            ep_dev.data = dev.fd as u64;
            unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, dev.fd, &mut ep_dev); }
            have_dev = true;
        }

        // Flush pending releases
        flush_pending_releases(dev.ufd, &mut pending_releases);

        // epoll_wait with timeout for periodic config checks
        let timeout: i32 = if pending_releases.is_empty() { 500 } else { 50 };
        let mut events = [EpollEvent::default(); 2];
        if unsafe { epoll_wait(epfd, events.as_mut_ptr(), 2, timeout) } <= 0 { continue; }

        let mut fd_ready = false; let mut fd_hup = false;
        for ev in &events {
            if have_inotify && ev.data == ifd as u64 {
                let mut buf = [0u8; 4096];
                unsafe { while read(ifd, buf.as_mut_ptr(), buf.len()) > 0 {} }
                force_cfg_check = true;
            } else if have_dev && ev.data == dev.fd as u64 {
                fd_ready = true;
                if ev.events & (EPOLLHUP | EPOLLERR) != 0 { fd_hup = true; }
            }
        }
        if !have_dev || (!fd_ready && !fd_hup) { continue; }

        // Read and process events
        let mut disconnected = false;
        loop {
            let mut iev = InputEvent::default();
            let rb = unsafe { read(dev.fd, &mut iev as *mut _ as *mut u8, ev_size) };
            if rb != ev_size as isize {
                if (rb < 0 && get_errno() != EAGAIN) || fd_hup { disconnected = true; }
                break;
            }
            unsafe {
                let mut skip = false;
                match iev.type_ as i32 {
                    EV_ABS => match iev.code as u32 {
                        ABS_X  => { dev.lx = iev.value; dev.ld = true; skip = true; }
                        ABS_Y  => { dev.ly = iev.value; dev.ld = true; skip = true; }
                        ABS_RX => { dev.rx = iev.value; dev.rd = true; skip = true; }
                        ABS_RY => { dev.ry = iev.value; dev.rd = true; skip = true; }
                        ABS_Z  => { if process_trigger(&mut iev, Side::Left,  &pipeline, &cfg.values, dev.ufd, &mut pending_releases) { skip = true; } }
                        ABS_RZ => { if process_trigger(&mut iev, Side::Right, &pipeline, &cfg.values, dev.ufd, &mut pending_releases) { skip = true; } }
                        _ => {}
                    },
                    EV_KEY => {
                        let mut e = Event::Button { code: iev.code, pressed: iev.value != 0 };
                        let (emits, dropped) = pipeline.run(&mut e, &cfg.values);
                        flush_emits(dev.ufd, &iev, &emits, &mut pending_releases);
                        if dropped { skip = true; }
                        else { iev.value = if e.pressed() { 1 } else { 0 }; }
                    }
                    EV_SYN if iev.code as u32 == SYN_REPORT => {
                        let _ = fs::write(RAW_FILE_L, format!("{} {}", dev.lx, dev.ly));
                        let _ = fs::write(RAW_FILE_R, format!("{} {}", dev.rx, dev.ry));
                        if dev.ld {
                            process_stick(&iev, Side::Left, dev.lx, dev.ly, ABS_X as u16, ABS_Y as u16, &pipeline, &cfg.values, dev.ufd, &mut pending_releases);
                            dev.ld = false;
                        }
                        if dev.rd {
                            process_stick(&iev, Side::Right, dev.rx, dev.ry, ABS_RX as u16, ABS_RY as u16, &pipeline, &cfg.values, dev.ufd, &mut pending_releases);
                            dev.rd = false;
                        }
                    }
                    _ => {}
                }
                if !skip { write_ev(dev.ufd, &iev); }
            }
        }
        if disconnected {
            unsafe { epoll_ctl(epfd, EPOLL_CTL_DEL, dev.fd, &mut ep_dev); }
            dev.deinit();
            have_dev = false;
            pending_releases.clear();
        }
    }
}

/// Find physical device, wait 1s, then create virtual uinput device.
/// Retries until success.
fn connect_device(dev: &mut Device, vid: u16, pid: u16) {
    loop {
        dev.fd = Device::find_device(vid, pid);
        if dev.fd >= 0 {
            eprintln!("keyforge: controller detected (vid={:04x} pid={:04x}), waiting 1s before creating virtual device", vid, pid);
            std::thread::sleep(Duration::from_millis(1000));
            if dev.init_u(dev.fd, vid) {
                eprintln!("keyforge: virtual device created");
                return;
            }
            eprintln!("keyforge: virtual device creation failed, retrying");
            dev.deinit();
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

/// Flush expired pending key releases to the virtual device.
fn flush_pending_releases(ufd: i32, pending: &mut Vec<(Instant, pipeline::EmitEvent)>) {
    if pending.is_empty() { return; }
    let now = Instant::now();
    let mut i = 0;
    while i < pending.len() {
        if pending[i].0 <= now {
            let emit = pending.swap_remove(i).1;
            let mut rev = InputEvent::default();
            rev.type_ = emit.ev_type; rev.code = emit.code; rev.value = 0;
            unsafe { write_ev(ufd, &rev); }
            rev.type_ = EV_SYN as u16; rev.code = SYN_REPORT as u16; rev.value = 0;
            unsafe { write_ev(ufd, &rev); }
            pending.swap_remove(i);
        } else { i += 1; }
    }
}

/// Write emitted events from plugin to virtual device, scheduling hold releases.
fn flush_emits(ufd: i32, base: &InputEvent, emits: &[pipeline::EmitEvent], pending: &mut Vec<(Instant, pipeline::EmitEvent)>) {
    for emit in emits {
        let mut se = *base; se.type_ = emit.ev_type; se.code = emit.code; se.value = emit.value;
        unsafe { write_ev(ufd, &se); }
        if let Some(ms) = emit.hold_ms
            && emit.value == 1 && emit.ev_type == EV_KEY as u16 {
            pending.push((Instant::now() + Duration::from_millis(ms),
                pipeline::EmitEvent { ev_type: emit.ev_type, code: emit.code, value: 0, hold_ms: None }));
        }
    }
}

/// Process a trigger event through the pipeline. Returns true if dropped (skip original).
fn process_trigger(iev: &mut InputEvent, side: Side, pipeline: &Pipeline, values: &HashMap<String, String>, ufd: i32, pending: &mut Vec<(Instant, pipeline::EmitEvent)>) -> bool {
    let mut e = Event::Trigger { value: iev.value, side };
    let (emits, dropped) = pipeline.run(&mut e, values);
    flush_emits(ufd, iev, &emits, pending);
    if dropped { return true; }
    iev.value = e.value();
    false
}

/// Process a stick event through the pipeline and write axis values.
fn process_stick(iev: &InputEvent, side: Side, x: i32, y: i32, code_x: u16, code_y: u16, pipeline: &Pipeline, values: &HashMap<String, String>, ufd: i32, pending: &mut Vec<(Instant, pipeline::EmitEvent)>) {
    let mut e = Event::Stick { x, y, side };
    let (emits, _) = pipeline.run(&mut e, values);
    let mut se = *iev; se.type_ = EV_ABS as u16;
    se.code = code_x; se.value = e.x(); unsafe { write_ev(ufd, &se); }
    se.code = code_y; se.value = e.y(); unsafe { write_ev(ufd, &se); }
    flush_emits(ufd, iev, &emits, pending);
}
