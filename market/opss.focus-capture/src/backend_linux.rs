//! Linux backend: **PipeWire** stream-to-stream capture.
//!
//! The target application's output stream is a PipeWire node
//! (`media.class = Stream/Output/Audio`, `application.process.id = <pid>`);
//! we create a capture stream and connect it *directly to that node id*
//! (`pw_stream_connect(..., target_id, ...)`), i.e. official PipeWire
//! stream-to-stream linking — no null-sink rerouting, no PulseAudio fallback
//! (documented as unsupported).
//!
//! Foreground window → pid via X11 (`_NET_ACTIVE_WINDOW` + `_NET_WM_PID`).
//! Wayland compositors do not expose the foreground pid, and the host's
//! `register_hotkey` is X11-only as well, so the whole feature is X11-scoped
//! by construction; on Wayland we fail with a precise message.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire::context::Context;
use pipewire::main_loop::MainLoop;
use pipewire::stream::{Stream, StreamFlags};
use pipewire::spa::utils::Direction;

use crate::capture::{
    convert_interleaved_to_mono, engage_capturing, resampler_for, WorkerShared, PHASE_FINISHED,
    PHASE_RUNNING,
};
use crate::resample::SincResampler;
use crate::ring::AudioRing;

/// Negotiated raw-audio format (kind: 0 = F32, 1 = S16, 2 = S32).
#[derive(Clone, Copy)]
struct Fmt {
    rate: u32,
    channels: usize,
    kind: u8,
}

impl Fmt {
    fn bits(&self) -> u16 {
        match self.kind {
            0 => 32,
            1 => 16,
            _ => 32,
        }
    }
    fn frame_bytes(&self) -> usize {
        self.channels * (self.bits() as usize / 8)
    }
}

pub fn current_pid() -> u32 {
    std::process::id()
}

pub fn process_name_by_pid(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Foreground window → owning pid via X11. `None` on Wayland / headless.
pub fn foreground_pid() -> Option<u32> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, ConnectionExt};
    let (conn, _) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.first()?.root;
    let aw = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let pid_atom = conn.intern_atom(false, b"_NET_WM_PID").ok()?.reply().ok()?.atom;
    let window_type: u32 = xproto::AtomEnum::WINDOW.into();
    let resp = conn
        .get_property(false, root, aw, window_type, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    let win = u32::from_le_bytes(resp.value.get(..4)?.try_into().ok()?);
    if win == 0 {
        return None;
    }
    let cardinal_type: u32 = xproto::AtomEnum::CARDINAL.into();
    let resp = conn
        .get_property(false, win, pid_atom, cardinal_type, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    Some(u32::from_le_bytes(resp.value.get(..4)?.try_into().ok()?))
}

pub fn spawn_worker(
    pid: u32,
    ring: &'static AudioRing,
    stop: Arc<AtomicBool>,
    shared: Arc<WorkerShared>,
    capturing: &'static AtomicBool,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("fc-pipewire".into())
        .spawn(move || {
            crate::fctrace!("worker(linux): thread start pid={pid}");
            if let Err(e) = run(pid, ring, &stop, &shared, capturing) {
                crate::fctrace!("worker(linux): error: {e}");
                if let Ok(mut slot) = shared.error.lock() {
                    *slot = e;
                }
            }
            crate::fctrace!("worker(linux): thread exit (phase->FINISHED)");
            shared.phase.store(PHASE_FINISHED, Ordering::Release);
        })
        .map_err(std::io::Error::other)
}

/// Parse a Format pod (SPA_PARAM_Format) by walking the pod bytes directly.
///
/// libspa's `spa_pod_parser_prop()` convenience iterator is a C `static
/// inline` and therefore absent from the bindgen bindings; the object-pod
/// layout (`spa_pod_object { header, type, n_props, props[] }`, each prop =
/// `{ header, key, flags, value }`) is stable ABI, so a small manual walk is
/// both simpler and dependency-free.
unsafe fn parse_format_pod(pod: &pipewire::spa::pod::Pod) -> Option<Fmt> {
    use pipewire::spa::sys as sps;

    let raw = pod.as_raw_ptr() as *const sps::spa_pod;
    let total = unsafe { (*raw).size } as usize + 8;
    let bytes = unsafe { std::slice::from_raw_parts(raw as *const u8, total) };
    let u32_at = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(off..off + 4)?.try_into().ok()?))
    };
    // spa_pod_object: [0..4] size, [4..8] type(SPA_TYPE_Object), [8..12] object
    // type id, [12..16] n_props, props start at 16.
    if u32_at(4)? != sps::SPA_TYPE_Object {
        return None;
    }
    let n_props = u32_at(12)? as usize;
    let mut off = 16usize;
    let mut rate = 0u32;
    let mut channels = 0usize;
    let mut kind = u8::MAX;
    for _ in 0..n_props {
        // spa_pod_prop wire layout: [ value pod header: size(4) type(4) ]
        // [ value payload, padded to 8 ] [ key(4) flags(4) ].
        let value_size = u32_at(off)? as usize;
        let prop_type = u32_at(off + 4)?;
        let padded = (value_size + 7) & !7;
        let key_off = off + 8 + padded;
        let key = u32_at(key_off)?;
        match key {
            sps::SPA_FORMAT_AUDIO_format if prop_type == sps::SPA_TYPE_Id => {
                let id = u32_at(off + 8)?;
                kind = match id {
                    sps::SPA_AUDIO_FORMAT_F32 => 0,
                    sps::SPA_AUDIO_FORMAT_S16 => 1,
                    sps::SPA_AUDIO_FORMAT_S32 => 2,
                    _ => u8::MAX,
                };
            }
            sps::SPA_FORMAT_AUDIO_rate if prop_type == sps::SPA_TYPE_Int => {
                rate = u32_at(off + 8)?;
            }
            sps::SPA_FORMAT_AUDIO_channels if prop_type == sps::SPA_TYPE_Int => {
                channels = u32_at(off + 8)? as usize;
            }
            _ => {}
        }
        off = key_off + 8;
    }
    if rate == 0 || channels == 0 || kind == u8::MAX {
        return None;
    }
    Some(Fmt { rate, channels, kind })
}

/// Build one `SPA_PARAM_Format`-style object pod (byte-wise; same stable
/// layout `parse_format_pod` reads back): [size,type][obj_id,n_props] +
/// props of [value header(8), payload padded to 8, key(4), flags(4)].
fn build_format_pod(format_id: u32, rate: u32, channels: u32) -> Vec<u8> {
    use pipewire::spa::sys as sps;
    let mut props: Vec<u8> = Vec::new();
    let push = |key: u32, vtype: u32, val: u32, out: &mut Vec<u8>| {
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&vtype.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // payload padding to 8
        out.extend_from_slice(&key.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
    };
    push(sps::SPA_FORMAT_mediaType, sps::SPA_TYPE_Id, sps::SPA_MEDIA_TYPE_audio, &mut props);
    push(sps::SPA_FORMAT_mediaSubtype, sps::SPA_TYPE_Id, sps::SPA_MEDIA_SUBTYPE_raw, &mut props);
    push(sps::SPA_FORMAT_AUDIO_format, sps::SPA_TYPE_Id, format_id, &mut props);
    push(sps::SPA_FORMAT_AUDIO_rate, sps::SPA_TYPE_Int, rate, &mut props);
    push(sps::SPA_FORMAT_AUDIO_channels, sps::SPA_TYPE_Int, channels, &mut props);
    let body = 8 + props.len() as u32;
    let mut pod = Vec::with_capacity(8 + body as usize);
    pod.extend_from_slice(&body.to_le_bytes());
    pod.extend_from_slice(&sps::SPA_TYPE_Object.to_le_bytes());
    pod.extend_from_slice(&sps::SPA_TYPE_OBJECT_Format.to_le_bytes());
    pod.extend_from_slice(&5u32.to_le_bytes());
    pod.extend_from_slice(&props);
    pod
}

/// Registry record for a candidate Stream/Output/Audio node:
/// (id, type, perms, version, client.id, application.process.id, binary, name)
type NodeRec = (
    u32,
    pipewire::types::ObjectType,
    pipewire::permissions::PermissionFlags,
    u32,
    Option<u32>,
    Option<u32>,
    String,
    String,
);

fn run(
    pid: u32,
    ring: &'static AudioRing,
    stop: &AtomicBool,
    shared: &WorkerShared,
    capturing: &'static AtomicBool,
) -> Result<(), String> {
    pipewire::init();
    let main_loop = MainLoop::new(None).map_err(|e| format!("PipeWire MainLoop: {e}"))?;
    let context = Context::new(&main_loop).map_err(|e| format!("PipeWire Context: {e}"))?;
    let core = context
        .connect(None)
        .map_err(|e| format!("PipeWire 连接失败（是否在 PipeWire 会话中？）: {e}"))?;

    // ── Enumerate the target's output stream nodes ──
    //
    // Pid attribution, most trustworthy first:
    //  1. node.client.id → Client global → `pipewire.sec.pid`
    //     (daemon-stamped from SO_PEERCRED; cannot be spoofed by the client
    //     and present in every configuration),
    //  2. node `application.process.id` (libpipewire default on desktop
    //     sessions; absent in minimal/headless configs),
    //  3. node `application.process.binary` / `application.name` == comm.
    let clients: Arc<Mutex<std::collections::HashMap<u32, u32>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let nodes: Arc<Mutex<Vec<NodeRec>>> = Arc::new(Mutex::new(Vec::new()));
    let registry = core
        .get_registry()
        .map_err(|e| format!("PipeWire registry: {e}"))?;
    let clients_cb = clients.clone();
    let nodes_cb = nodes.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            if let Some(sec_pid) = props.get("pipewire.sec.pid") {
                if let Ok(p) = sec_pid.parse::<u32>() {
                    clients_cb.lock().unwrap().insert(global.id, p);
                }
                return;
            }
            if props.get("media.class") != Some("Stream/Output/Audio") {
                return;
            }
            nodes_cb.lock().unwrap().push((
                global.id,
                global.type_.clone(),
                global.permissions,
                global.version,
                props.get("client.id").and_then(|v| v.parse().ok()),
                props
                    .get("application.process.id")
                    .and_then(|v| v.parse().ok()),
                props
                    .get("application.process.binary")
                    .unwrap_or("")
                    .to_string(),
                props.get("application.name").unwrap_or("").to_string(),
            ));
        })
        .register();

    let comm = process_name_by_pid(pid);
    let deadline = Instant::now() + Duration::from_secs(2);
    let pick = |clients: &std::collections::HashMap<u32, u32>, nodes: &Vec<NodeRec>| -> Option<
        (
            u32,
            String,
            pipewire::types::ObjectType,
            pipewire::permissions::PermissionFlags,
            u32,
        ),
    > {
        for (id, ty, perms, ver, client_id, pid_prop, binary, name) in nodes {
            let by_sec = client_id.and_then(|c| clients.get(&c).copied()) == Some(pid);
            let by_pid = *pid_prop == Some(pid);
            let by_name =
                !comm.is_empty() && (*binary == comm || *name == comm);
            if by_sec || by_pid || by_name {
                return Some((*id, name.clone(), ty.clone(), *perms, *ver));
            }
        }
        None
    };
    let mut target = None;
    while Instant::now() < deadline {
        main_loop.loop_().iterate(Duration::from_millis(50));
        target = pick(&clients.lock().unwrap(), &nodes.lock().unwrap());
        if target.is_some() {
            break;
        }
    }
    crate::fctrace!(
        "worker(linux): enumeration: clients={:?} nodes={:?}",
        *clients.lock().unwrap(),
        *nodes.lock().unwrap()
    );
    let Some((node_id, node_name, node_type, node_perms, node_ver)) = target else {
        return Err(format!(
            "PipeWire 未找到 pid {pid} 的活动音频输出流（应用需正在发声；Wayland 会话不支持前台识别）"
        ));
    };
    crate::fctrace!("worker(linux): target node {node_id} ({node_name})");

    // ── Capture stream connected directly to the target node ──
    let mut props = pipewire::properties::Properties::new();
    props.insert("media.type", "Audio");
    props.insert("media.category", "Capture");
    props.insert("media.role", "Communication");
    props.insert("node.name", "FocusCapture");
    props.insert("application.name", "MicYou FocusCapture");
    let stream = Stream::new(&core, "FocusCapture", props)
        .map_err(|e| format!("PipeWire Stream: {e}"))?;

    let rate_arc = Arc::new(AtomicU32::new(0));
    let ch_arc = Arc::new(AtomicU32::new(0));
    let quit = Arc::new(AtomicBool::new(false));

    let listener_state = ListenerState {
        stop_probe: StopProbe { stop_ptr: stop as *const AtomicBool },
        quit: quit.clone(),
        ring,
        rate_arc: rate_arc.clone(),
        ch_arc: ch_arc.clone(),
        fmt: None,
        resampler: None,
        mono: Vec::with_capacity(4096),
        out48: Vec::with_capacity(4096),
    };

    let listener = stream
        .add_local_listener_with_user_data(listener_state)
        .state_changed(|_stream, _state, old, new| {
            crate::fctrace!("worker(linux): stream state {old:?} -> {new:?}");
        })
        .param_changed(|_stream, state, id, pod| {
            crate::fctrace!("worker(linux): param_changed id={id} pod={}", pod.is_some());
            if id != pipewire::spa::sys::SPA_PARAM_Format {
                return;
            }
            let Some(pod) = pod else { return };
            let Some(fmt) = (unsafe { parse_format_pod(pod) }) else {
                crate::fctrace!("worker(linux): format pod parse FAILED");
                return;
            };
            crate::fctrace!(
                "worker(linux): negotiated {}Hz {}ch kind={}",
                fmt.rate,
                fmt.channels,
                fmt.kind
            );
            state.resampler = resampler_for(fmt.rate);
            state.rate_arc.store(fmt.rate, Ordering::Relaxed);
            state.ch_arc.store(fmt.channels as u32, Ordering::Relaxed);
            state.fmt = Some(fmt);
        })
        .process(|stream_ref, state| {
            if state.stop_probe.stopped() {
                state.quit.store(true, Ordering::Release);
                return;
            }
            let Some(mut buffer) = stream_ref.dequeue_buffer() else {
                return;
            };
            let Some(fmt) = state.fmt else { return };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let size = data.chunk().size() as usize;
            let offset = data.chunk().offset() as usize;
            let Some(bytes) = data.data() else { return };
            let start = offset.min(bytes.len());
            let len = size.min(bytes.len() - start);
            let frames = len / fmt.frame_bytes().max(1);
            if frames == 0 {
                return;
            }
            state.mono.clear();
            state.mono.reserve(frames);
            unsafe {
                convert_interleaved_to_mono(
                    bytes[start..].as_ptr(),
                    frames,
                    fmt.channels,
                    fmt.bits(),
                    fmt.kind == 0,
                    &mut state.mono,
                );
            }
            let mono = &mut state.mono;
            let out = &mut state.out48;
            match &mut state.resampler {
                Some(res) => {
                    res.process(mono, out);
                    state.ring.push(out);
                }
                None => {
                    state.ring.push(mono);
                }
            }
        })
        .register()
        .map_err(|e| format!("PipeWire stream listener: {e}"))?;

    // Stream-to-stream links need our side to offer the link format. Adopt
    // the target's own EnumFormat verbatim (same approach the daemon uses
    // for device capture); fall back to a broad candidate list if the node
    // does not answer enumeration.
    use pipewire::spa::sys as sps;
    let adopted: Option<Vec<u8>> = {
        let gobj = pipewire::registry::GlobalObject::<&pipewire::spa::utils::dict::DictRef> {
            id: node_id,
            permissions: node_perms,
            type_: node_type,
            version: node_ver,
            props: None,
        };
        match registry.bind::<pipewire::node::Node, &pipewire::spa::utils::dict::DictRef>(&gobj) {
            Ok(node) => {
                let slot = Arc::new(Mutex::new(None::<Vec<u8>>));
                let slot_cb = slot.clone();
                let node_listener = node
                    .add_listener_local()
                    .param(move |_seq, ptype, _idx, _next, pod| {
                        if ptype == pipewire::spa::param::ParamType::EnumFormat {
                            if let Some(pod) = pod {
                                let mut g = slot_cb.lock().unwrap();
                                if g.is_none() {
                                    *g = Some(pod.as_bytes().to_vec());
                                }
                            }
                        }
                    })
                    .register();
                node.enum_params(0, Some(pipewire::spa::param::ParamType::EnumFormat), 0, u32::MAX);
                let dl = Instant::now() + Duration::from_secs(1);
                while slot.lock().unwrap().is_none() && Instant::now() < dl {
                    main_loop.loop_().iterate(Duration::from_millis(25));
                }
                let got = slot.lock().unwrap().clone();
                crate::fctrace!("worker(linux): adopted EnumFormat = {}", got.is_some());
                drop(node_listener);
                got
            }
            Err(e) => {
                crate::fctrace!("worker(linux): node bind failed: {e}");
                None
            }
        }
    };

    let mut pod_bytes: Vec<Vec<u8>> = match &adopted {
        Some(b) => vec![b.clone()],
        None => Vec::new(),
    };
    if pod_bytes.is_empty() {
        for &fmt_id in &[sps::SPA_AUDIO_FORMAT_F32, sps::SPA_AUDIO_FORMAT_S32, sps::SPA_AUDIO_FORMAT_S16] {
            for &rate in &[48000u32, 44100, 96000, 192000] {
                for &ch in &[2u32, 1] {
                    pod_bytes.push(build_format_pod(fmt_id, rate, ch));
                }
            }
        }
    }
    let pods: Vec<&pipewire::spa::pod::Pod> = pod_bytes
        .iter()
        .filter_map(|b| pipewire::spa::pod::Pod::from_bytes(b))
        .collect();
    let mut pod_refs: Vec<&pipewire::spa::pod::Pod> = pods;
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            pod_refs.as_mut_slice(),
        )
        .map_err(|e| format!("PipeWire 连接目标节点失败: {e}"))?;

    // ── Drive the loop until stopped ──
    let mut announced = false;
    while !stop.load(Ordering::Acquire) && !quit.load(Ordering::Relaxed) {
        main_loop.loop_().iterate(Duration::from_millis(100));
        if !announced && rate_arc.load(Ordering::Relaxed) != 0 {
            shared
                .rate
                .store(rate_arc.load(Ordering::Relaxed), Ordering::Relaxed);
            shared
                .channels
                .store(ch_arc.load(Ordering::Relaxed), Ordering::Relaxed);
            shared.phase.store(PHASE_RUNNING, Ordering::Release);
            engage_capturing(stop, capturing);
            crate::fctrace!("worker(linux): running");
            announced = true;
        }
    }

    crate::fctrace!("worker(linux): leaving loop");
    let _ = stream.disconnect();
    drop(listener);
    drop(stream);
    drop(core); // disconnects the Core proxy
    Ok(())
}

/// The worker's stop flag lives in the caller's stack frame; the listener
/// user-data holds a raw pointer to it (the join in `reap_worker` guarantees
/// the flag outlives the thread).
struct StopProbe {
    stop_ptr: *const AtomicBool,
}
// SAFETY: used only on the PipeWire loop thread; the AtomicBool is owned by
// the bus thread and lives until the worker is joined.
unsafe impl Send for StopProbe {}

impl StopProbe {
    fn stopped(&self) -> bool {
        unsafe { &*self.stop_ptr }.load(Ordering::Acquire)
    }
}

struct ListenerState {
    stop_probe: StopProbe,
    quit: Arc<AtomicBool>,
    ring: &'static AudioRing,
    rate_arc: Arc<AtomicU32>,
    ch_arc: Arc<AtomicU32>,
    fmt: Option<Fmt>,
    resampler: Option<SincResampler>,
    mono: Vec<f32>,
    out48: Vec<f32>,
}
