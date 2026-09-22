//! Windows backend: WASAPI **process loopback** (`VAD\\Process_Loopback`,
//! Windows 10 2004 / build 19041+).
//!
//! See docs/TECHNICAL.md §4 for the activation-parameter pitfalls (VT_BLOB
//! PROPVARIANT wrapper, caller-specified format because CMixerClient has no
//! GetMixFormat, mandatory AUDCLNT_STREAMFLAGS_LOOPBACK) and §5 for the
//! teardown ordering rules.

use std::sync::atomic::{AtomicBool, Ordering};
use std::ptr::null_mut;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::capture::{convert_interleaved_to_mono, WorkerShared, PHASE_FINISHED, PHASE_RUNNING};
use crate::resample::SincResampler;
use crate::ring::AudioRing;
use crate::TARGET_RATE;

    use windows::core::{implement, Interface, Ref, HRESULT, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::Media::Audio::{
        ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
        IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
        IAudioCaptureClient, IAudioClient, AUDIOCLIENT_ACTIVATION_PARAMS,
        AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        AUDCLNT_STREAMFLAGS_LOOPBACK, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
    };
    use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    use windows::Win32::System::Variant::{VT_BLOB, VT_EMPTY};
    use windows::Win32::System::Threading::{
        CreateEventW, GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, SetEvent,
        WaitForSingleObject, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };


    /// SYNCHRONIZE (0x00100000) so we can wait on the process handle to
    /// detect target exit; QUERY_LIMITED_INFORMATION for the image name.
    const SYNCHRONIZE: u32 = 0x0010_0000;

    /// RAII guard for the activation PROPVARIANT.
    ///
    /// THE field crash (regressions #1–#3, `0xC0000374 STATUS_HEAP_CORRUPTION`
    /// on the worker thread at session stop): the official sample wraps
    /// `AUDIOCLIENT_ACTIVATION_PARAMS` in a `VT_BLOB` PROPVARIANT whose
    /// `pBlobData` points at a **stack** struct. In C++ that is safe because
    /// PROPVARIANT is a POD without a destructor — but the windows-rs binding
    /// implements `Drop` → `PropVariantClear`, and for `VT_BLOB` that calls
    /// `CoTaskMemFree(pBlobData)`: freeing a stack pointer corrupts the heap.
    /// The fault fired at scope exit (drop order puts `activate_params`
    /// before `Cleanup`), i.e. exactly when the packet loop was left — which
    /// is why every crash happened on the *stop* press and no teardown trace
    /// line ever appeared.
    ///
    /// Fix: neutralize the variant (`vt = VT_EMPTY`, for which
    /// PropVariantClear is a no-op) on EVERY exit path, including early
    /// error returns, via this guard's Drop running before the inner
    /// PROPVARIANT's own Drop.
    struct StackBlobPropVariant(PROPVARIANT);
    impl Drop for StackBlobPropVariant {
        fn drop(&mut self) {
            unsafe {
                let inner = &mut *self.0.Anonymous.Anonymous;
                inner.vt = VT_EMPTY;
            }
        }
    }

    /// Engine-side buffer request. Shared-mode rounds this to engine periods;
    /// a generous 1 s (OBS's win-capture-audio uses 5 s) tolerates consumer
    /// stalls without the engine dropping packets — our ring clamps latency
    /// downstream anyway.
    const BUFFER_DURATION_HNS: i64 = 10_000_000;

    /// Activation completion wait (ms). Process-loopback activation is
    /// serviced by the audio engine and normally completes in < 100 ms.
    const ACTIVATE_TIMEOUT_MS: u32 = 5_000;

    /// Max time we block waiting for a buffer event before re-checking the
    /// stop flag / target liveness (ms).
    const EVENT_WAIT_MS: u32 = 100;

    pub fn foreground_pid() -> Option<u32> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return None;
            }
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            (pid != 0).then_some(pid)
        }
    }

    pub fn current_pid() -> u32 {
        unsafe { GetCurrentProcessId() }
    }

    /// Best-effort executable file name (e.g. "eldenring.exe") for a pid.
    pub fn process_name_by_pid(pid: u32) -> String {
        unsafe {
            let Ok(h) = OpenProcess(
                PROCESS_ACCESS_RIGHTS(PROCESS_QUERY_LIMITED_INFORMATION.0),
                false,
                pid,
            ) else {
                return String::new();
            };
            let name = process_name_of_handle(h);
            let _ = CloseHandle(h);
            name
        }
    }

    unsafe fn process_name_of_handle(h: HANDLE) -> String {
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), PWSTR(buf.as_mut_ptr()), &mut len)
        }
        .ok()
        .map(|_| {
            let full = String::from_utf16_lossy(&buf[..len as usize]);
            full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
        })
        .unwrap_or_default()
    }

    /// IActivateAudioInterfaceCompletionHandler: signals a manual-reset event
    /// when the audio engine finishes activating the process-loopback client.
    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct ActivationHandler {
        completed: HANDLE,
    }

    impl Drop for ActivationHandler {
        fn drop(&mut self) {
            // Runs when the LAST COM reference is released (ours + the audio
            // engine's), i.e. strictly after any ActivateCompleted callback —
            // closing earlier could let a late SetEvent hit a recycled handle.
            unsafe {
                if !self.completed.is_invalid() {
                    let _ = CloseHandle(self.completed);
                }
            }
        }
    }

    impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
        fn ActivateCompleted(
            &self,
            _activateoperation: Ref<IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            // `self` derefs to the ActivationHandler fields.
            unsafe { SetEvent(self.completed)? };
            Ok(())
        }
    }

    // HANDLE is a thread-agnostic kernel reference; SetEvent/WaitForSingleObject
    // are documented thread-safe, so moving the handler (which only carries the
    // event handle) to the MTA callback thread is sound.
    unsafe impl Send for ActivationHandler {}
    unsafe impl Sync for ActivationHandler {}

    /// Owns every raw resource so error paths cannot leak handles or leave
    /// COM uninitialized-paired.
    struct Cleanup {
        proc: HANDLE,
        buffer_event: HANDLE,
        client: Option<IAudioClient>,
        started: bool,
        com_initialized: bool,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            unsafe {
                if self.started {
                    crate::fctrace!("teardown: IAudioClient::Stop()");
                    if let Some(c) = &self.client {
                        let _ = c.Stop();
                    }
                    crate::fctrace!("teardown: Stop() returned");
                }
                // Release the audio client BEFORE anything else: AudioSes
                // (CMixerClient) tears down its engine RPC session in the
                // destructor, which requires this thread's COM apartment to
                // still be initialized. Letting the field drop after the
                // Drop body (i.e. after CoUninitialize) is UB and was the
                // field crash: the host process died within seconds of the
                // stop hotkey. `take()` releases it right here.
                // (The IAudioCaptureClient local in run_capture is declared
                // after `guard`, so scope ordering already drops it first.)
                crate::fctrace!("teardown: releasing IAudioClient");
                self.client.take();
                crate::fctrace!("teardown: IAudioClient released");
                // The engine no longer references the buffer event (the
                // session is gone), so closing it now cannot race a late
                // SetEvent on a recycled handle.
                if !self.buffer_event.is_invalid() {
                    let _ = CloseHandle(self.buffer_event);
                }
                if !self.proc.is_invalid() {
                    let _ = CloseHandle(self.proc);
                }
                crate::fctrace!("teardown: handles closed, CoUninitialize last");
                // COM teardown is strictly LAST.
                if self.com_initialized {
                    CoUninitialize();
                }
                crate::fctrace!("teardown: complete");
            }
        }
    }

    pub fn spawn_worker(
        pid: u32,
        ring: &'static AudioRing,
        stop: Arc<AtomicBool>,
        shared: Arc<WorkerShared>,
        capturing: &'static AtomicBool,
    ) -> std::io::Result<JoinHandle<()>> {
        std::thread::Builder::new()
            .name("fc-process-loopback".into())
            .spawn(move || {
                crate::fctrace!("worker: thread start pid={pid}");
                if let Err(e) = run_capture(pid, ring, &stop, &shared, capturing) {
                    crate::fctrace!("worker: run_capture error: {e}");
                    if let Ok(mut slot) = shared.error.lock() {
                        *slot = e;
                    }
                }
                crate::fctrace!("worker: thread exit (phase->FINISHED)");
                shared.phase.store(PHASE_FINISHED, Ordering::Release);
            })
            .map_err(std::io::Error::other)
    }

    fn run_capture(
        pid: u32,
        ring: &'static AudioRing,
        stop: &AtomicBool,
        shared: &WorkerShared,
        capturing: &'static AtomicBool,
    ) -> Result<(), String> {
        unsafe {
            // Per-thread COM init; our worker is a fresh thread → MTA.
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() {
                return Err(format!("CoInitializeEx failed: {hr:?}"));
            }
            let mut guard = Cleanup {
                proc: HANDLE::default(),
                buffer_event: HANDLE::default(),
                client: None,
                started: false,
                com_initialized: true,
            };

            // Fail fast with an actionable message when we cannot even query
            // the target (typical cause: target elevated, MicYou not).
            guard.proc = OpenProcess(
                PROCESS_ACCESS_RIGHTS(PROCESS_QUERY_LIMITED_INFORMATION.0 | SYNCHRONIZE),
                false,
                pid,
            )
            .map_err(|e| {
                format!(
                    "OpenProcess(pid={pid}) 失败: {e:?}。若目标以管理员权限运行，MicYou 也需以管理员运行"
                )
            })?;
            let app = process_name_of_handle(guard.proc);

            // ── Activate the process-loopback audio client ──
            let activation_event = CreateEventW(None, true, false, PCWSTR::null())
                .map_err(|e| format!("CreateEventW: {e:?}"))?;
            let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler {
                completed: activation_event,
            }
            .into();

            let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: pid,
                        // Include every stream the target renders. (The
                        // INCLUDE/EXCLUDE pair is tree-scoped in the modern
                        // SDK; INCLUDE keeps semantics identical to the old
                        // INCLUDE_TARGET_PROCESS_ONLY for single-process apps
                        // and additionally covers child processes, which is
                        // what users expect from launchers/game clients.)
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };

            // The activation parameters MUST be wrapped in a VT_BLOB
            // PROPVARIANT — exactly like the official ApplicationLoopbackAudio
            // sample. Reinterpreting AUDIOCLIENT_ACTIVATION_PARAMS *as* a
            // PROPVARIANT makes the engine read ActivationType as `vt`
            // (= VT_NULL), TargetProcessId as blob.cbSize and LoopbackMode as
            // blob.pBlobData (NULL) → activation dies with E_INVALIDARG.
            let mut activate_params = StackBlobPropVariant(PROPVARIANT::default());
            {
                let inner = &mut *activate_params.0.Anonymous.Anonymous;
                inner.vt = VT_BLOB;
                let blob = &mut inner.Anonymous.blob;
                blob.cbSize = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
                blob.pBlobData =
                    &mut params as *mut AUDIOCLIENT_ACTIVATION_PARAMS as *mut u8;
            }

            let operation = ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&activate_params.0 as *const PROPVARIANT),
                &handler,
            )
            .map_err(|e| format!("ActivateAudioInterfaceAsync: {e:?}"))?;
            crate::fctrace!("worker: activation params consumed by engine");

            crate::fctrace!("worker: waiting for activation completion (5s cap)");
            match WaitForSingleObject(activation_event, ACTIVATE_TIMEOUT_MS) {
                WAIT_OBJECT_0 => crate::fctrace!("worker: activation completed"),
                WAIT_TIMEOUT => return Err("音频引擎激活超时（5s）".into()),
                other => return Err(format!("等待激活完成失败: {other:?}")),
            }

            let mut activate_hr = HRESULT::default();
            let mut activated: Option<windows::core::IUnknown> = None;
            operation
                .GetActivateResult(&mut activate_hr, &mut activated)
                .map_err(|e| format!("GetActivateResult: {e:?}"))?;
            activate_hr.ok().map_err(|_| {
                let code = activate_hr.0 as u32;
                let hint = match code {
                    // E_ACCESSDENIED
                    0x8007_0005 => "拒绝访问：目标以更高权限运行（试试以管理员身份运行 MicYou），或为受 DRM 保护的内容",
                    // E_INVALIDARG — after the VT_BLOB fix this indicates a host/OS problem, not our params
                    0x8007_0057 => "激活参数被拒绝（系统音频服务异常？）",
                    // ERROR_ELEMENT_NOT_FOUND
                    0x8007_0490 => "音频引擎未找到目标进程的音频端点（稍后重试；若持续失败请确认系统为 Windows 10 2004 / build 19041 及以上）",
                    // AUDCLNT_E_WRONG_ENDPOINT_TYPE
                    0x8889_000b => "端点类型错误（系统不支持进程环回？需 Windows 10 2004+）",
                    _ => "未知原因",
                };
                format!("进程环回激活失败 (HRESULT {code:#010x}, {app}): {hint}")
            })?;
            let client: IAudioClient = activated
                .ok_or("激活结果为空")?
                .cast()
                .map_err(|e| format!("cast IAudioClient: {e:?}"))?;
            guard.client = Some(client);

            // ── Format: CALLER-specified, never GetMixFormat ──
            //
            // Process loopback activates AudioSes!CMixerClient, which does NOT
            // implement GetMixFormat / IsFormatSupported (both return
            // E_NOTIMPL — confirmed by Microsoft, Q&A #1125409). The capture
            // is not tied to any endpoint; the engine resamples & remixes the
            // target process's audio into whatever format we Initialize with.
            //
            // Primary: 48 kHz stereo float32 — same choice as the OBS
            // win-capture-audio plugin; matches the host DSP chain rate, so
            // the resampler stays out of the hot path entirely.
            // Fallback: 44.1 kHz stereo PCM16 — the format Microsoft's
            // ApplicationLoopbackAudio sample hard-codes and calls safe.
            let float48 = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
                nChannels: 2,
                nSamplesPerSec: TARGET_RATE,
                nBlockAlign: 2 * 4,
                nAvgBytesPerSec: TARGET_RATE * 2 * 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let pcm441 = WAVEFORMATEX {
                wFormatTag: 1, // WAVE_FORMAT_PCM
                nChannels: 2,
                nSamplesPerSec: 44_100,
                nBlockAlign: 2 * 2,
                nAvgBytesPerSec: 44_100 * 2 * 2,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            let client_ref = guard.client.as_ref().unwrap();
            let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
            crate::fctrace!("worker: activating client done; initializing format (48k-f32 first)");
            let fmt = if let Ok(()) =
                client_ref.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    stream_flags,
                    BUFFER_DURATION_HNS,
                    0,
                    &float48,
                    None,
                )
            {
                crate::fctrace!("worker: Initialize OK 48k-f32");
                MixFormat { rate: TARGET_RATE, channels: 2, bits: 32, float: true }
            } else if let Ok(()) =
                client_ref.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    stream_flags,
                    BUFFER_DURATION_HNS,
                    0,
                    &pcm441,
                    None,
                )
            {
                {
                    crate::fctrace!("worker: 48k-f32 rejected, fell back to 44.1k-PCM16");
                    MixFormat { rate: 44_100, channels: 2, bits: 16, float: false }
                }
            } else {
                return Err(
                    "IAudioClient::Initialize 对 48kHz-float32 与 44.1kHz-PCM16 均失败（音频引擎异常）".into(),
                );
            };

            guard.buffer_event = CreateEventW(None, false, false, PCWSTR::null())
                .map_err(|e| format!("CreateEventW(buffer): {e:?}"))?;
            guard.client.as_ref().unwrap()
                .SetEventHandle(guard.buffer_event)
                .map_err(|e| format!("SetEventHandle: {e:?}"))?;
            let capture: IAudioCaptureClient = guard.client.as_ref().unwrap()
                .GetService()
                .map_err(|e| format!("GetService(IAudioCaptureClient): {e:?}"))?;

            shared.rate.store(fmt.rate, Ordering::Relaxed);
            shared.channels.store(fmt.channels as u32, Ordering::Relaxed);

            guard.client.as_ref().unwrap()
                .Start()
                .map_err(|e| format!("IAudioClient::Start: {e:?}"))?;
            guard.started = true;
            crate::fctrace!("worker: Start() OK, entering packet loop");
            shared.phase.store(PHASE_RUNNING, Ordering::Release);
            // Fallback for hosts without set_interval (no watchdog to flip
            // CAPTURING): engage mixing directly via the shared double-check
            // (stopper writes flag=true strictly before CAPTURING=false, so
            // a racing start always converges to stopped).
            crate::capture::engage_capturing(stop, capturing);

            // ── Packet loop ──
            let mut mono: Vec<f32> = Vec::with_capacity(4096);
            let mut out48: Vec<f32> = Vec::with_capacity(4096);
            let mut resampler = crate::capture::resampler_for(fmt.rate);

            while !stop.load(Ordering::Acquire) {
                match WaitForSingleObject(guard.buffer_event, EVENT_WAIT_MS) {
                    // (trace on exit paths below)
                    WAIT_OBJECT_0 => {
                        drain_packets(&capture, &fmt, &mut mono, &mut out48, &mut resampler, ring, shared)
                            .map_err(|e| format!("采集循环错误: {e}"))?;
                    }
                    WAIT_TIMEOUT => {}
                    other => return Err(format!("WaitForSingleObject: {other:?}")),
                }
                // Target exited → stop quietly (watchdog notifies the user).
                if WaitForSingleObject(guard.proc, 0) == WAIT_OBJECT_0 {
                    crate::fctrace!("worker: target process exited, leaving loop");
                    shared.target_exited.store(true, Ordering::Relaxed);
                    break;
                }
            }
            crate::fctrace!("worker: leaving packet loop (stop flag={} )", stop.load(Ordering::Relaxed));
            Ok(())
        }
    }

    struct MixFormat {
        rate: u32,
        channels: usize,
        bits: u16,
        float: bool,
    }

    /// Move every pending packet into the ring (converted to 48 kHz mono).
    unsafe fn drain_packets(
        capture: &IAudioCaptureClient,
        fmt: &MixFormat,
        mono: &mut Vec<f32>,
        out48: &mut Vec<f32>,
        resampler: &mut Option<SincResampler>,
        ring: &AudioRing,
        shared: &WorkerShared,
    ) -> Result<(), String> {
        loop {
            let packets = unsafe { capture.GetNextPacketSize() }
                .map_err(|e| format!("GetNextPacketSize: {e:?}"))?;
            if packets == 0 {
                return Ok(());
            }
            let mut ptr: *mut u8 = null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            unsafe { capture.GetBuffer(&mut ptr, &mut frames, &mut flags, None, None) }
                .map_err(|e| format!("GetBuffer: {e:?}"))?;

            if frames > 0 {
                let n = frames as usize;
                mono.clear();
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    // Engine marked the packet silent; payload is undefined.
                    mono.resize(n, 0.0);
                } else if ptr.is_null() {
                    mono.resize(n, 0.0);
                } else {
                    mono.reserve(n);
                    unsafe {
                        convert_interleaved_to_mono(
                            ptr,
                            n,
                            fmt.channels,
                            fmt.bits,
                            fmt.float,
                            mono,
                        );
                    }
                }
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    shared.discontinuities.fetch_add(1, Ordering::Relaxed);
                }
                match resampler {
                    Some(rs) => {
                        rs.process(mono, out48);
                        ring.push(out48.as_slice());
                    }
                    None => {
                        ring.push(mono.as_slice());
                    }
                }
            }
            unsafe { capture.ReleaseBuffer(frames) }.map_err(|e| format!("ReleaseBuffer: {e:?}"))?;
        }
    }
