const HEAD_WIRE_SIZE: usize = 240;

use super::{
    controllers::{self, Controllers},
    load::Load,
    util::*,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
pub struct Data {
    pub head: Value,
    pub observed: u64,
    pub session: Value,
    pub samples: VecDeque<(u64, u64)>,
    pub controllers: Controllers,
    pub system: Value,
    pub native: Value,
}
pub struct Telemetry {
    pub data: Mutex<Data>,
    pub requested: AtomicU64,
    pub live: bool,
    pub replay: Option<PathBuf>,
    pub session_file: PathBuf,
    pub root: PathBuf,
    pub explicit_log: Option<PathBuf>,
    pub explicit_boot: Option<String>,
}
impl Telemetry {
    pub fn new(
        live: bool,
        replay: Option<PathBuf>,
        session_file: PathBuf,
        root: PathBuf,
        explicit_log: Option<PathBuf>,
        explicit_boot: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(Data {
                head: json!({"valid":false,"reasons":["no_source"]}),
                observed: 0,
                session: Value::Null,
                samples: VecDeque::new(),
                controllers: Controllers::default(),
                system: json!({"scope":if live{"headset"}else{"development-host"}}),
                native: Value::Null,
            }),
            requested: AtomicU64::new(0),
            live,
            replay,
            session_file,
            root,
            explicit_log,
            explicit_boot,
        })
    }
    pub fn start(self: &Arc<Self>) {
        let a = self.clone();
        std::thread::spawn(move || a.head_loop());
        if self.live {
            let a = self.clone();
            std::thread::spawn(move || a.controller_loop());
            let a = self.clone();
            std::thread::spawn(move || super::load::observer(a));
        }
        let a = self.clone();
        std::thread::spawn(move || {
            let mut load = Load::default();
            let mut last = 0;
            while !stopped() {
                let n = now();
                if fresh(a.requested.load(Ordering::Relaxed), n, 10_000_000_000) {
                    if a.live {
                        let selected =
                            controllers::selection(&a.session_file).unwrap_or(Value::Null);
                        let mut c = a.data.lock().unwrap().controllers.clone();
                        c.logs(
                            selected,
                            a.explicit_log.as_deref(),
                            a.explicit_boot.as_deref(),
                        );
                        let mut d = a.data.lock().unwrap();
                        if c.selected["trial"] == d.controllers.selected["trial"] {
                            c.packets = d.controllers.packets.clone();
                            c.connected =
                                d.controllers.connected && c.selected["cleanup_complete"] != true;
                        }
                        d.controllers = c;
                    }
                    if n - last >= 2_000_000_000 {
                        let system = load.system(a.live);
                        a.data.lock().unwrap().system = system;
                        last = n;
                    }
                }
                pause(100);
            }
        });
    }
    fn update_head(&self, v: Value, count: u64) {
        let t = now();
        let mut d = self.data.lock().unwrap();
        d.head = v;
        d.observed = t;
        if d.samples.len() == 120 {
            d.samples.pop_front();
        }
        d.samples.push_back((t, count));
    }
    fn head_loop(self: Arc<Self>) {
        if let Some(path) = &self.replay {
            if let Ok(mut f) = File::open(path) {
                let mut header = [0; 12];
                if f.read_exact(&mut header).is_err() { return; }
                let size = match (u32le(&header, 0), u32le(&header, 4), u32le(&header, 8)) {
                    (0x54523841, 1, 168) => 168,
                    (0x54523841, 2, 240) => 240,
                    _ => return,
                };
                if std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0)).is_err() { return; }
                let mut b = vec![0; size];
                let mut first = None;
                let start = now();
                let mut last = 0;
                while !stopped() && f.read_exact(&mut b).is_ok() {
                    let sent = u64le(&b, 56);
                    let first = *first.get_or_insert(sent);
                    let delay = sent.saturating_sub(first).saturating_sub(now() - start);
                    pause((delay / 1_000_000).min(5000));
                    if sent.saturating_sub(last) >= 33_000_000 {
                        if let Some(mut v) = decode_head(&b, sent) {
                            v["recorded_valid"] = v["valid"].clone();
                            v["valid"] = json!(false);
                            v["historical"] = json!(true);
                            self.update_head(v, 0);
                        }
                        last = sent;
                    }
                }
            }
            self.update_head(
                json!({"valid":false,"historical":true,"reasons":["replay_finished"]}),
                0,
            );
            return;
        }
        if !self.live {
            return;
        }
        while !stopped() {
            let Some(d) = session(&self.session_file).filter(|d| d["cleanup_complete"] != true)
            else {
                pause(500);
                continue;
            };
            let path = d["public_socket"].as_str().unwrap_or("");
            if !path
                .strip_prefix("/run/a081-qvr-pose-")
                .and_then(|s| s.strip_suffix("/tracking.sock"))
                .is_some_and(safe_id)
            {
                pause(500);
                continue;
            }
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.args(["--relay-reader", "head", path])
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            user_child(&mut cmd);
            if let Ok(mut child) = cmd.spawn() {
                let mut output = child.stdout.take().unwrap();
                let mut pending = vec![];
                let mut checked = 0;
                {
                    let mut data = self.data.lock().unwrap();
                    data.session = d["trial"].clone();
                    data.samples.clear();
                }
                while !stopped() {
                    let n = now();
                    if n - checked > 100_000_000 {
                        let current = session(&self.session_file);
                        if current.as_ref().is_none_or(|v| {
                            v["public_socket"] != d["public_socket"]
                                || v["cleanup_complete"] == true
                        }) {
                            break;
                        }
                        checked = n;
                    }
                    if !readable(output.as_raw_fd(), 100) {
                        continue;
                    }
                    let mut b = [0; 16384];
                    let Ok(len) = output.read(&mut b) else { break };
                    if len == 0 {
                        break;
                    }
                    pending.extend_from_slice(&b[..len]);
                    while pending.len() >= 8 + HEAD_WIRE_SIZE {
                        let record: Vec<_> = pending.drain(..(8 + HEAD_WIRE_SIZE)).collect();
                        let count = u64le(&record, 0);
                        if let Some(mut v) = decode_head(&record[8..], now()) {
                            v["received"] = json!(count);
                            v["observed_ns"] = json!(now());
                            self.update_head(v, count);
                        }
                    }
                }
                stop_child(&mut child, true);
            }
            self.update_head(
                json!({"valid":false,"disconnected":true,"reasons":["source_disconnected"]}),
                0,
            );
            pause(500);
        }
    }
    fn controller_loop(self: Arc<Self>) {
        while !stopped() {
            let Some(d) = controllers::selection(&self.session_file)
                .filter(|d| d["cleanup_complete"] != true)
            else {
                pause(500);
                continue;
            };
            let endpoint = d["controller_pose_socket"].as_str().unwrap();
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.args(["--relay-reader", "controller", endpoint])
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            user_child(&mut cmd);
            if let Ok(mut child) = cmd.spawn() {
                let mut output = child.stdout.take().unwrap();
                let mut pending = vec![];
                {
                    let mut data = self.data.lock().unwrap();
                    data.controllers.selected = d.clone();
                    data.controllers.packets = [Value::Null, Value::Null];
                    data.controllers.connected = false;
                }
                let mut checked = 0;
                let mut bad = false;
                while !stopped() && !bad {
                    let n = now();
                    if n - checked > 100_000_000 {
                        if controllers::selection(&self.session_file).is_none_or(|v| {
                            v["trial"] != d["trial"]
                                || v["controller_pose_socket"] != d["controller_pose_socket"]
                                || v["cleanup_complete"] == true
                        }) {
                            break;
                        }
                        checked = n;
                    }
                    if !readable(output.as_raw_fd(), 100) {
                        continue;
                    }
                    let mut b = [0; 16384];
                    let Ok(len) = output.read(&mut b) else { break };
                    if len == 0 {
                        break;
                    }
                    pending.extend_from_slice(&b[..len]);
                    while pending.len() >= 96 {
                        let record: Vec<_> = pending.drain(..96).collect();
                        let Some(v) = controllers::decode(&record) else {
                            bad = true;
                            break;
                        };
                        let h = number(&v["hand"]) as usize;
                        let mut state = self.data.lock().unwrap();
                        let prev = &state.controllers.packets[h];
                        if !prev.is_null()
                            && (prev["packet_session"] != v["packet_session"]
                                || number(&prev["sequence"]) >= number(&v["sequence"]))
                        {
                            bad = true;
                            break;
                        }
                        state.controllers.connected = true;
                        state.controllers.packets[h] = v;
                    }
                }
                stop_child(&mut child, true);
            }
            let mut d = self.data.lock().unwrap();
            d.controllers.connected = false;
            d.controllers.packets = [Value::Null, Value::Null];
            drop(d);
            pause(500);
        }
    }
    pub fn snapshot(&self) -> Value {
        let n = now();
        self.requested.store(n, Ordering::Relaxed);
        let d = self.data.lock().unwrap();
        let mut head = d.head.clone();
        let age_ = number(&head["sample_ns"]);
        head["observer_age_ms"] = age(d.observed, n);
        head["age_ms"] = if self.live { age(age_, n) } else { Value::Null };
        if self.live && (!fresh(age_, n, 100_000_000) || !fresh(d.observed, n, 100_000_000)) {
            head["valid"] = json!(false);
            let mut reasons = head["reasons"].as_array().cloned().unwrap_or_default();
            if !reasons.contains(&json!("stale")) {
                reasons.push(json!("stale"));
            }
            head["reasons"] = json!(reasons);
        }
        if !self.live {
            head["valid"] = json!(false);
        }
        let rate = if fresh(d.observed, n, 200_000_000) {
            d.samples
                .front()
                .zip(d.samples.back())
                .filter(|(a, b)| b.0 > a.0 && b.1 >= a.1)
                .map(|(a, b)| (b.1 - a.1) as f64 * 1e9 / (b.0 - a.0) as f64)
        } else {
            None
        };
        let native = if !self.live {
            json!({"available":false,"reason":"offline"})
        } else if d.native["received_monotonic"]
            .as_f64()
            .is_some_and(|t| n as f64 / 1e9 - t < 2.0)
        {
            let mut v = d.native.clone();
            v["available"] = json!(true);
            v["age_ms"] =
                json!(n as f64 / 1e6 - v["received_monotonic"].as_f64().unwrap() * 1000.0);
            v
        } else {
            json!({"available":false,"reason":"no_fresh_openvr_sample"})
        };
        json!({"mode":if self.replay.is_some(){"replay"}else if self.live{"live"}else{"offline"},"session":d.session,"head":head,"native_frames":native,"packet_hz":rate,"controllers":d.controllers.snapshot(),"system":d.system,"source":self.replay.as_deref().unwrap_or(&self.session_file),"read_only":true,"generated_monotonic_ns":n.to_string(),"backend":"rust"})
    }
}
pub fn decode_head(p: &[u8], t: u64) -> Option<Value> {
    if !matches!(p.len(), 168 | 240) || u32le(p, 0) != 0x54523841 ||
        !matches!((u32le(p, 4), u32le(p, 8), p.len()), (1, 168, 168) | (2, 240, 240)) {
        return None;
    }
    let flags = u32le(p, 12);
    let gyro = u64le(p, 32);
    let sample = u64le(p, 48);
    let sent = u64le(p, 56);
    let pose = u64le(p, 64);
    let camera = u64le(p, 72);
    let f: Vec<_> = (0..14).map(|i| f32le(p, 80 + 4 * i)).collect();
    let quality = f[13];
    let prediction: Vec<_> = (168..p.len()).step_by(4).map(|offset| f32le(p, offset)).collect();
    let state = u32le(p, 136);
    let warning = u32le(p, 140);
    let bracket = u32le(p, 148);
    let mut reasons = vec![];
    if flags != 15 || u32le(p, 152) != 1 || u32le(p, 144) >= 80 {
        reasons.push("packet_flags");
    }
    if quality != 1.0 || state != 4 {
        reasons.push("tracking_quality");
    }
    if pose != gyro {
        reasons.push("pose_timestamp");
    }
    if !fresh(camera, gyro, 100_000_000) {
        reasons.push("camera_age");
    }
    if !fresh(sample, t, 100_000_000) || sample > sent || sent > t {
        reasons.push("pose_age");
    }
    if bracket > 50000 {
        reasons.push("clock_bracket");
    }
    if f.iter().chain(prediction.iter()).any(|x| !x.is_finite()) {
        reasons.push("nonfinite");
    } else if (f[6..10].iter().map(|x| x * x).sum::<f32>() - 1.0).abs() > 0.01 {
        reasons.push("quaternion_norm");
    }
    Some(
        json!({"session":u64le(p,16).to_string(),"sequence":u64le(p,24),"sample_ns":sample,"send_ns":sent,"valid":reasons.is_empty(),"reasons":reasons,"quality":finite(quality),"tracking_state":state,"warning_flags":warning,"clock_bracket_us":bracket as f64/1000.0,"camera_age_ms":age(camera,gyro),"position":[finite(-f[11]),finite(f[10]),finite(f[12])],"orientation":[finite(-f[7]),finite(f[6]),finite(f[8]),finite(f[9])],"gyro":f[..3].iter().map(|x|finite(*x)).collect::<Vec<_>>(),"accel":f[3..6].iter().map(|x|finite(*x)).collect::<Vec<_>>(),"coordinate_profile":"android-portrait-v1","physical_accuracy_calibrated":false}),
    )
}
pub fn relay_reader(kind: &str, path: &str) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 1000 {
        return Err(io::Error::other("observer_uid"));
    }
    let size = if kind == "head" {
        HEAD_WIRE_SIZE
    } else if kind == "controller" {
        96
    } else {
        return Err(io::Error::other("observer kind"));
    };
    unsafe {
        let fd = libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let sock = File::from_raw_fd(fd);
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        if path.len() >= addr.sun_path.len() {
            return Err(io::Error::other("socket path"));
        }
        addr.sun_family = libc::AF_UNIX as _;
        for (a, b) in addr.sun_path.iter_mut().zip(path.bytes()) {
            *a = b as _;
        }
        if libc::connect(
            fd,
            &addr as *const _ as _,
            std::mem::size_of_val(&addr) as _,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        if libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as _,
            &mut len,
        ) != 0
            || cred.uid != 0
        {
            return Err(io::Error::other("relay owner"));
        }
        let mut out = io::stdout().lock();
        let mut pending = None;
        let mut count = 0u64;
        let mut next = 0u64;
        loop {
            let delay = if pending.is_some() {
                next.saturating_sub(now()).div_ceil(1_000_000)
            } else {
                1000
            };
            if readable(sock.as_raw_fd(), delay.min(1000) as i32) {
                for _ in 0..1024 {
                    let mut p = vec![0u8; size];
                    let n = libc::recv(
                        fd,
                        p.as_mut_ptr() as _,
                        p.len(),
                        libc::MSG_DONTWAIT | libc::MSG_TRUNC,
                    );
                    if n < 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() == io::ErrorKind::WouldBlock {
                            break;
                        }
                        return Err(e);
                    }
                    if n == 0 {
                        return Ok(());
                    }
                    if n as usize != size {
                        return Err(io::Error::other("truncated relay packet"));
                    }
                    count += 1;
                    if kind == "controller" {
                        out.write_all(&p)?;
                        out.flush()?;
                    } else {
                        pending = Some(p);
                    }
                }
            }
            if kind == "head" && now() >= next {
                if let Some(p) = pending.take() {
                    out.write_all(&count.to_le_bytes())?;
                    out.write_all(&p)?;
                    out.flush()?;
                    next = now() + 16_000_000;
                }
            }
        }
    }
}
