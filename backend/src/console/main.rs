#![recursion_limit = "256"]
mod control;
mod controllers;
mod http;
mod load;
mod preview;
mod screen;
mod telemetry;
mod util;
use mix_camera_native::*;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    ffi::CString,
    fs, io,
    net::{Ipv4Addr, TcpListener, TcpStream},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicI32, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use util::*;
struct Native {
    pointer: usize,
    tid: AtomicI32,
}
impl Native {
    fn ptr(&self) -> *const Camera {
        self.pointer as *const Camera
    }
}
struct App {
    live: bool,
    screen: bool,
    address: String,
    origin: String,
    token: String,
    assets: HashMap<String, Vec<u8>>,
    control: control::Control,
    telemetry: Arc<telemetry::Telemetry>,
    preview: Arc<preview::Preview>,
    camera: Arc<Native>,
    video: Option<Arc<screen::Manager>>,
    connections: AtomicUsize,
    ws: AtomicUsize,
    frames: AtomicUsize,
}
impl App {
    fn snapshot(&self) -> Value {
        let mut v = self.telemetry.snapshot();
        v["preview"] = self.preview.status();
        v
    }
    fn handle(&self, mut stream: TcpStream) -> io::Result<()> {
        let Some(_permit) = http::Permit::take(&self.connections, 96) else {
            return http::json_reply(
                &mut stream,
                429,
                &json!({"error":"connection_limit"}),
                self.screen,
            );
        };
        let r = match http::Request::read(&stream) {
            Ok(r) => r,
            Err(e) => {
                return http::json_reply(
                    &mut stream,
                    400,
                    &json!({"error":e.to_string()}),
                    self.screen,
                )
            }
        };
        if stopped() {
            return http::json_reply(
                &mut stream,
                503,
                &json!({"error":"service_stopping"}),
                self.screen,
            );
        }
        if r.method != "GET" && !r.same_origin(&self.origin) {
            return http::json_reply(
                &mut stream,
                403,
                &json!({"error":"Unexpected origin"}),
                self.screen,
            );
        }
        let name = if r.path == "/" {
            "index.html"
        } else {
            r.path.trim_start_matches('/')
        };
        if r.method == "GET" {
            if let Some(data) = self.assets.get(name) {
                let kind = if name.ends_with(".js") {
                    "text/javascript"
                } else if name.ends_with(".css") {
                    "text/css"
                } else {
                    "text/html; charset=utf-8"
                };
                return http::reply(&mut stream, 200, data, kind, self.screen);
            }
        }
        if self.screen {
            if r.headers.get("authorization") != Some(&format!("Bearer {}", self.token)) {
                return http::json_reply(&mut stream, 401, &json!({"error":"Unauthorized"}), true);
            }
            if !self.live {
                return http::json_reply(
                    &mut stream,
                    503,
                    &json!({"error":"live_video_disabled_in_offline_mode"}),
                    true,
                );
            }
            let result = self
                .video
                .as_ref()
                .unwrap()
                .api(&r.method, &r.path, r.json()?);
            let (status, v) = result.unwrap_or_else(|e| (409, json!({"error":e})));
            return http::json_reply(&mut stream, status, &v, true);
        }
        if r.path.starts_with("/video/") {
            if !self.live {
                return http::json_reply(
                    &mut stream,
                    503,
                    &json!({"error":"live_video_disabled_in_offline_mode"}),
                    true,
                );
            }
            return match http::proxy(&mut stream, &r, &self.address, &self.token) {
                Ok(()) => Ok(()),
                Err(e) => http::json_reply(
                    &mut stream,
                    503,
                    &json!({"error":"local_video_unavailable","reason":e.to_string()}),
                    true,
                ),
            };
        }
        if r.path == "/api/video-control" && matches!(r.method.as_str(), "GET" | "POST") {
            let state = if r.method == "POST" {
                let v = r.json()?;
                self.control
                    .configure(v["action"].as_str().unwrap_or(""), v["limit"].as_u64())
            } else {
                self.control.status()
            };
            return match state {
                Ok(mut v) => {
                    v["camera_ingest"] = json!("rust");
                    v["backend"] = json!("rust");
                    v["camera_ingest_thread_id"] = json!(self.camera.tid.load(Ordering::Relaxed));
                    http::json_reply(&mut stream, 200, &v, false)
                }
                Err(e) => {
                    http::json_reply(&mut stream, 409, &json!({"error":e.to_string()}), false)
                }
            };
        }
        if let Some(route) = r.path.strip_prefix("/api/cameras/webrtc") {
            if !route.is_empty() && !route.starts_with('/') {
                return http::json_reply(&mut stream, 404, &json!({"error":"not_found"}), false);
            }
            let body = r.json()?;
            let mut lock = None;
            let mut lease = None;
            let mut generation = 0;
            if r.method == "POST" && route.is_empty() {
                let phase = body["phase"].as_u64().filter(|p| *p < 3);
                let fps = body["fps"].as_u64().unwrap_or(30);
                let bitrate = body["bitrate"].as_u64().unwrap_or(2_000_000);
                if phase.is_none()
                    || ![15, 25, 30].contains(&fps)
                    || !(500_000..=8_000_000).contains(&bitrate)
                {
                    return http::json_reply(
                        &mut stream,
                        400,
                        &json!({"error":"Invalid phase/fps/bitrate"}),
                        false,
                    );
                }
                match self
                    .control
                    .reserve(&format!("phase-{}", phase.unwrap()), 640, 480, fps)
                {
                    Ok((a, b, g)) => {
                        lock = Some(a);
                        lease = Some(b);
                        generation = g;
                    }
                    Err(e) => {
                        return http::json_reply(
                            &mut stream,
                            409,
                            &json!({"error":e.to_string()}),
                            false,
                        )
                    }
                }
            }
            let m = CString::new(r.method).map_err(io::Error::other)?;
            let path = CString::new(route).map_err(io::Error::other)?;
            let body = CString::new(body.to_string()).map_err(io::Error::other)?;
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            unsafe {
                mix_camera_video(
                    self.camera.ptr(),
                    stream.as_raw_fd(),
                    m.as_ptr(),
                    path.as_ptr(),
                    body.as_ptr(),
                    lease.as_ref().map(AsRawFd::as_raw_fd).unwrap_or(-1),
                    generation,
                );
            }
            drop(lease);
            drop(lock);
            return Ok(());
        }
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/ws/state") => {
                let Some(_permit) = http::Permit::take(&self.ws, 4) else {
                    return http::json_reply(
                        &mut stream,
                        429,
                        &json!({"error":"state_viewer_limit"}),
                        false,
                    );
                };
                http::websocket(stream, &r, &self.origin, || self.snapshot())
            }
            ("GET", "/api/state") => http::json_reply(&mut stream, 200, &self.snapshot(), false),
            ("GET", "/api/cameras") => {
                unsafe {
                    mix_camera_status(self.camera.ptr(), stream.as_raw_fd());
                }
                Ok(())
            }
            ("GET", "/api/cameras/frames") => {
                if !self.live || !self.control.state().is_ok_and(|v| v["enabled"] == true) {
                    return http::json_reply(
                        &mut stream,
                        409,
                        &json!({"error":"all_video_manually_disabled"}),
                        false,
                    );
                }
                let after = r.query("after").unwrap_or("0").parse::<u64>();
                let mask = r.query("streams").unwrap_or("262143").parse::<u32>();
                let (Ok(after), Ok(mask)) = (after, mask) else {
                    return http::json_reply(
                        &mut stream,
                        400,
                        &json!({"error":"invalid_camera_revision"}),
                        false,
                    );
                };
                if mask == 0 || mask >= 1 << 18 {
                    return http::json_reply(
                        &mut stream,
                        400,
                        &json!({"error":"invalid_camera_mask"}),
                        false,
                    );
                }
                let Some(_permit) = http::Permit::take(&self.frames, 4) else {
                    return http::json_reply(
                        &mut stream,
                        429,
                        &json!({"error":"camera_viewer_limit"}),
                        false,
                    );
                };
                stream.set_write_timeout(Some(Duration::from_secs(3)))?;
                unsafe {
                    mix_camera_frames(self.camera.ptr(), stream.as_raw_fd(), after, mask);
                }
                Ok(())
            }
            ("GET", "/frame.jpg") => {
                if self.live && !self.control.state().is_ok_and(|v| v["enabled"] == true) {
                    return http::json_reply(
                        &mut stream,
                        409,
                        &json!({"error":"all_video_manually_disabled"}),
                        false,
                    );
                }
                if let Some(frame) = self.preview.frame() {
                    http::reply(&mut stream, 200, &frame, self.preview.mime(), false)
                } else {
                    http::json_reply(&mut stream, 503, &json!({"error":"no_frame"}), false)
                }
            }
            ("POST", "/api/preview") => match self.preview.configure(r.json()?) {
                Ok(v) => http::json_reply(&mut stream, 200, &v, false),
                Err(e) => {
                    http::json_reply(&mut stream, 400, &json!({"error":e.to_string()}), false)
                }
            },
            _ => http::json_reply(&mut stream, 404, &json!({"error":"not_found"}), false),
        }
    }
}
fn capture_valid(p: &Path, screen: bool) -> bool {
    if !trusted(p, 0, false) {
        return false;
    }
    let allowed = if screen {
        [
            "/opt/mix-remote-video/nv12-capture",
            "/opt/mix-remote-video-gpu/nv12-capture-gpu",
            "/opt/mix-remote-video-gpu-specialized/nv12-capture-gpu",
        ]
        .contains(&p.to_str().unwrap_or(""))
    } else {
        p == Path::new("/opt/mix-debug-console/capture")
    };
    if !allowed {
        return false;
    }
    let Ok(b) = fs::read(p) else { return false };
    b.len() > 20
        && b[..4] == *b"\x7fELF"
        && b[4] == 2
        && b[5] == 1
        && u16::from_le_bytes([b[18], b[19]]) == 183
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--wait-address") {
        if args.len() != 5 || args[3] != "--timeout" {
            return Err("wait-address arguments".into());
        }
        let ip: Ipv4Addr = args[2].parse()?;
        let seconds = args[4].parse::<u64>()?;
        if ip.is_unspecified()
            || !(ip.is_private() || ip.is_loopback())
            || !(1..=120).contains(&seconds)
        {
            return Err("wait-address scope".into());
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        loop {
            match std::net::UdpSocket::bind((ip, 0)) {
                Ok(_) => return Ok(()),
                Err(e)
                    if e.kind() == io::ErrorKind::AddrNotAvailable
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(250))
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    if args.get(1).map(String::as_str) == Some("--relay-reader") {
        if args.len() != 4 {
            return Err("observer arguments".into());
        }
        telemetry::relay_reader(&args[2], &args[3])?;
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("--signal-child") {
        if args.len() != 4 || unsafe { libc::geteuid() } != 1000 {
            return Err("signal helper arguments".into());
        }
        let pid = args[2].parse::<i32>()?;
        let sig = args[3].parse::<i32>()?;
        if pid <= 1 || ![libc::SIGTERM, libc::SIGKILL].contains(&sig) {
            return Err("signal helper scope".into());
        }
        if unsafe { libc::kill(pid, sig) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        return Ok(());
    }
    let root = std::env::current_exe()?.parent().unwrap().to_path_buf();
    let screen =
        args.iter().any(|s| s == "--screen-service") || args[0].ends_with("mix-video-server");
    let mut options = HashMap::new();
    let mut offline = false;
    let mut it = args.iter().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--screen-service" => (),
            "--offline" => offline = true,
            "--listen"
            | "--address"
            | "--port"
            | "--token-file"
            | "--session-file"
            | "--replay"
            | "--image"
            | "--capture"
            | "--camera-socket"
            | "--controller-log"
            | "--controller-boot-id" => {
                options.insert(k.as_str(), it.next().ok_or("Missing option value")?.clone());
            }
            "--help" => {
                println!("Native A081 debug / screen server: --listen PRIVATE_IP --port PORT --token-file FILE [--offline] [--screen-service] [--capture FILE] [--session-file FILE]");
                return Ok(());
            }
            _ => return Err(format!("Unknown option: {k}").into()),
        }
    }
    let address = options
        .get("--listen")
        .or_else(|| options.get("--address"))
        .cloned()
        .unwrap_or("127.0.0.1".into());
    let ip: Ipv4Addr = address.parse()?;
    if ip.is_unspecified() || !(ip.is_private() || ip.is_loopback()) {
        return Err("Use a specific private IPv4".into());
    }
    let port = options
        .get("--port")
        .map(|s| s.parse::<u16>())
        .transpose()?
        .unwrap_or(if screen { 8091 } else { 8090 });
    if port < 1024 {
        return Err("Port must be >=1024".into());
    }
    let replay = options.get("--replay").map(PathBuf::from);
    let live = !offline && replay.is_none();
    if options.contains_key("--image") && live {
        return Err("Historical image requires offline/replay".into());
    }
    if live
        && (unsafe { libc::geteuid() } != 0
            || std::env::consts::ARCH != "aarch64"
            || text("/etc/a081-rootfs-id") != "a081-steamos-arm64"
            || fs::read("/proc/device-tree/model")?
                .strip_suffix(&[0])
                .unwrap_or(&[])
                != b"iQIYI Qiyu MIX (A081)")
    {
        return Err("Live mode requires root on identified A081 SteamOS".into());
    }
    let token_path = PathBuf::from(options.get("--token-file").ok_or("--token-file required")?);
    if !trusted(&token_path, unsafe { libc::geteuid() }, true) {
        return Err("Token must be a private regular file owned by this user".into());
    }
    let token = text(token_path);
    if token.len() < 32 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("Token requires >=32 printable ASCII characters".into());
    }
    let capture = options
        .get("--capture")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if screen {
                std::env::var_os("MIX_VIDEO_CAPTURE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/opt/mix-remote-video/nv12-capture"))
            } else {
                root.join("capture")
            }
        });
    if live && !capture_valid(&capture, screen) {
        return Err("Untrusted capture program".into());
    }
    let asset_root = if screen {
        root.join("web")
    } else {
        root.clone()
    };
    let mut assets = HashMap::new();
    for name in if screen {
        vec!["index.html", "app.js", "style.css"]
    } else {
        vec![
            "index.html",
            "app.js",
            "style.css",
            "dashboard.js",
            "cameras.js",
            "phase-video.js",
        ]
    } {
        assets.insert(name.into(), fs::read(asset_root.join(name))?);
    }
    unsafe {
        libc::signal(libc::SIGTERM, shutdown as *const () as usize);
        libc::signal(libc::SIGINT, shutdown as *const () as usize);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let control = control::Control::new(live)?;
    let telemetry = telemetry::Telemetry::new(
        live,
        replay,
        options
            .get("--session-file")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/run/mix-vr-shell-controller/session.json".into()),
        root.clone(),
        options.get("--controller-log").map(PathBuf::from),
        options.get("--controller-boot-id").cloned(),
    );
    let preview = preview::Preview::new(
        (live && !screen).then_some(capture.clone()),
        options.get("--image").map(PathBuf::from),
        control.clone(),
    );
    let socket = CString::new(
        options
            .get("--camera-socket")
            .map(String::as_str)
            .unwrap_or("/run/mix-camera-preview/frames.sock"),
    )?;
    let pointer = unsafe { mix_camera_create(socket.as_ptr(), live && !screen) };
    if pointer.is_null() {
        return Err("Camera initialization".into());
    }
    let camera = Arc::new(Native {
        pointer: pointer as usize,
        tid: AtomicI32::new(0),
    });
    let mut workers = vec![];
    if !screen {
        telemetry.start();
        preview.start();
        let c = camera.clone();
        workers.push(std::thread::spawn(move || {
            c.tid.store(
                unsafe { libc::syscall(libc::SYS_gettid) } as i32,
                Ordering::Relaxed,
            );
            unsafe {
                mix_camera_run(c.ptr());
            }
        }));
        let c = camera.clone();
        workers.push(std::thread::spawn(move || unsafe {
            mix_camera_video_run(c.ptr())
        }));
    }
    let video = (screen && live).then(|| screen::Manager::new(control.clone(), capture));
    let app = Arc::new(App {
        live,
        screen,
        address: address.clone(),
        origin: format!("http://{address}:{port}"),
        token,
        assets,
        control,
        telemetry,
        preview,
        camera,
        video,
        connections: AtomicUsize::new(0),
        ws: AtomicUsize::new(0),
        frames: AtomicUsize::new(0),
    });
    let listener = TcpListener::bind((ip, port))?;
    listener.set_nonblocking(true)?;
    eprintln!(
        "{}",
        json!({"backend":"rust","service":if screen{"screen-video"}else{"debug-console"},"address":address,"port":port,"live":live})
    );
    while !stopped() {
        match listener.accept() {
            Ok((stream, _)) => {
                if app.connections.load(Ordering::Relaxed) >= 96 {
                    drop(stream);
                    continue;
                }
                let app = app.clone();
                std::thread::spawn(move || {
                    let _ = app.handle(stream);
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => pause(20),
            Err(e) => return Err(e.into()),
        }
    }
    unsafe {
        mix_camera_stop(app.camera.ptr());
    }
    if let Some(v) = &app.video {
        v.close();
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("mix-native-server: {e}");
        std::process::exit(1);
    }
}
