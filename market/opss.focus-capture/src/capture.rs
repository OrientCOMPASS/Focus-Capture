//! Per-process audio capture: platform dispatch + shared state/conversion.
//!
//! Backends (each implements the same four-function contract):
//! * Windows — WASAPI **process loopback** (`VAD\Process_Loopback`, Win10 2004+), see `backend_windows.rs`
//! * Linux   — **PipeWire** stream-to-stream capture (`node.target` = the app's output node), X11 foreground lookup, see `backend_linux.rs`
//! * macOS   — CoreAudio **process tap** (`AudioHardwareCreateProcessTap`, macOS 14.4+), see `backend_macos.rs`
//!
//! Threading contract (api-reference.md §使用注意事项): backends run ONLY on
//! the dedicated worker thread spawned by the bus thread; they never call
//! MicYou host APIs — state flows back through `WorkerShared`/atomics and
//! samples flow through the SPSC [`AudioRing`].

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8};
use std::sync::Mutex;

use crate::TARGET_RATE;

/// Worker lifecycle phase (observed by the bus-thread watchdog).
pub const PHASE_STARTING: u8 = 0;
pub const PHASE_RUNNING: u8 = 1;
pub const PHASE_FINISHED: u8 = 2;

/// Shared state between the capture worker (writer) and the watchdog running
/// on a host-dispatched thread (reader). No host API is involved.
pub struct WorkerShared {
    pub phase: AtomicU8,
    /// Set when the target process exited; the worker then stops itself.
    pub target_exited: AtomicBool,
    /// Human-readable failure reason; non-empty only when phase == FINISHED
    /// and the stop was not user-initiated.
    pub error: Mutex<String>,
    /// Negotiated capture format (filled before phase becomes RUNNING).
    pub rate: AtomicU32,
    pub channels: AtomicU32,
    /// Diagnostics: buffers flagged discontinuous by the platform.
    pub discontinuities: AtomicU32,
}

impl WorkerShared {
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_STARTING),
            target_exited: AtomicBool::new(false),
            error: Mutex::new(String::new()),
            rate: AtomicU32::new(0),
            channels: AtomicU32::new(0),
            discontinuities: AtomicU32::new(0),
        }
    }
}

impl Default for WorkerShared {
    fn default() -> Self {
        Self::new()
    }
}

/// Interleaved `frames` × `channels` samples of `bits`/`float` format →
/// channel-averaged mono f32 appended to `out`.
///
/// # Safety
/// `src` must point at `frames * channels` samples of the declared format
/// with natural alignment (WASAPI guarantees DWORD alignment; PipeWire and
/// CoreAudio HAL buffers are page/malloc-aligned).
///
/// Shared by all backends so the downmix semantics (average, matching the
/// host's AEC reference downmix) stay identical everywhere.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub unsafe fn convert_interleaved_to_mono(
    src: *const u8,
    frames: usize,
    channels: usize,
    bits: u16,
    float: bool,
    out: &mut Vec<f32>,
) {
    let ch = channels.max(1);
    let inv = 1.0f32 / ch as f32;
    match (float, bits) {
        (true, 32) => {
            let s = unsafe { std::slice::from_raw_parts(src as *const f32, frames * ch) };
            for frame in s.chunks(ch) {
                let mut sum = 0.0f32;
                for &x in frame {
                    sum += x;
                }
                out.push(sum * inv);
            }
        }
        (false, 16) => {
            let s = unsafe { std::slice::from_raw_parts(src as *const i16, frames * ch) };
            for frame in s.chunks(ch) {
                let mut sum = 0.0f32;
                for &x in frame {
                    sum += x as f32;
                }
                out.push(sum * inv * (1.0 / 32768.0));
            }
        }
        (false, 32) => {
            let s = unsafe { std::slice::from_raw_parts(src as *const i32, frames * ch) };
            for frame in s.chunks(ch) {
                let mut sum = 0.0f64;
                for &x in frame {
                    sum += x as f64;
                }
                out.push((sum * inv as f64 * (1.0 / 2_147_483_648.0)) as f32);
            }
        }
        _ => out.resize(out.len() + frames, 0.0),
    }
}

// ─────────────────────────── backend selection ───────────────────────────

#[cfg(target_os = "windows")]
#[path = "backend_windows.rs"]
mod backend_windows;
#[cfg(target_os = "linux")]
#[path = "backend_linux.rs"]
mod backend_linux;
#[cfg(target_os = "macos")]
#[path = "backend_macos.rs"]
mod backend_macos;

#[cfg(target_os = "windows")]
pub use backend_windows::{current_pid, foreground_pid, process_name_by_pid, spawn_worker};
#[cfg(target_os = "linux")]
pub use backend_linux::{current_pid, foreground_pid, process_name_by_pid, spawn_worker};
#[cfg(target_os = "macos")]
pub use backend_macos::{current_pid, foreground_pid, process_name_by_pid, spawn_worker};

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
mod backend_stub {
    use super::*;
    use crate::ring::AudioRing;
    use std::sync::Arc;
    use std::thread::JoinHandle;

    pub fn foreground_pid() -> Option<u32> {
        None
    }
    pub fn current_pid() -> u32 {
        0
    }
    pub fn process_name_by_pid(_pid: u32) -> String {
        String::new()
    }
    pub fn spawn_worker(
        _pid: u32,
        _ring: &'static AudioRing,
        _stop: Arc<AtomicBool>,
        _shared: Arc<WorkerShared>,
        _capturing: &'static AtomicBool,
    ) -> std::io::Result<JoinHandle<()>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "process capture is implemented for Windows / Linux (PipeWire) / macOS 14.4+ only",
        ))
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub use backend_stub::{current_pid, foreground_pid, process_name_by_pid, spawn_worker};

/// Convenience for backends: engage the mixing flag with the stop-flag
/// double-check (stopper writes flag=true strictly before CAPTURING=false,
/// so a racing start always converges to stopped).
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub(crate) fn engage_capturing(stop: &AtomicBool, capturing: &'static AtomicBool) {
    if !stop.load(std::sync::atomic::Ordering::Acquire) {
        capturing.store(true, std::sync::atomic::Ordering::Release);
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            capturing.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Convenience for backends: build a 48 kHz resampler when the negotiated
/// rate differs (48 kHz is the host DSP chain's fixed rate).
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub(crate) fn resampler_for(rate: u32) -> Option<crate::resample::SincResampler> {
    (rate != TARGET_RATE).then(|| crate::resample::SincResampler::new(rate, TARGET_RATE))
}
