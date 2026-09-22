//! FocusCapture — MicYou native DSP plugin (Windows).
//!
//! 一键把「当前焦点应用」的声音混入 MicYou 麦克风流：
//!
//! * 焦点在目标应用上时按下全局快捷键 → 通过 WASAPI **process loopback**
//!   （`VAD\Process_Loopback`，Windows 10 2004+）捕获该进程渲染的音频；
//! * 捕获结果经下混（→mono）与重采样（→48 kHz）写入无锁 SPSC 环形缓冲；
//! * 宿主实时音频线程在 `micyou_plugin_process`（DSP 链 `Plugins` 节点）中
//!   把环形缓冲里的声音**加性混音**进麦克风信号（带 10 ms 淡入淡出）；
//! * 在任意应用里再次按下同一快捷键 → 立即停止混音并回收采集线程。
//!
//! 线程纪律（对应 api-reference.md「使用注意事项」）：
//!
//! | 线程 | 触碰的资源 | Host API |
//! | --- | --- | --- |
//! | 宿主总线线程（handle_message / interval tick / init / deinit） | SESSION 状态机、Host API | ✅ 仅此处调用 |
//! | 采集 worker 线程 | WASAPI、ring 生产端、原子状态 | ❌ 绝不调用 |
//! | 宿主实时音频线程（process） | ring 消费端、包络、原子增益 | ❌ 绝不调用，零分配 |

#![allow(non_camel_case_types)]

use std::cell::UnsafeCell;
use std::ffi::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

mod abi;
mod capture;
mod hotkey;
#[macro_use]
mod trace;
/// Workers resample when the negotiated capture rate differs from the host
/// chain's 48 kHz; the unit tests validate the DSP math on any platform.
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos", test))]
mod resample;
mod ring;

use abi::{mpl_host_api_t, mpl_plugin_info_t, mpl_result_t};
use capture::{WorkerShared, PHASE_FINISHED, PHASE_RUNNING};
use ring::AudioRing;

const PLUGIN_ID: &[u8] = b"opss.focus-capture\0";
const PLUGIN_VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();

/// Host DSP chain runs at a fixed 48 kHz (micyou-plugin `PluginDspBridge`).
pub const TARGET_RATE: u32 = 48_000;

/// Ring capacity: ~1.37 s of mono 48 kHz (power of two, 256 KiB).
const RING_CAP: usize = 65_536;
/// Latency clamp: consumer discards audio older than 300 ms (drop-oldest).
const BACKLOG_SAMPLES: usize = TARGET_RATE as usize * 3 / 10;
/// Fade in/out length for click-free start & stop (10 ms @ 48 kHz).
const FADE_SAMPLES: f32 = 480.0;
/// Watchdog cadence while a capture session lives (host interval timer).
const WATCHDOG_MS: u64 = 400;
const WATCHDOG_PAYLOAD: &str = "fcwd";
/// Fallback when the config key is missing/unreadable.
const DEFAULT_HOTKEY: &str = "ctrl+shift+f8";
/// Hard cap for the "starting" phase before we declare activation failed.
const START_TIMEOUT: Duration = Duration::from_secs(8);
/// Give the worker this long to wind down when reaping on the bus thread.
const REAP_POLL: Duration = Duration::from_millis(10);
const REAP_BUDGET_HOTKEY: Duration = Duration::from_millis(400);
/// deinit MUST join the worker (the library is about to be unmapped; a thread
/// still executing our code would crash the host). Activation waits up to 5 s
/// inside the worker, so the budget has to cover that pathological case.
const REAP_BUDGET_DEINIT: Duration = Duration::from_secs(6);

static RING: OnceLock<AudioRing> = OnceLock::new();

/// True while captured audio should be mixed (audio-thread fast path flag).
/// Only ever written under the SESSION lock (bus thread), read by process().
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// Linear mix gain as f32 bits (lock-free read on the audio thread).
static GAIN_BITS: AtomicU32 = AtomicU32::new(1.0f32.to_bits());

/// True when state transitions should raise system notifications.
static NOTIFY_ON: AtomicBool = AtomicBool::new(true);

/// Fade envelope state — owned exclusively by the host real-time audio thread
/// (process() is serialized on a single thread), hence UnsafeCell is sound.
struct MixState {
    env: UnsafeCell<f32>,
}
// SAFETY: single-thread (audio) ownership documented above.
unsafe impl Sync for MixState {}
static MIX: MixState = MixState {
    env: UnsafeCell::new(0.0),
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Starting,
    Running,
    Stopping,
}

struct Session {
    phase: Phase,
    /// Hotkey handle returned by register_hotkey (0 = none).
    hotkey_id: u64,
    hotkey_text: String,
    hotkey_ok: bool,
    /// Shortcut string the live `hotkey_id` registration was made for.
    host_hotkey_sc: String,
    /// False once the user changed the hotkey away from `host_hotkey_sc`
    /// (the stale host registration can never be released — ABI has no
    /// unregister — so its messages are filtered out by id).
    host_hotkey_active: bool,

    /// Watchdog interval id (0 = not armed). Armed only during a session.
    interval_id: u64,
    stop_flag: Option<Arc<AtomicBool>>,
    shared: Option<Arc<WorkerShared>>,
    worker: Option<std::thread::JoinHandle<()>>,
    started_at: Option<Instant>,
    app: String,
    pid: u32,
    rate: u32,
    channels: u32,
    /// Error to surface (notification + panel) once the worker is reaped.
    pending_report: Option<(String, String)>,
    last_error: String,
    /// Watchdog tick counter (config re-read every N ticks).
    ticks: u64,
}

impl Session {
    const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            hotkey_id: 0,
            hotkey_text: String::new(),
            hotkey_ok: false,
            host_hotkey_sc: String::new(),
            host_hotkey_active: false,

            interval_id: 0,
            stop_flag: None,
            shared: None,
            worker: None,
            started_at: None,
            app: String::new(),
            pid: 0,
            rate: 0,
            channels: 0,
            pending_report: None,
            last_error: String::new(),
            ticks: 0,
        }
    }
}

static SESSION: Mutex<Session> = Mutex::new(Session::new());

fn session() -> MutexGuard<'static, Session> {
    SESSION.lock().unwrap_or_else(|p| p.into_inner())
}

/// Catch every panic at the FFI boundary — unwinding across `extern "C"` is UB
/// and would take the whole host process down.
fn guard<F: FnOnce() -> mpl_result_t>(f: F) -> mpl_result_t {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(mpl_result_t::MPL_ERR_RUNTIME)
}

// ────────────────────────── required entry points ──────────────────────────

#[no_mangle]
pub extern "C" fn micyou_plugin_info() -> *const mpl_plugin_info_t {
    static INFO: mpl_plugin_info_t = mpl_plugin_info_t {
        abi_version: abi::MPL_ABI_VERSION,
        api_version: abi::MPL_API_VERSION,
        id: PLUGIN_ID.as_ptr() as *const std::ffi::c_char,
        version: PLUGIN_VERSION.as_ptr() as *const std::ffi::c_char,
    };
    &INFO
}

/// # Safety
/// `host` must point to a valid `mpl_host_api_t` for the duration of the call;
/// we copy it by value and never retain the pointer (per api-reference.md).
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_init(host: *const mpl_host_api_t) -> mpl_result_t {
    guard(|| {
        fctrace!("init enter (FC_TRACE={})", trace::enabled());
        if trace::enabled() {
            trace::veh::install();
        }
        if host.is_null() {
            return mpl_result_t::MPL_ERR_INVALID_ARG;
        }
        // By-value copy — the host may free/reuse the table after init.
        abi::store_host(unsafe { *host });

        let ring = RING.get_or_init(|| AudioRing::new(RING_CAP));
        // Fresh session timeline: no stale samples, envelope settled at zero.
        // Safe without synchronization because init runs before the plugin is
        // (re-)registered in the DSP chain — the audio thread cannot be inside
        // process() at this point (same invariant the host relies on: DSP
        // unregister blocks until any in-flight process_all frame finishes).
        ring.clear();
        unsafe { *MIX.env.get() = 0.0 };

        let mut s = session();
        s.phase = Phase::Idle;
        s.last_error.clear();

        // Restore user configuration (defaults from plugin.json on first run).
        s.hotkey_text = config_string("hotkey", DEFAULT_HOTKEY);
        let gain = config_f64("gain", 1.0).clamp(0.0, 4.0) as f32;
        GAIN_BITS.store(gain.to_bits(), Ordering::Relaxed);
        NOTIFY_ON.store(config_bool("notify", true), Ordering::Relaxed);

        // Hotkey delivery: host global-hotkey registration (keyboard combos;
        // validated up front for precise errors). Live re-plan on change.
        plan_hotkey(&mut s);
        fctrace!(
            "init: hotkey=[{}] ok={} err={:?}",
            s.hotkey_text,
            s.hotkey_ok,
            s.last_error
        );
        if s.hotkey_ok {
            abi::log_info(&format!(
                "FocusCapture v{} ready — hotkey [{}] registered (id {})",
                env!("CARGO_PKG_VERSION"),
                s.hotkey_text,
                s.hotkey_id
            ));
        } else {
            abi::log_error(&format!(
                "hotkey unavailable for [{}]: {} — plugin cannot start captures",
                s.hotkey_text, s.last_error
            ));
        }

        abi::set_panel_icon("status", "🎯");
        write_status(&s);
        fctrace!("init leave OK");
        mpl_result_t::MPL_OK
    })
}

#[no_mangle]
pub extern "C" fn micyou_plugin_deinit() {
    let _ = guard(|| {
        fctrace!("deinit enter");
        let mut s = session();
        // Signal the worker and make sure the audio path stops mixing.
        if let Some(f) = &s.stop_flag {
            f.store(true, Ordering::Release);
        }
        CAPTURING.store(false, Ordering::Release);
        // MUST join: the library is about to be unmapped (see REAP_BUDGET_DEINIT).
        reap_worker(&mut s, REAP_BUDGET_DEINIT);
        if s.interval_id != 0 {
            abi::clear_interval(s.interval_id);
            s.interval_id = 0;
        }
        // Reset the audio-thread-visible state (see init(): no concurrent
        // process() is possible once we have been unregistered from the chain).
        if let Some(ring) = RING.get() {
            ring.clear();
        }
        unsafe { *MIX.env.get() = 0.0 };
        *s = Session::new();
        drop(s);
        abi::clear_host();
        trace::veh::uninstall();
        fctrace!("deinit leave");
        mpl_result_t::MPL_OK
    });
}

// ────────────────────────── optional entry points ──────────────────────────

/// Real-time DSP: additive mix of the captured process audio.
///
/// Contract: `data` holds `samples` interleaved f32 values (`channels` per
/// frame); everything here is allocation-free, lock-free and bounded —
/// 2 atomic loads, 1 atomic store, one pass of `frames` FMA-scale ops.
///
/// # Safety
/// `data` must point to `samples` writable f32s, `bypass` to a u32.
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_process(
    data: *mut f32,
    samples: u32,
    channels: u32,
    _queued_ms: f64,
    bypass: *mut u32,
) -> mpl_result_t {
    guard(|| {
        if data.is_null() || bypass.is_null() || channels == 0 || samples == 0 {
            if !bypass.is_null() {
                unsafe { *bypass = 1 };
            }
            return mpl_result_t::MPL_ERR_INVALID_ARG;
        }
        let active = CAPTURING.load(Ordering::Acquire);
        let env_ptr = MIX.env.get();
        // Idle fast path: nothing captured, fade already settled → bypass
        // (host keeps the frame untouched; zero measurable cost).
        if !active && unsafe { *env_ptr } <= 0.0 {
            unsafe { *bypass = 1 };
            return mpl_result_t::MPL_OK;
        }
        let Some(ring) = RING.get() else {
            unsafe { *bypass = 1 };
            return mpl_result_t::MPL_OK;
        };

        let samples = samples as usize;
        let channels = channels as usize;
        let frames = samples / channels;
        let slice = unsafe { std::slice::from_raw_parts_mut(data, frames * channels) };

        // Smooth envelope: 10 ms linear ramp toward the target, advanced once
        // per sample inside the mix callback (frame granularity == sample
        // granularity here because the ring stores mono 48 kHz).
        let mut env = unsafe { *env_ptr };
        let target = if active { 1.0 } else { 0.0 };
        let step = 1.0 / FADE_SAMPLES;
        let gain = f32::from_bits(GAIN_BITS.load(Ordering::Relaxed));

        let take = ring.mix_mono_into(slice, frames, channels, BACKLOG_SAMPLES, |_| {
            if env < target {
                env = (env + step).min(target);
            } else if env > target {
                env = (env - step).max(target);
            }
            gain * env
        });

        // Ring ran dry while fading out (worker already stopped): keep the
        // envelope decaying so it settles to 0 and the bypass fast path
        // re-engages within one fade length.
        if !active && take < frames {
            let remaining = (frames - take) as f32 * step;
            env = (env - remaining).max(0.0);
        }

        unsafe { *env_ptr = env };
        unsafe { *bypass = 0 };
        mpl_result_t::MPL_OK
    })
}

/// Bus messages: hotkey toggles, watchdog ticks, panel actions.
///
/// Runs on a host-dispatched thread — the ONLY context where Host API calls
/// are legal (besides init/deinit). Everything heavy (WASAPI activation) is
/// offloaded to the capture worker; this function returns in microseconds.
///
/// # Safety
/// `topic` is a NUL-terminated string; `payload` spans `payload_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_handle_message(
    _source: *const c_char,
    topic: *const c_char,
    payload: *const u8,
    payload_len: u32,
) -> mpl_result_t {
    guard(|| {
        let topic = unsafe { abi::cstr_or(topic, "") };
        fctrace!("handle_message topic={topic}");
        if let Some(id_str) = topic.strip_prefix("hotkey:") {
            let pressed: u64 = id_str.parse().unwrap_or(0);
            let mut s = session();
            // Only honor the live host registration (stale registrations from
            // earlier hotkey strings are filtered out by handle id).
            if !s.host_hotkey_active || pressed == 0 || pressed != s.hotkey_id {
                return mpl_result_t::MPL_OK;
            }
            toggle(&mut s);
            return mpl_result_t::MPL_OK;
        }
        if topic == "interval:tick" {
            let payload_str = unsafe { payload_string(payload, payload_len) };
            let mut s = session();
            if let Some((id, pl)) = parse_tick(&payload_str) {
                if s.interval_id != 0 && id == s.interval_id && pl == WATCHDOG_PAYLOAD {
                    watchdog(&mut s);
                }
            }
            return mpl_result_t::MPL_OK;
        }
        if topic == "ui:stop" {
            let mut s = session();
            if matches!(s.phase, Phase::Starting | Phase::Running) {
                request_stop(&mut s);
            }
            return mpl_result_t::MPL_OK;
        }
        if topic == "ui:apply" {
            let mut s = session();
            apply_config(&mut s);
            return mpl_result_t::MPL_OK;
        }
        mpl_result_t::MPL_OK
    })
}

/// `interval:tick` payload is `{"interval":<id>,"payload":"<tag>"}`.
fn parse_tick(payload_json: &str) -> Option<(u64, &str)> {
    if payload_json.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(payload_json).ok()?;
    Some((
        v.get("interval")?.as_u64()?,
        v.get("payload")?.as_str()?.to_owned().leak(),
    ))
}

unsafe fn payload_string(payload: *const u8, len: u32) -> String {
    if payload.is_null() || len == 0 {
        return String::new();
    }
    let body = unsafe { std::slice::from_raw_parts(payload, len as usize) };
    String::from_utf8_lossy(body).into_owned()
}

// ─────────────────────────── state machine (bus thread) ────────────────────

fn toggle(s: &mut Session) {
    fctrace!("toggle: phase={:?}", s.phase);
    match s.phase {
        Phase::Idle => start_capture(s),
        Phase::Starting | Phase::Running => request_stop(s),
        Phase::Stopping => {
            // Restart pressed while the previous worker is winding down:
            // reap it first (bounded), then start fresh.
            if reap_worker(s, REAP_BUDGET_HOTKEY) {
                start_capture(s);
            } else {
                abi::log_warn("worker still winding down; press the hotkey again shortly");
            }
        }
    }
}

fn start_capture(s: &mut Session) {
    fctrace!("start_capture enter");
    let Some(pid) = capture::foreground_pid() else {
        abi::log_warn("no foreground window — cannot resolve target process");
        notify_if_enabled("无法确定焦点应用", "没有检测到前台窗口，请把目标应用切到前台后再按快捷键");
        return;
    };
    if pid == capture::current_pid() {
        notify_if_enabled(
            "不能捕获 MicYou 自身",
            "监听/耳返声音会被再次混入麦克风形成回声反馈。请先切换到目标应用再按快捷键",
        );
        return;
    }
    let app = capture::process_name_by_pid(pid);
    let Some(ring) = RING.get() else { return };
    // Reset the timeline before the producer exists (consumer side is idle:
    // CAPTURING is false and the envelope is settled).
    ring.clear();

    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(WorkerShared::new());
    fctrace!("start_capture: pid={pid} app={} spawning worker", s.app);
    match capture::spawn_worker(pid, ring, stop.clone(), shared.clone(), &CAPTURING) {
        Ok(handle) => {
            fctrace!("start_capture: worker spawned OK");
            s.worker = Some(handle);
            s.stop_flag = Some(stop);
            s.shared = Some(shared);
            s.phase = Phase::Starting;
            s.started_at = Some(Instant::now());
            s.app = if app.is_empty() {
                format!("pid:{pid}")
            } else {
                app
            };
            s.pid = pid;
            s.rate = 0;
            s.channels = 0;
            s.pending_report = None;
            s.last_error.clear();
            ensure_watchdog(s);
            write_status(s);
            abi::log_info(&format!(
                "capture starting: {} (pid {pid})",
                s.app
            ));
        }
        Err(e) => {
            let msg = format!("采集线程启动失败: {e}");
            abi::log_error(&msg);
            s.last_error = msg.clone();
            write_status(s);
            notify_if_enabled("捕获启动失败", &msg);
        }
    }
}

fn request_stop(s: &mut Session) {
    fctrace!("request_stop enter (phase={:?})", s.phase);
    if let Some(f) = &s.stop_flag {
        f.store(true, Ordering::Release);
    }
    fctrace!("request_stop: stop flag set");
    // Audio stops mixing immediately (with a 10 ms fade); the worker unwinds
    // asynchronously and the watchdog reaps it.
    CAPTURING.store(false, Ordering::Release);
    s.phase = Phase::Stopping;
    fctrace!("request_stop: CAPTURING=false, phase=Stopping");
    write_status(s);
    abi::log_info(&format!("capture stop requested ({})", s.app));
    fctrace!("request_stop: calling notify (host)");
    notify_if_enabled("FocusCapture", &format!("已停止捕获 {}", s.app));
    fctrace!("request_stop leave");
}

/// Advance the session using worker-reported state. Runs on every watchdog
/// tick; also responsible for one-shot user notifications so that all Host API
/// calls stay on host-dispatched threads.
fn watchdog(s: &mut Session) {
    s.ticks += 1;
    fctrace!("watchdog tick: phase={:?}", s.phase);
    match s.phase {
        Phase::Starting => {
            let Some(shared) = s.shared.clone() else {
                finish(s, None);
                return;
            };
            match shared.phase.load(Ordering::Acquire) {
                PHASE_RUNNING => {
                    s.phase = Phase::Running;
                    s.rate = shared.rate.load(Ordering::Relaxed);
                    s.channels = shared.channels.load(Ordering::Relaxed);
                    CAPTURING.store(true, Ordering::Release);
                    write_status(s);
                    let msg = format!(
                        "{} (pid {}) 的声音已混入麦克风流 · {}Hz/{}ch",
                        s.app, s.pid, s.rate, s.channels
                    );
                    abi::log_info(&format!("capture running: {msg}"));
                    notify_if_enabled("🎯 正在捕获焦点应用", &msg);
                }
                PHASE_FINISHED => {
                    let err = worker_error(&shared);
                    finish(
                        s,
                        Some(("捕获失败".to_string(), err)),
                    );
                }
                _ => {
                    let elapsed = s.started_at.map(|t| t.elapsed()).unwrap_or_default();
                    if elapsed > START_TIMEOUT {
                        if let Some(f) = &s.stop_flag {
                            f.store(true, Ordering::Release);
                        }
                        finish(
                            s,
                            Some((
                                "捕获启动超时".to_string(),
                                "音频引擎在 8 秒内未完成进程环回激活（系统为 Windows 10 2004 以下？音频服务异常？）".to_string(),
                            )),
                        );
                    }
                }
            }
        }
        Phase::Running => {
            let Some(shared) = s.shared.clone() else {
                finish(s, None);
                return;
            };
            if shared.target_exited.load(Ordering::Relaxed) {
                finish(
                    s,
                    Some((
                        "FocusCapture".to_string(),
                        format!("目标应用 {} 已退出，捕获自动停止", s.app),
                    )),
                );
            } else if shared.phase.load(Ordering::Acquire) == PHASE_FINISHED {
                let err = worker_error(&shared);
                finish(s, Some(("捕获中断".to_string(), err)));
            } else if s.ticks.is_multiple_of(5) {
                // Cheap periodic re-read so settings changed outside the panel
                // (JSON editor) also take effect without re-enabling.
                apply_config(s);
            }
        }
        Phase::Stopping => finish(s, None),
        Phase::Idle => {
            if s.interval_id != 0 {
                abi::clear_interval(s.interval_id);
                s.interval_id = 0;
            }
        }
    }
}

/// Try to conclude a session: reap the worker (bounded), disarm the watchdog,
/// surface any pending report, return to Idle. When the worker has not exited
/// yet we stay in `Stopping` and retry on the next tick.
fn finish(s: &mut Session, report: Option<(String, String)>) {
    fctrace!("finish enter: phase={:?} report={}", s.phase, report.is_some());
    if let Some(shared) = &s.shared {
        let disc = shared
            .discontinuities
            .load(std::sync::atomic::Ordering::Relaxed);
        if disc > 0 {
            abi::log_warn(&format!(
                "WASAPI reported {disc} discontinuous buffer(s) during this session (device glitches / CPU starvation)"
            ));
        }
    }
    if report.is_some() {
        // Failures also stop the mixing path immediately.
        CAPTURING.store(false, Ordering::Release);
    }
    if let Some((title, body)) = report {
        s.last_error = body.clone();
        s.pending_report = Some((title, body));
        if let Some(f) = &s.stop_flag {
            f.store(true, Ordering::Release);
        }
        s.phase = Phase::Stopping;
    }
    if !reap_worker(s, Duration::ZERO) {
        write_status(s);
        return; // worker still winding down; next tick retries
    }
    if let Some((title, body)) = s.pending_report.take() {
        abi::log_error(&format!("{title}: {body}"));
        notify_if_enabled(&title, &body);
    }
    write_status(s);
}

/// Join the worker within `budget` (ZERO = only if already finished).
/// Returns true when no worker remains.
fn reap_worker(s: &mut Session, budget: Duration) -> bool {
    let Some(handle) = s.worker.as_ref() else {
        return true;
    };
    fctrace!("reap_worker: is_finished={} budget={}ms", handle.is_finished(), budget.as_millis());
    if !handle.is_finished() {
        if budget.is_zero() {
            return false;
        }
        let deadline = Instant::now() + budget;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(REAP_POLL);
        }
        if !handle.is_finished() {
            return false;
        }
    }
    if let Some(h) = s.worker.take() {
        fctrace!("reap_worker: joining");
        if h.join().is_err() {
            abi::log_error("capture worker panicked (contained by thread boundary)");
        }
        fctrace!("reap_worker: joined");
    }
    s.stop_flag = None;
    s.shared = None;
    s.phase = Phase::Idle;
    CAPTURING.store(false, Ordering::Release);
    if s.interval_id != 0 {
        abi::clear_interval(s.interval_id);
        s.interval_id = 0;
    }
    true
}

fn ensure_watchdog(s: &mut Session) {
    if s.interval_id == 0 {
        s.interval_id = abi::set_interval(WATCHDOG_MS, WATCHDOG_PAYLOAD);
        if s.interval_id == 0 {
            abi::log_warn(
                "host interval unavailable — capture still works, but status/notifications are delayed until the next host-dispatched message",
            );
        }
    }
}

fn worker_error(shared: &WorkerShared) -> String {
    shared
        .error
        .lock()
        .map(|e| if e.is_empty() { "未知错误".to_string() } else { e.clone() })
        .unwrap_or_else(|_| "未知错误".to_string())
}

// ──────────────────────────── hotkey planning ──────────────────────────────

/// (Re-)establish host hotkey delivery for `s.hotkey_text`.
///
/// Keyboard-only by design (see src/hotkey.rs header for why the self-owned
/// mouse-hook engine was removed). Live changes work by registering the NEW
/// combo with the host and filtering messages by handle id — the stale
/// registration cannot be released (no unregister in the ABI) but is inert.
fn plan_hotkey(s: &mut Session) {
    // Revive a still-held host registration for the same combo (A→B→A round
    // trips must not leak duplicate registrations — the ABI cannot release
    // the old one, so reusing it is both cleaner and instant).
    if s.hotkey_id != 0 && s.host_hotkey_sc == s.hotkey_text {
        s.host_hotkey_active = true;
        s.hotkey_ok = true;
        s.last_error.clear();
        return;
    }
    s.host_hotkey_active = false;
    s.hotkey_ok = false;

    let text = s.hotkey_text.clone();
    // Up-front validation gives a precise error (e.g. mouse tokens) instead
    // of the host's generic parse failure.
    if let Err(e) = hotkey::parse(&text) {
        s.last_error = format!("快捷键不可用：{e}");
        return;
    }
    let id = abi::register_hotkey(&text);
    if id != 0 {
        s.hotkey_id = id;
        s.host_hotkey_sc = text;
        s.host_hotkey_active = true;
        s.hotkey_ok = true;
    } else {
        s.last_error =
            "快捷键注册失败：可能与其他程序冲突或格式非法（宿主 register_hotkey 拒绝）".to_string();
    }
}

// ──────────────────────────── config helpers ───────────────────────────────

fn config_string(key: &str, default: &str) -> String {
    let raw = abi::get_config(key);
    serde_json::from_str::<String>(&raw).unwrap_or_else(|_| default.to_string())
}

fn config_f64(key: &str, default: f64) -> f64 {
    let raw = abi::get_config(key);
    serde_json::from_str::<f64>(&raw).unwrap_or(default)
}

fn config_bool(key: &str, default: bool) -> bool {
    let raw = abi::get_config(key);
    serde_json::from_str::<bool>(&raw).unwrap_or(default)
}

/// Re-read mutable settings (gain / notify). Hotkey changes cannot be applied
/// live — the ABI has no unregister_hotkey — so we flag it for the panel.
fn apply_config(s: &mut Session) {
    let gain = config_f64("gain", 1.0).clamp(0.0, 4.0) as f32;
    GAIN_BITS.store(gain.to_bits(), Ordering::Relaxed);
    NOTIFY_ON.store(config_bool("notify", true), Ordering::Relaxed);
    let hk = config_string("hotkey", DEFAULT_HOTKEY);
    if hk != s.hotkey_text {
        s.hotkey_text = hk;
        s.last_error.clear();
        // Live re-plan: no plugin reload needed. A stale host registration
        // (previous keyboard combo) simply stops being honored — its handle
        // id no longer matches, and the ABI cannot unregister it.
        plan_hotkey(s);
        abi::log_info(&format!(
            "hotkey re-planned: [{}] (ok={})",
            s.hotkey_text, s.hotkey_ok
        ));
        write_status(s);
    }
}

fn notify_if_enabled(title: &str, body: &str) {
    if NOTIFY_ON.load(Ordering::Relaxed) {
        abi::notify(title, body);
    }
}

/// Persist the machine-readable status snapshot the panel polls. One
/// `set_config` call per state change (not per tick) keeps host-side I/O low.
fn write_status(s: &Session) {
    let phase = match s.phase {
        Phase::Idle => "idle",
        Phase::Starting => "starting",
        Phase::Running => "capturing",
        Phase::Stopping => "stopping",
    };
    let status = serde_json::json!({
        "phase": phase,
        "app": s.app,
        "pid": s.pid,
        "rate": s.rate,
        "channels": s.channels,
        "error": s.last_error,
        "hotkey": s.hotkey_text,
        "hotkeyOk": s.hotkey_ok,
    });
    abi::set_config("fc_status", &status.to_string());
}

