//! macOS (arm64) backend: CoreAudio **process tap**
//! (`AudioHardwareCreateProcessTap` + `CATapDescription`, macOS 14.4+).
//!
//! Deliberately minimal and official-API-only (no ScreenCaptureKit, no
//! legacy workarounds): the tap is created for the target pid and behaves
//! like an input device — we read it with a classic `AudioDeviceIOProc`.
//! The 14.4-only symbols are resolved with `dlsym` at capture start so the
//! plugin still *loads* on older macOS and reports a precise error instead
//! of failing at `dlopen`.
//!
//! Foreground application pid via `NSWorkspace.frontmostApplication`
//! (raw objc msgSend — no AppKit dependency needed).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::capture::{engage_capturing, resampler_for, WorkerShared, PHASE_FINISHED, PHASE_RUNNING};
use crate::resample::SincResampler;
use crate::ring::AudioRing;

type OSStatus = i32;
type AudioObjectID = u32;

#[repr(C)]
struct AudioObjectPropertyAddress {
    m_selector: u32,
    m_scope: u32,
    m_element: u32,
}

#[repr(C)]
struct AudioBuffer {
    m_number_channels: u32,
    m_data_byte_size: u32,
    m_data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    m_number_buffers: u32,
    m_buffers: [AudioBuffer; 1], // followed by m_number_buffers-1 more
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamBasicDescription {
    m_sample_rate: f64,
    m_format_id: u32,
    m_format_flags: u32,
    m_bytes_per_packet: u32,
    m_frames_per_packet: u32,
    m_bytes_per_frame: u32,
    m_channels_per_frame: u32,
    m_bits_per_channel: u32,
    _reserved: u32,
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}
const K_AUDIO_DEVICE_PROPERTY_STREAM_FORMAT: u32 = fourcc(b"fmt ");
const K_AUDIO_OBJECT_PROPERTY_SCOPE_INPUT: u32 = fourcc(b"inpt");
const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = fourcc(b"glob");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 0x1;
const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 0x4;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: *mut u32,
        data: *mut c_void,
    ) -> OSStatus;
    fn AudioDeviceCreateIOProcID(
        device: AudioObjectID,
        proc_: AudioDeviceIOProc,
        client_data: *mut c_void,
        proc_id: *mut u32,
    ) -> OSStatus;
    fn AudioDeviceStart(device: AudioObjectID, proc_: AudioDeviceIOProc) -> OSStatus;
    fn AudioDeviceStop(device: AudioObjectID, proc_: AudioDeviceIOProc) -> OSStatus;
    fn AudioDeviceDestroyIOProcID(device: AudioObjectID, proc_id: u32) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFNumberCreate(alloc: *const c_void, the_type: i32, value_ptr: *const c_void) -> *mut c_void;
    fn CFArrayCreate(
        alloc: *const c_void,
        values: *const *const c_void,
        num_values: i64,
        callbacks: *const c_void,
    ) -> *mut c_void;
    fn CFRelease(cf: *mut c_void);
}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const u8) -> *mut c_void;
    fn sel_registerName(name: *const u8) -> *mut c_void;
    fn objc_msgSend();
}

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const u8) -> *mut c_void;
    fn proc_pidpath(pid: i32, buf: *mut u8, bufsize: u32) -> i32;
}

const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

/// 14.4+ symbols, resolved lazily.
struct TapApi {
    desc_create: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    desc_set_pids: unsafe extern "C" fn(*mut c_void, *mut c_void),
    create_tap: unsafe extern "C" fn(*mut c_void, *mut AudioObjectID) -> OSStatus,
    destroy_tap: unsafe extern "C" fn(AudioObjectID) -> OSStatus,
}

impl TapApi {
    fn resolve() -> Result<Self, String> {
        unsafe fn sym(name: &[u8]) -> Option<*mut c_void> {
            let p = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) };
            (!p.is_null()).then_some(p)
        }
        macro_rules! fetch {
            ($n:expr) => {
                sym($n).ok_or_else(|| {
                    format!(
                        "系统缺少 {}：需要 macOS 14.4 或更新版本",
                        String::from_utf8_lossy($n)
                    )
                })?
            };
        }
        Ok(TapApi {
            desc_create: unsafe {
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*const c_void) -> *mut c_void,
                >(fetch!(b"CATapDescriptionCreate\0"))
            },
            desc_set_pids: unsafe {
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void, *mut c_void),
                >(fetch!(b"CATapDescriptionSetProcessIDs\0"))
            },
            create_tap: unsafe {
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*mut c_void, *mut AudioObjectID) -> OSStatus,
                >(fetch!(b"AudioHardwareCreateProcessTap\0"))
            },
            destroy_tap: unsafe {
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(AudioObjectID) -> OSStatus>(
                    fetch!(b"AudioHardwareDestroyProcessTap\0"),
                )
            },
        })
    }
}

unsafe fn msg_send_obj(obj: *mut c_void, sel: *mut c_void) -> *mut c_void {
    type F = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    unsafe { std::mem::transmute::<unsafe extern "C" fn(), F>(objc_msgSend)(obj, sel) }
}

unsafe fn msg_send_int(obj: *mut c_void, sel: *mut c_void) -> i64 {
    type F = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i64;
    unsafe { std::mem::transmute::<unsafe extern "C" fn(), F>(objc_msgSend)(obj, sel) }
}

pub fn current_pid() -> u32 {
    std::process::id()
}

pub fn foreground_pid() -> Option<u32> {
    unsafe {
        let cls = objc_getClass(c"NSWorkspace".as_ptr() as *const u8);
        if cls.is_null() {
            return None;
        }
        let sel_shared = sel_registerName(c"sharedWorkspace".as_ptr() as *const u8);
        let sel_front = sel_registerName(c"frontmostApplication".as_ptr() as *const u8);
        let sel_pid = sel_registerName(c"processIdentifier".as_ptr() as *const u8);
        let ws = msg_send_obj(cls, sel_shared);
        if ws.is_null() {
            return None;
        }
        let app = msg_send_obj(ws, sel_front);
        if app.is_null() {
            return None;
        }
        let pid = msg_send_int(app, sel_pid);
        (pid > 0).then_some(pid as u32)
    }
}

pub fn process_name_by_pid(pid: u32) -> String {
    let mut buf = [0u8; 1024];
    let n = unsafe { proc_pidpath(pid as i32, buf.as_mut_ptr(), buf.len() as u32) };
    if n <= 0 {
        return String::new();
    }
    let path = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
    path.rsplit('/').next().unwrap_or(&path).to_string()
}

/// Per-capture state handed to the IO proc as `client_data`.
/// `stop_ptr` points at the caller's stop flag; `reap_worker`'s join
/// guarantees it outlives the IO proc.
struct MacState {
    stop_ptr: *const AtomicBool,
    ring: &'static AudioRing,
    resampler: Option<SincResampler>,
    mono: Vec<f32>,
    out48: Vec<f32>,
    channels: usize,
    interleaved: bool,
}

// SAFETY: MacState travels only as a raw pointer installed on our own thread
// and read by the CoreAudio IO proc; all mutable fields are owned by that
// callback exclusively.
unsafe impl Send for MacState {}

unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const c_void,
    in_data: *const AudioBufferList,
    _in_time: *const c_void,
    _out_data: *mut c_void,
    _out_time: *const c_void,
    client_data: *mut c_void,
) -> OSStatus {
    let st = unsafe { &mut *(client_data as *mut MacState) };
    if unsafe { &*st.stop_ptr }.load(Ordering::Acquire) || in_data.is_null() {
        return 0;
    }
    unsafe {
        let list = &*in_data;
        let nbuf = list.m_number_buffers as usize;
        if nbuf == 0 {
            return 0;
        }
        let buffers = std::slice::from_raw_parts(
            list.m_buffers.as_ptr(),
            nbuf,
        );
        let first = &buffers[0];
        let frames = if st.interleaved {
            let ch = st.channels.max(1);
            (first.m_data_byte_size as usize / 4) / ch
        } else {
            first.m_data_byte_size as usize / 4
        };
        if frames == 0 || first.m_data.is_null() {
            return 0;
        }
        st.mono.clear();
        st.mono.reserve(frames);
        if st.interleaved {
            // Single buffer, channels interleaved (Float32).
            let samples = std::slice::from_raw_parts(first.m_data as *const f32, frames * st.channels.max(1));
            let inv = 1.0 / st.channels.max(1) as f32;
            for frame in samples.chunks(st.channels.max(1)) {
                let mut sum = 0.0f32;
                for &x in frame {
                    sum += x;
                }
                st.mono.push(sum * inv);
            }
        } else {
            // De-interleaved: one Float32 buffer per channel.
            let chans: Vec<&[f32]> = buffers
                .iter()
                .filter(|b| !b.m_data.is_null())
                .map(|b| {
                    std::slice::from_raw_parts(b.m_data as *const f32, (b.m_data_byte_size as usize / 4).min(frames))
                })
                .collect();
            let inv = 1.0 / chans.len().max(1) as f32;
            for f in 0..frames {
                let mut sum = 0.0f32;
                for c in &chans {
                    sum += c.get(f).copied().unwrap_or(0.0);
                }
                st.mono.push(sum * inv);
            }
        }
        let mono = &mut st.mono;
        let out = &mut st.out48;
        match &mut st.resampler {
            Some(res) => {
                res.process(mono, out);
                st.ring.push(out);
            }
            None => {
                st.ring.push(mono);
            }
        }
    }
    0
}

type AudioDeviceIOProc = unsafe extern "C" fn(
    AudioObjectID,
    *const c_void,
    *const AudioBufferList,
    *const c_void,
    *mut c_void,
    *const c_void,
    *mut c_void,
) -> OSStatus;

pub fn spawn_worker(
    pid: u32,
    ring: &'static AudioRing,
    stop: Arc<AtomicBool>,
    shared: Arc<WorkerShared>,
    capturing: &'static AtomicBool,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("fc-processtap".into())
        .spawn(move || {
            crate::fctrace!("worker(macos): thread start pid={pid}");
            if let Err(e) = run(pid, ring, &stop, &shared, capturing) {
                crate::fctrace!("worker(macos): error: {e}");
                if let Ok(mut slot) = shared.error.lock() {
                    *slot = e;
                }
            }
            crate::fctrace!("worker(macos): thread exit (phase->FINISHED)");
            shared.phase.store(PHASE_FINISHED, Ordering::Release);
        })
        .map_err(std::io::Error::other)
}

fn run(
    pid: u32,
    ring: &'static AudioRing,
    stop: &AtomicBool,
    shared: &WorkerShared,
    capturing: &'static AtomicBool,
) -> Result<(), String> {
    let api = TapApi::resolve()?;

    // ── Create the process tap for the target pid ──
    let desc = unsafe { (api.desc_create)(std::ptr::null()) };
    if desc.is_null() {
        return Err("CATapDescriptionCreate 失败".into());
    }
    let pid_i32 = pid as i32;
    let num = unsafe { CFNumberCreate(std::ptr::null(), 9 /*kCFNumberIntType*/, &pid_i32 as *const i32 as *const c_void) };
    if num.is_null() {
        return Err("CFNumberCreate 失败".into());
    }
    let values = [num as *const c_void];
    let arr = unsafe { CFArrayCreate(std::ptr::null(), values.as_ptr(), 1, std::ptr::null()) };
    unsafe { CFRelease(num) };
    if arr.is_null() {
        return Err("CFArrayCreate 失败".into());
    }
    unsafe { (api.desc_set_pids)(desc, arr) };
    unsafe { CFRelease(arr) };

    let mut tap: AudioObjectID = 0;
    let st = unsafe { (api.create_tap)(desc, &mut tap) };
    unsafe { CFRelease(desc) };
    if st != 0 || tap == 0 {
        return Err(format!(
            "AudioHardwareCreateProcessTap 失败 (OSStatus {st})：目标进程可能已退出或拒绝被捕获"
        ));
    }
    crate::fctrace!("worker(macos): tap created, object id {tap}");

    // ── Query the tap's stream format (Float32; interleaved or not) ──
    let mut asbd = AudioStreamBasicDescription {
        m_sample_rate: 48000.0,
        m_format_id: 0,
        m_format_flags: 0,
        m_bytes_per_packet: 0,
        m_frames_per_packet: 1,
        m_bytes_per_frame: 0,
        m_channels_per_frame: 1,
        m_bits_per_channel: 32,
        _reserved: 0,
    };
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let addr = AudioObjectPropertyAddress {
        m_selector: K_AUDIO_DEVICE_PROPERTY_STREAM_FORMAT,
        m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_INPUT,
        m_element: 0,
    };
    let r = unsafe {
        AudioObjectGetPropertyData(
            tap,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut asbd as *mut _ as *mut c_void,
        )
    };
    if r != 0 {
        // Some taps expose the format in global scope.
        let addr_g = AudioObjectPropertyAddress {
            m_selector: K_AUDIO_DEVICE_PROPERTY_STREAM_FORMAT,
            m_scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            m_element: 0,
        };
        let mut size_g = size;
        let r2 = unsafe {
            AudioObjectGetPropertyData(
                tap,
                &addr_g,
                0,
                std::ptr::null(),
                &mut size_g,
                &mut asbd as *mut _ as *mut c_void,
            )
        };
        if r2 != 0 {
            unsafe { (api.destroy_tap)(tap) };
            return Err(format!("读取 tap 流格式失败 (OSStatus {r}/{r2})"));
        }
    }
    let rate = asbd.m_sample_rate as u32;
    let channels = asbd.m_channels_per_frame.max(1) as usize;
    let interleaved = asbd.m_format_flags & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED == 0;
    let is_float = asbd.m_format_flags & K_AUDIO_FORMAT_FLAG_IS_FLOAT != 0;
    if !is_float || asbd.m_bits_per_channel != 32 {
        unsafe { (api.destroy_tap)(tap) };
        return Err("tap 流格式非 Float32（不支持）".into());
    }
    crate::fctrace!(
        "worker(macos): tap format {}Hz {}ch interleaved={interleaved}",
        rate,
        channels
    );

    // ── IO proc ──
    let state_ptr = Box::into_raw(Box::new(MacState {
        stop_ptr: stop as *const AtomicBool,
        ring,
        resampler: resampler_for(rate),
        mono: Vec::with_capacity(4096),
        out48: Vec::with_capacity(4096),
        channels,
        interleaved,
    }));

    let mut proc_id: u32 = 0;
    let r = unsafe { AudioDeviceCreateIOProcID(tap, io_proc, state_ptr as *mut c_void, &mut proc_id) };
    if r != 0 {
        unsafe {
            drop(Box::from_raw(state_ptr));
            (api.destroy_tap)(tap);
        }
        return Err(format!("AudioDeviceCreateIOProcID 失败 (OSStatus {r})"));
    }
    let r = unsafe { AudioDeviceStart(tap, io_proc) };
    if r != 0 {
        unsafe {
            AudioDeviceDestroyIOProcID(tap, proc_id);
            drop(Box::from_raw(state_ptr));
            (api.destroy_tap)(tap);
        }
        return Err(format!("AudioDeviceStart 失败 (OSStatus {r})"));
    }

    shared.rate.store(rate, Ordering::Relaxed);
    shared.channels.store(channels as u32, Ordering::Relaxed);
    shared.phase.store(PHASE_RUNNING, Ordering::Release);
    engage_capturing(stop, capturing);
    crate::fctrace!("worker(macos): running");

    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    crate::fctrace!("worker(macos): leaving loop, tearing down");
    unsafe {
        AudioDeviceStop(tap, io_proc);
        AudioDeviceDestroyIOProcID(tap, proc_id);
        drop(Box::from_raw(state_ptr));
        (api.destroy_tap)(tap);
    }
    Ok(())
}

