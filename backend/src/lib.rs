//! Camera data plane. The HTTP host passes a socket descriptor, never pixels.
//! ACP2 preserves the producer's QCFT association; it does not certify LED sync.
use serde_json::json;
use std::{
    collections::VecDeque,
    ffi::CStr,
    io, mem,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::UnixDatagram,
        },
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

#[cfg(feature = "webrtc")]
mod video;

const STREAMS: usize = 18;
const MAX_PACKET: usize = 104 + 128 * 1024;
const MAX_AGE: u64 = 500_000_000;
const UNKNOWN: u32 = u32::MAX;
fn mono_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) } != 0 {
        return 0;
    }
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
fn u32le(p: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(p[n..n + 4].try_into().unwrap())
}
fn u64le(p: &[u8], n: usize) -> u64 {
    u64::from_le_bytes(p[n..n + 8].try_into().unwrap())
}

struct Frame {
    packet: Arc<[u8]>,
    session: u64,
    sequence: u64,
    timestamp: u64,
    width: u32,
    height: u32,
    revision: u64,
    phase: u32,
    flags: u32,
    exposure_ns: u64,
    sensor_sequence: u64,
    sof_ns: u64,
    stream_id: u64,
    anchor: u64,
    exposure_status: i32,
    timing_flags: u32,
}
fn parse(p: &[u8], now: u64) -> Result<(usize, Frame), &'static str> {
    if p.len() < 48 {
        return Err("short_header");
    }
    let header = match &p[..4] {
        b"ACP1" => 48,
        b"ACP2" => 104,
        _ => return Err("packet_version"),
    };
    if p.len() < header {
        return Err("short_header");
    }
    let slot = u32le(p, 4) as usize;
    let (session, sequence, timestamp) = (u64le(p, 8), u64le(p, 16), u64le(p, 24));
    let (width, height, channels, length) =
        (u32le(p, 32), u32le(p, 36), u32le(p, 40), u32le(p, 44));
    if slot >= 6
        || session == 0
        || width == 0
        || width > 640
        || height == 0
        || height > 480
        || channels != if slot < 4 { 1 } else { 3 }
        || length != width * height * channels
        || p.len() != header + length as usize
        || p.len() > MAX_PACKET
        || timestamp > now
        || now - timestamp >= MAX_AGE
    {
        return Err("invalid_or_stale_frame");
    }
    let mut f = Frame {
        packet: Arc::from(p),
        session,
        sequence,
        timestamp,
        width,
        height,
        revision: 0,
        phase: UNKNOWN,
        flags: 0,
        exposure_ns: 0,
        sensor_sequence: 0,
        sof_ns: 0,
        stream_id: 0,
        anchor: 0,
        exposure_status: 0,
        timing_flags: 0,
    };
    if header == 104 {
        f.phase = u32le(p, 48);
        f.flags = u32le(p, 52);
        f.exposure_ns = u64le(p, 56);
        f.sensor_sequence = u64le(p, 64);
        f.sof_ns = u64le(p, 72);
        f.stream_id = u64le(p, 80);
        f.anchor = u64le(p, 88);
        f.exposure_status = u32le(p, 96) as i32;
        f.timing_flags = u32le(p, 100);
        if f.flags & !3 != 0 || (f.phase > 2 && f.phase != UNKNOWN) {
            return Err("invalid_phase_metadata");
        }
        if f.flags & 2 != 0
            && (slot >= 4
                || f.flags & 1 == 0
                || f.phase > 2
                || f.exposure_status != 0
                || f.exposure_ns == 0
                || f.exposure_ns > 1_000_000_000
                || f.sof_ns == 0
                || f.sof_ns > timestamp
                || f.stream_id == 0
                || f.timing_flags & 31 != 31
                || f.anchor == 0)
        {
            return Err("unverified_phase");
        }
    }
    let id = if slot >= 4 {
        slot + 12
    } else if f.flags & 2 != 0 {
        f.phase as usize * 4 + slot
    } else {
        12 + slot
    };
    Ok((id, f))
}

struct Inner {
    frames: [Option<Frame>; STREAMS],
    times: [VecDeque<u64>; STREAMS],
    // Preserve ordering across phase queues and flush all phases on producer restart.
    sources: [Option<(u64, u64, u64)>; 6],
    revision: u64,
    rejected: u64,
    received: u64,
    bytes_received: u64,
    sent: u64,
    bytes_sent: u64,
    send_errors: u64,
    last_request: Option<Instant>,
    error: Option<String>,
}
impl Inner {
    fn clear(&mut self) {
        self.frames = std::array::from_fn(|_| None);
        for times in &mut self.times {
            times.clear();
        }
        self.sources.fill(None);
    }
    fn accept(&mut self, p: &[u8]) -> Result<(), &'static str> {
        let (id, mut frame) = parse(p, mono_ns())?;
        let slot = u32le(p, 4) as usize;
        if let Some((session, sequence, timestamp)) = self.sources[slot] {
            if frame.timestamp <= timestamp
                || (session == frame.session && sequence == frame.sequence)
            {
                return Err("duplicate_or_reordered_frame");
            }
            if session != frame.session {
                for i in 0..STREAMS {
                    if stream_slot(i) == slot {
                        self.frames[i] = None;
                        self.times[i].clear();
                    }
                }
            }
        }
        self.sources[slot] = Some((frame.session, frame.sequence, frame.timestamp));
        self.revision += 1;
        frame.revision = self.revision;
        let times = &mut self.times[id];
        if times.len() == 30 {
            times.pop_front();
        }
        times.push_back(frame.timestamp);
        self.received += 1;
        self.bytes_received += p.len() as u64;
        self.frames[id] = Some(frame);
        self.error = None;
        Ok(())
    }
}
pub struct Camera {
    path: PathBuf,
    live: bool,
    stop: AtomicBool,
    gate: Mutex<(Instant, bool)>,
    inner: Mutex<Inner>,
    changed: Condvar,
    #[cfg(feature = "webrtc")]
    video: Mutex<video::Video>,
}
fn stream_slot(id: usize) -> usize {
    if id >= 16 {
        id - 12
    } else {
        id % 4
    }
}
fn fresh(f: &Frame, now: u64) -> bool {
    now >= f.timestamp && now - f.timestamp < MAX_AGE
}

// All calls are made by cameras.py. The handle stays alive until process exit;
// request threads can safely finish while the receiver shuts down.
#[no_mangle]
pub unsafe extern "C" fn mix_camera_create(path: *const libc::c_char, live: bool) -> *mut Camera {
    if path.is_null() {
        return std::ptr::null_mut();
    }
    let Ok(path) = CStr::from_ptr(path).to_str() else {
        return std::ptr::null_mut();
    };
    if !path.starts_with('/') || path.len() >= 108 {
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(Camera {
        path: PathBuf::from(path),
        live,
        stop: AtomicBool::new(false),
        gate: Mutex::new((Instant::now() - Duration::from_secs(1), true)),
        changed: Condvar::new(),
        #[cfg(feature = "webrtc")]
        video: Mutex::new(video::Video::default()),
        inner: Mutex::new(Inner {
            frames: std::array::from_fn(|_| None),
            times: std::array::from_fn(|_| VecDeque::with_capacity(30)),
            sources: [None; 6],
            revision: 0,
            rejected: 0,
            received: 0,
            bytes_received: 0,
            sent: 0,
            bytes_sent: 0,
            send_errors: 0,
            last_request: None,
            error: None,
        }),
    }))
}
#[no_mangle]
pub unsafe extern "C" fn mix_camera_stop(c: *const Camera) {
    let c = &*c;
    c.stop.store(true, Ordering::Relaxed);
    c.changed.notify_all();
}

fn open_socket(c: &Camera) -> io::Result<UnixDatagram> {
    let parent = c
        .path
        .parent()
        .ok_or_else(|| io::Error::other("socket_parent"))?;
    if !parent.exists() {
        std::fs::create_dir(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let meta = parent.symlink_metadata()?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        return Err(io::Error::other("unsafe_camera_socket_directory"));
    }
    let sock = UnixDatagram::unbound()?;
    let one: libc::c_int = 1;
    let size: libc::c_int = 2 * 1024 * 1024;
    unsafe {
        if libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            &one as *const _ as _,
            mem::size_of_val(&one) as _,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &size as *const _ as _,
            mem::size_of_val(&size) as _,
        );
    }
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    // Bind after PASSCRED is set; never replace another observer's socket.
    let path = c.path.as_os_str().as_encoded_bytes();
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    for (dest, source) in addr.sun_path.iter_mut().zip(path) {
        *dest = *source as _;
    }
    if unsafe {
        libc::bind(
            sock.as_raw_fd(),
            &addr as *const _ as _,
            mem::size_of_val(&addr) as _,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if let Err(e) = std::fs::set_permissions(&c.path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(&c.path);
        return Err(e);
    }
    Ok(sock)
}
fn receive(sock: &UnixDatagram, p: &mut [u8]) -> io::Result<usize> {
    unsafe {
        let mut iov = libc::iovec {
            iov_base: p.as_mut_ptr() as _,
            iov_len: p.len(),
        };
        // Word-aligned ancillary storage for SCM_CREDENTIALS.
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as _;
        msg.msg_controllen = mem::size_of_val(&control);
        let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(io::Error::other("truncated_packet"));
        }
        let mut h = libc::CMSG_FIRSTHDR(&msg);
        while !h.is_null() {
            if (*h).cmsg_level == libc::SOL_SOCKET
                && (*h).cmsg_type == libc::SCM_CREDENTIALS
                && (*h).cmsg_len >= libc::CMSG_LEN(mem::size_of::<libc::ucred>() as _) as usize
            {
                let cred = std::ptr::read_unaligned(libc::CMSG_DATA(h) as *const libc::ucred);
                if cred.uid == libc::geteuid() {
                    return Ok(n as usize);
                }
            }
            h = libc::CMSG_NXTHDR(&msg, h);
        }
        Err(io::Error::other("invalid_producer"))
    }
}
#[no_mangle]
pub unsafe extern "C" fn mix_camera_run(c: *const Camera) {
    let c = &*c;
    if !c.live {
        return;
    }
    let mut sock = None;
    let mut packet = vec![0u8; MAX_PACKET];
    while !c.stop.load(Ordering::Relaxed) {
        let active = camera_active(c);
        if !active {
            if sock.take().is_some() {
                let _ = std::fs::remove_file(&c.path);
                c.inner.lock().unwrap().clear();
            }
            let guard = c.inner.lock().unwrap();
            drop(
                c.changed
                    .wait_timeout(guard, Duration::from_millis(100))
                    .unwrap(),
            );
            continue;
        }
        if sock.is_none() {
            match open_socket(c) {
                Ok(s) => sock = Some(s),
                Err(e) => {
                    c.inner.lock().unwrap().error = Some(e.to_string());
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }
        match receive(sock.as_ref().unwrap(), &mut packet) {
            Ok(n) => {
                let mut state = c.inner.lock().unwrap();
                if let Err(e) = state.accept(&packet[..n]) {
                    state.rejected += 1;
                    state.error = Some(e.into());
                } else {
                    c.changed.notify_all();
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                ()
            }
            Err(e) => {
                let mut s = c.inner.lock().unwrap();
                s.rejected += 1;
                s.error = Some(e.to_string());
            }
        }
    }
    if sock.take().is_some() {
        let _ = std::fs::remove_file(&c.path);
    }
    c.inner.lock().unwrap().clear();
    c.changed.notify_all();
}

fn send_all(fd: i32, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let n = unsafe {
            libc::send(
                fd,
                bytes.as_ptr() as _,
                bytes.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if n > 0 {
            bytes = &bytes[n as usize..];
            continue;
        }
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if e.kind() != io::ErrorKind::WouldBlock {
            return Err(e);
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let left = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(2000) as i32;
        if unsafe { libc::poll(&mut poll, 1, left) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
fn response(
    fd: i32,
    status: &str,
    kind: &str,
    extra: &str,
    bodies: &[Arc<[u8]>],
) -> io::Result<()> {
    let length: usize = bodies.iter().map(|b| b.len()).sum();
    let headers = format!("HTTP/1.0 {status}\r\nContent-Type: {kind}\r\nContent-Length: {length}\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\n{extra}\r\n");
    let deadline = Instant::now() + Duration::from_secs(2);
    send_all(fd, headers.as_bytes(), deadline)?;
    for b in bodies {
        send_all(fd, b, deadline)?;
    }
    Ok(())
}
#[no_mangle]
pub unsafe extern "C" fn mix_camera_frames(
    c: *const Camera,
    fd: i32,
    after: u64,
    mask: u32,
) -> i32 {
    let c = &*c;
    let started = mono_ns();
    if !c.live {
        return -1;
    }
    let mut s = c.inner.lock().unwrap();
    s.last_request = Some(Instant::now());
    c.changed.notify_all();
    // A restarted backend must return its new revision immediately.
    if after == s.revision && !c.stop.load(Ordering::Relaxed) {
        s = c
            .changed
            .wait_timeout_while(s, Duration::from_millis(800), |s| {
                s.revision == after && !c.stop.load(Ordering::Relaxed)
            })
            .unwrap()
            .0;
    }
    let now = mono_ns();
    let bodies: Vec<_> = s
        .frames
        .iter()
        .enumerate()
        .filter_map(|(id, f)| {
            f.as_ref()
                .filter(|f| {
                    mask & (1 << id) != 0
                        && (f.revision > after || after > s.revision)
                        && fresh(f, now)
                })
                .map(|f| f.packet.clone())
        })
        .collect();
    let revision = s.revision;
    drop(s);
    let result = response(
        fd,
        if bodies.is_empty() {
            "204 No Content"
        } else {
            "200 OK"
        },
        "application/octet-stream",
        &format!("X-Camera-Revision: {revision}\r\nX-Camera-Monotonic-Ns: {now}\r\nX-Camera-Wait-Ns: {}\r\n", now.saturating_sub(started)),
        &bodies,
    );
    let mut s = c.inner.lock().unwrap();
    if result.is_ok() {
        s.sent += bodies.len() as u64;
        s.bytes_sent += bodies.iter().map(|b| b.len() as u64).sum::<u64>();
        0
    } else {
        s.send_errors += 1;
        -1
    }
}
#[no_mangle]
pub unsafe extern "C" fn mix_camera_status(c: *const Camera, fd: i32) -> i32 {
    let c = &*c;
    let now = mono_ns();
    let s = c.inner.lock().unwrap();
    let rows: Vec<_> = (0..STREAMS).map(|id| {
        let slot = stream_slot(id); let f = s.frames[id].as_ref(); let live = f.is_some_and(|f| fresh(f, now));
        let times = &s.times[id];
        let fps = if live && times.len() > 1 { (times.len()-1) as f64 * 1e9 / (times.back().unwrap()-times.front().unwrap()) as f64 } else { 0.0 };
        json!({"id":id,"slot":slot,"phase":if id<12 {Some(id/4)} else {None},
            "role":if id<8 {"controller"} else if id<12 {"slam"} else if id<16 {"unclassified"} else {"passthrough"},
            "state":if live {"live"} else if f.is_some() {"stale"} else {"waiting"},
            "age_ms":f.map(|f| (now as i128-f.timestamp as i128) as f64/1e6),"preview_fps":fps,
            "sequence":f.map(|f|f.sequence.to_string()),"session":f.map(|f|f.session.to_string()),
            "width":f.map(|f|f.width),"height":f.map(|f|f.height),
            "phase_valid":f.is_some_and(|f|f.flags&2!=0),"reported_phase":f.filter(|f|f.phase!=UNKNOWN).map(|f|f.phase),
            "exposure_ns":f.filter(|f|f.exposure_ns>0).map(|f|f.exposure_ns.to_string()),
            "sensor_sequence":f.filter(|f|f.flags&1!=0).map(|f|f.sensor_sequence.to_string()),
            "sof_monotonic_ns":f.filter(|f|f.flags&1!=0).map(|f|f.sof_ns.to_string()),
            "qcft_stream_id":f.filter(|f|f.flags&1!=0).map(|f|f.stream_id.to_string()),
            "phase_anchor_qtimer":f.filter(|f|f.anchor>0).map(|f|f.anchor.to_string()),
            "exposure_status":f.filter(|f|f.packet.starts_with(b"ACP2")).map(|f|f.exposure_status),
            "timing_flags":f.map(|f|f.timing_flags)})
    }).collect();
    // Keep the physical-camera summary for existing /api/cameras clients.
    let cameras: Vec<_> = (0..6)
        .map(|slot| {
            let id = (0..STREAMS)
                .filter(|&id| stream_slot(id) == slot)
                .max_by_key(|&id| s.frames[id].as_ref().map(|f| f.timestamp).unwrap_or(0))
                .unwrap();
            let mut row = rows[id].clone();
            row["sensor"] = json!(if slot < 4 { "OV7251" } else { "IMX471" });
            row["csiphy"] = json!([5, 4, 2, 3, 1, 0][slot]);
            row["label"] = json!(format!("Camera {slot}"));
            row
        })
        .collect();
    let data = json!({"enabled":c.live,"cameras":cameras,"streams":rows,"error":s.error,"revision":s.revision,
        "rejected_packets":s.rejected,
        "backend":"rust","transport":"sampled-raw","max_age_ms":500,"max_preview_fps_per_stream":30,
        "physical_cameras":6,"simultaneous_six_verified":false,"csid_count":4,"optical_led_phase_verified":false,
        "counters":{"received_frames":s.received,"received_bytes":s.bytes_received,"rejected_packets":s.rejected,
            "sent_frames":s.sent,"sent_bytes":s.bytes_sent,"send_errors":s.send_errors}}).to_string();
    drop(s);
    if response(
        fd,
        "200 OK",
        "application/json",
        "",
        &[Arc::from(data.into_bytes())],
    )
    .is_ok()
    {
        0
    } else {
        -1
    }
}

// Control plane only. Session data and camera pixels stay in native code.
#[no_mangle]
pub unsafe extern "C" fn mix_camera_video_run(c: *const Camera) {
    #[cfg(feature = "webrtc")]
    {
        let c = &*c;
        if !c.live {
            return;
        }
        loop {
            let active = {
                let mut video = c.video.lock().unwrap();
                video.tick(c);
                video.active()
            };
            if c.stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(if active { 5 } else { 100 }));
        }
    }
    #[cfg(not(feature = "webrtc"))]
    let _ = c;
}
#[no_mangle]
pub unsafe extern "C" fn mix_camera_video(
    c: *const Camera,
    fd: i32,
    method: *const libc::c_char,
    path: *const libc::c_char,
    body: *const libc::c_char,
    lease: i32,
    generation: u64,
) -> i32 {
    let c = &*c;
    let result: Result<serde_json::Value, String> = (|| {
        let method = CStr::from_ptr(method).to_str().map_err(|e| e.to_string())?;
        let path = CStr::from_ptr(path).to_str().map_err(|e| e.to_string())?;
        let body =
            serde_json::from_slice(CStr::from_ptr(body).to_bytes()).map_err(|e| e.to_string())?;
        #[cfg(feature = "webrtc")]
        return c
            .video
            .lock()
            .unwrap()
            .api(c, method, path, body, lease, generation);
        #[cfg(not(feature = "webrtc"))]
        {
            let _: serde_json::Value = body;
            let _ = (c, lease, generation);
            if method == "GET" && path.is_empty() {
                Ok(json!({"available":false,"phases":[null,null,null]}))
            } else {
                Err("native_webrtc_feature_not_built".into())
            }
        }
    })();
    let (status, mut value) = match result {
        Ok(v) => ("200 OK", v),
        Err(e) => ("409 Conflict", json!({"error":e})),
    };
    if let Some(phase) = value["phase"].as_u64() {
        if phase < 3 {
            let state = c.inner.lock().unwrap();
            value["camera_ingress"] = json!((0..4)
                .map(|slot| {
                    let i = phase as usize * 4 + slot;
                    let times = &state.times[i];
                    let live = state.frames[i]
                        .as_ref()
                        .is_some_and(|f| fresh(f, mono_ns()));
                    let fps = if live && times.len() > 1 {
                        (times.len() - 1) as f64 * 1e9
                            / (times.back().unwrap() - times.front().unwrap()) as f64
                    } else {
                        0.0
                    };
                    json!({"slot":slot,"preview_fps":fps,"live":live,"last_dq_ns":times.back()})
                })
                .collect::<Vec<_>>());
        }
    }
    value["server_monotonic_ns"] = json!(mono_ns());
    let mut cpu: libc::timespec = mem::zeroed();
    libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut cpu);
    value["process_cpu_seconds"] = json!(cpu.tv_sec as f64 + cpu.tv_nsec as f64 / 1e9);
    response(
        fd,
        status,
        "application/json",
        "",
        &[Arc::from(serde_json::to_vec(&value).unwrap())],
    )
    .map(|_| 0)
    .unwrap_or(-1)
}
#[no_mangle]
pub unsafe extern "C" fn mix_camera_active(c: *const Camera) -> bool {
    camera_active(&*c)
}
#[no_mangle]
pub unsafe extern "C" fn mix_camera_socket_open(c: *const Camera) -> i32 {
    use std::os::fd::IntoRawFd;
    match open_socket(&*c) {
        Ok(s) => s.into_raw_fd(),
        Err(e) => {
            (*c).inner.lock().unwrap().error = Some(e.to_string());
            -1
        }
    }
}
#[no_mangle]
pub unsafe extern "C" fn mix_camera_ingest(c: *const Camera, data: *const u8, len: usize) -> bool {
    let c = &*c;
    if len > MAX_PACKET || data.is_null() {
        return false;
    }
    let mut state = c.inner.lock().unwrap();
    match state.accept(std::slice::from_raw_parts(data, len)) {
        Ok(()) => {
            c.changed.notify_all();
            true
        }
        Err(e) => {
            state.rejected += 1;
            state.error = Some(e.into());
            false
        }
    }
}
#[no_mangle]
pub unsafe extern "C" fn mix_camera_clear(c: *const Camera) {
    (*c).inner.lock().unwrap().clear();
}

fn video_allowed() -> bool {
    match std::fs::read("/run/mix-video-control/state.json") {
        Ok(b) => serde_json::from_slice::<serde_json::Value>(&b)
            .ok()
            .is_some_and(|v| v["enabled"] == true),
        Err(e) => e.kind() == io::ErrorKind::NotFound,
    }
}

fn camera_active(c: &Camera) -> bool {
    let mut gate = c.gate.lock().unwrap();
    if gate.0.elapsed() > Duration::from_millis(200) {
        *gate = (Instant::now(), video_allowed());
    }
    let allowed = gate.1;
    drop(gate);
    c.live
        && allowed
        && !c.stop.load(Ordering::Relaxed)
        && c.inner
            .lock()
            .unwrap()
            .last_request
            .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
}
