//! Opt-in field diagnostics: `FC_TRACE=1` makes the plugin print a
//! timestamped, thread-tagged trace of every lifecycle step to **stderr**
//! (visible when running the CLI/TUI frontends, or in GUI console output).
//!
//! Why this exists: two field crashes on the *stop* path could not be
//! localized from host logs alone (the host log granularity is one line per
//! state change, and a faulting thread leaves nothing behind). With FC_TRACE
//! the last printed step before a crash brackets the fault, and the
//! vectored exception handler (§ veh) additionally reports the exception
//! code, faulting address and owning module — enough to distinguish "our
//! DLL" from "AudioSes/mmdevapi/ntdll" without a debugger.
//!
//! Never call `trace!` from the real-time audio thread: it allocates and
//! performs I/O. All call sites are on bus/worker threads.

use std::io::{stderr, Write};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static ENABLED: OnceLock<bool> = OnceLock::new();

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var("FC_TRACE")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

pub fn trace(msg: &str) {
    if !enabled() {
        return;
    }
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!(
        "[fc {wall_ms} tid={:?}] {msg}",
        std::thread::current().id()
    );
    // Sink 1: stderr (CLI/TUI frontends show it directly).
    let mut err = stderr();
    let _ = writeln!(err, "{line}");
    let _ = err.flush();
    // Sink 2: %TEMP%\focuscapture-trace.log — the GUI is a windows-subsystem
    // app with no console attached, and the crash reproduces there; a file
    // sink keeps the last steps and the VEH fault report recoverable.
    if let Some(dir) = std::env::var("TEMP").ok().or_else(|| std::env::var("TMP").ok()) {
        let path = std::path::Path::new(&dir).join("focuscapture-trace.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

#[macro_export]
macro_rules! fctrace {
    ($($arg:tt)*) => {
        if $crate::trace::enabled() {
            $crate::trace::trace(&format!($($arg)*))
        }
    };
}

// ── vectored exception handler: report the fault, then let the process die ──

#[cfg(windows)]
pub mod veh {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, RemoveVectoredExceptionHandler, EXCEPTION_POINTERS,
    };
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW};
    use windows_core::PCWSTR;

    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x0000_0002;
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

    static HANDLE: AtomicIsize = AtomicIsize::new(0);

    unsafe extern "system" fn handler(pointers: *mut EXCEPTION_POINTERS) -> i32 {
        // Best-effort only: no allocation beyond our trace buffer, no locks
        // that might be held by the faulting thread.
        let rec = unsafe { &*(*pointers).ExceptionRecord };
        let code = rec.ExceptionCode.0;
        let addr = rec.ExceptionAddress;

        let mut module = String::from("<unknown>");
        let mut hmod = HMODULE::default();
        let ok = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(addr as *const u16),
                &mut hmod,
            )
            .is_ok()
        };
        if ok {
            let mut buf = [0u16; 260];
            let n = unsafe { GetModuleFileNameW(Some(hmod), &mut buf) };
            let full = String::from_utf16_lossy(&buf[..n as usize]);
            module = full
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or(&full)
                .to_string();
        }
        crate::trace::trace(&format!(
            "!!! UNHANDLED EXCEPTION code={code:#010x} address={addr:?} module={module} — last steps above bracket the fault"
        ));
        EXCEPTION_CONTINUE_SEARCH
    }

    /// Install the reporter (idempotent). Called from init when FC_TRACE is on.
    pub fn install() {
        if HANDLE.load(Ordering::Relaxed) != 0 {
            return;
        }
        let h = unsafe { AddVectoredExceptionHandler(1, Some(handler)) } as isize;
        HANDLE.store(h, Ordering::Relaxed);
        crate::trace::trace(&format!("VEH crash reporter installed (handle {h:#x})"));
    }

    pub fn uninstall() {
        let h = HANDLE.swap(0, Ordering::Relaxed);
        if h != 0 {
            unsafe { RemoveVectoredExceptionHandler(h as *mut core::ffi::c_void) };
        }
    }
}

#[cfg(not(windows))]
pub mod veh {
    pub fn install() {}
    pub fn uninstall() {}
}
