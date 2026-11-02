use super::{control::Control, util::*};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as app;
use gstreamer_sdp as sdp;
use gstreamer_webrtc as rtc;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::File,
    io::{self, BufRead, BufReader, Read},
    os::fd::AsRawFd,
    path::PathBuf,
    process::{Child, ChildStdout, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
#[derive(Default)]
struct Count {
    frames: u64,
    bytes: u64,
    first: u64,
    last: u64,
    first_pts: u64,
    last_pts: u64,
    nonincreasing: u64,
    caps: Option<String>,
}
impl Count {
    fn add(&mut self, size: u64, pts: u64, t: u64) {
        if self.frames == 0 {
            self.first = t;
            self.first_pts = pts;
        } else if pts <= self.last_pts {
            self.nonincreasing += 1;
        }
        self.frames += 1;
        self.bytes += size;
        self.last = t;
        self.last_pts = pts;
    }
    fn value(&self) -> Value {
        json!({"frames":self.frames,"bytes":self.bytes,"first_monotonic_ns":self.first,"last_monotonic_ns":self.last,"first_pts_ns":self.first_pts,"last_pts_ns":self.last_pts,"nonincreasing_pts":self.nonincreasing,"observed_fps":if self.frames>1&&self.last>self.first{Some((self.frames-1)as f64*1e9/(self.last-self.first)as f64)}else{None},"pts_fps":if self.frames>1&&self.last_pts>self.first_pts{Some((self.frames-1)as f64*1e9/(self.last_pts-self.first_pts)as f64)}else{None},"caps":self.caps})
    }
}
#[derive(Default)]
struct Stats {
    offer: Option<Value>,
    ice: Vec<Value>,
    error: Option<String>,
    capture: Value,
    capture_errors: BTreeMap<String, u64>,
    timing: BTreeMap<String, VecDeque<f64>>,
    latency: BTreeMap<String, VecDeque<f64>>,
    phase_counts: BTreeMap<String, u64>,
    phase_errors: BTreeMap<String, u64>,
    pending: BTreeMap<u64, Value>,
    counts: BTreeMap<String, Count>,
    encoder_pts: BTreeSet<u64>,
    unmatched: u64,
    flow_errors: u64,
    dropped: u64,
    missing_ordinal: u64,
    last_ordinal: u64,
    rtp: Value,
    rtp_prev: Option<(u32, Option<u64>)>,
}
fn sample(m: &mut BTreeMap<String, VecDeque<f64>>, key: &str, x: f64) {
    let q = m.entry(key.into()).or_default();
    if q.len() == 300 {
        q.pop_front();
    }
    q.push_back(x);
}
fn pct(v: &VecDeque<f64>) -> Value {
    let mut a: Vec<_> = v.iter().copied().collect();
    a.sort_by(f64::total_cmp);
    if a.is_empty() {
        return json!({"samples":0,"p50":null,"p95":null,"max":null});
    }
    json!({"samples":a.len(),"p50":a[a.len()/2],"p95":a[(a.len()-1)*95/100],"max":a[a.len()-1]})
}
impl Stats {
    fn phase(&mut self, key: &str, a: u64, b: u64) {
        if b < a {
            *self
                .phase_errors
                .entry("timestamp_order".into())
                .or_default() += 1;
            return;
        }
        sample(&mut self.latency, key, (b - a) as f64 / 1000.0);
        *self.phase_counts.entry(key.into()).or_default() += 1;
    }
    fn count(&mut self, key: &str, size: u64, pts: u64, t: u64) {
        self.counts.entry(key.into()).or_default().add(size, pts, t);
    }
}
pub fn venus(codec: &str) -> Result<(String, String), String> {
    let nodes: Vec<_> = entries("/sys/class/video4linux")
        .iter()
        .filter(|p| text(p.join("name")) == "qcom-venus-encoder")
        .map(|p| format!("/dev/{}", p.file_name().unwrap().to_string_lossy()))
        .collect();
    if nodes.len() != 1 {
        return Err("Expected one Venus encoder".into());
    }
    for feature in gst::Registry::get().features(gst::ElementFactory::static_type()) {
        let name = feature.name();
        if name.starts_with("v4l2") && name.ends_with(&format!("{codec}enc")) {
            if let Ok(e) = gst::ElementFactory::make(name.as_str()).build() {
                if e.property::<String>("device") == nodes[0] {
                    return Ok((name.to_string(), nodes[0].clone()));
                }
            }
        }
    }
    Err(format!("Venus {codec} factory unavailable"))
}
fn init() -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        gst::init().map_err(|e| e.to_string())?;
        std::thread::spawn(|| gst::glib::MainLoop::new(None, false).run());
        Ok(())
    })
    .clone()
}
#[derive(Clone)]
struct Options {
    width: u64,
    height: u64,
    fps: u64,
    bitrate: u64,
    eye: String,
    requested: String,
    codec: String,
    fallback: Option<String>,
}
impl Options {
    fn parse(v: &Value) -> Result<Self, String> {
        let width = v["width"].as_u64().unwrap_or(1280);
        let fps = v["fps"].as_u64().unwrap_or(60);
        let bitrate = v["bitrate"].as_u64().unwrap_or(8_000_000);
        let eye = v["eye"].as_str().unwrap_or("stereo");
        let requested = v["codec"].as_str().unwrap_or("h264");
        if !["stereo", "left", "right"].contains(&eye)
            || !["auto", "h264", "h265"].contains(&requested)
        {
            return Err("Unsupported eye/codec".into());
        }
        let stereo = [640, 960, 1280, 1600, 1920, 2560, 3200, 3664];
        let mono = [320, 480, 640, 800, 960, 1280, 1600, 1832];
        if !(if eye == "stereo" { stereo } else { mono }).contains(&width)
            || ![15, 30, 60].contains(&fps)
            || !(1_000_000..=100_000_000).contains(&bitrate)
            || bitrate % 1_000_000 != 0
            || width > 2560 && fps > 30
        {
            return Err("Unsupported width/fps/bitrate".into());
        }
        for key in ["width", "fps", "bitrate"] {
            if !v[key].is_null() && v[key].as_u64().is_none() {
                return Err("Expected integer settings".into());
            }
        }
        let receivers = v["receiver_codecs"].as_array().cloned().unwrap_or_default();
        if !v["receiver_codecs"].is_null() && !v["receiver_codecs"].is_array()
            || receivers.len() > 16
            || receivers.iter().any(|x| x != "h264" && x != "h265")
        {
            return Err("Invalid receiver codecs".into());
        }
        let codec = if requested != "h264" && receivers.contains(&json!("h265")) {
            "h265"
        } else {
            "h264"
        };
        let fallback =
            (requested != "h264" && codec == "h264").then(|| "receiver_no_hevc".to_string());
        let source = if eye == "stereo" { 3664 } else { 1832 };
        Ok(Self {
            width,
            height: ((width * 1920).div_ceil(source) + 1) & !1,
            fps,
            bitrate,
            eye: eye.into(),
            requested: requested.into(),
            codec: codec.into(),
            fallback,
        })
    }
}
struct InterruptRead {
    pipe: ChildStdout,
    stop: Arc<AtomicBool>,
}
impl Read for InterruptRead {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        while !self.stop.load(Ordering::Relaxed) && !stopped() {
            if readable(self.pipe.as_raw_fd(), 100) {
                return self.pipe.read(b);
            }
        }
        // Buffered reads retry Interrupted forever; cancellation must be EOF.
        Ok(0)
    }
}
struct Session {
    id: String,
    options: Options,
    pipeline: gst::Pipeline,
    source: app::AppSrc,
    peer: gst::Element,
    stats: Arc<Mutex<Stats>>,
    stop: Arc<AtomicBool>,
    capture: Option<Child>,
    reader: Option<JoinHandle<()>>,
    lease: Option<File>,
    generation: u64,
    last: Instant,
    offering: bool,
    closed: bool,
    native: bool,
    program: PathBuf,
    factory: String,
    device: String,
    level: &'static str,
    pipe_bytes: i32,
    exit: Option<i32>,
}
impl Session {
    fn new(o: Options, program: PathBuf, lease: File, generation: u64) -> Result<Self, String> {
        init()?;
        let (factory, device) = venus(&o.codec)?;
        let native = program == PathBuf::from("/opt/mix-remote-video-gpu/nv12-capture-gpu");
        let (level, level_control) =
            if o.width * o.height > 2073600 || o.width > 1920 || o.bitrate > 50_000_000 {
                ("5.1", 15i32)
            } else {
                ("4.2", 13)
            };
        let encoded = if o.codec == "h264" {
            format!("video/x-h264,profile=constrained-baseline,level=(string){level}")
        } else {
            "video/x-h265,profile=main,level=(string)5.1".into()
        };
        let input = if native {
            "appsrc name=source is-live=true format=time do-timestamp=false block=true max-buffers=4 max-bytes=0 max-time=0".into()
        } else {
            format!("appsrc name=source is-live=true format=time do-timestamp=false block=false max-buffers=2 max-bytes=0 max-time=0 leaky-type=downstream ! {factory} name=encoder output-io-mode=mmap capture-io-mode=mmap ! {encoded}")
        };
        let pipeline=gst::parse::launch(&format!("{input} ! {c}parse name=parser config-interval=-1 ! video/x-{c},stream-format=byte-stream,alignment=au ! rtp{c}pay name=pay pt=96 config-interval=-1 aggregate-mode=zero-latency ! application/x-rtp,media=video,encoding-name={},clock-rate=90000,payload=96 ! webrtcbin name=peer bundle-policy=max-bundle",o.codec.to_uppercase(),c=o.codec)).map_err(|e|e.to_string())?.downcast::<gst::Pipeline>().map_err(|_|"Invalid screen pipeline")?;
        let source = pipeline
            .by_name("source")
            .unwrap()
            .downcast::<app::AppSrc>()
            .unwrap();
        let caps = if native {
            format!("video/x-{},stream-format=byte-stream,alignment=au", o.codec)
        } else {
            format!("video/x-raw,format=NV12,width={},height={},framerate={}/1,colorimetry=bt601,interlace-mode=progressive,pixel-aspect-ratio=1/1,multiview-mode=mono",o.width,o.height,o.fps)
        };
        source.set_caps(Some(&caps.parse::<gst::Caps>().map_err(|e| e.to_string())?));
        if !native {
            let mut controls = gst::Structure::builder("controls")
                .field("video_bitrate", o.bitrate as i32)
                .field("video_gop_size", o.fps as i32)
                .build();
            if o.codec == "h264" {
                controls.set("h264_profile", 1i32);
                controls.set("h264_level", level_control);
            } else {
                controls.set("hevc_profile", 0i32);
                controls.set("hevc_level", 8i32);
            }
            pipeline
                .by_name("encoder")
                .unwrap()
                .set_property("extra-controls", controls);
        }
        let peer = pipeline.by_name("peer").unwrap();
        if let Some(t) =
            peer.emit_by_name::<Option<rtc::WebRTCRTPTransceiver>>("get-transceiver", &[&0i32])
        {
            t.set_property("direction", rtc::WebRTCRTPTransceiverDirection::Sendonly);
        }
        let stats = Arc::new(Mutex::new(Stats::default()));
        {
            let mut s = stats.lock().unwrap();
            for k in [
                "readbacks_delivered",
                "appsrc_submitted",
                "encoder_input",
                "encoded_vcl_au",
            ] {
                s.counts.insert(k.into(), Count::default());
            }
            s.rtp = json!({"packets":0,"bytes":0,"marker_access_units":0,"duplicate_marker_timestamp":0,"timestamp_delta_ticks":{},"marker_pts_delta_mismatch_over_1_tick":0,"marker_pts_delta_max_error_ticks":0.0});
        }
        let s = stats.clone();
        peer.connect("on-ice-candidate", false, move |v| {
            if let (Ok(index), Ok(candidate)) = (v[1].get::<u32>(), v[2].get::<String>()) {
                let mut s = s.lock().unwrap();
                if s.ice.len() < 128 {
                    s.ice
                        .push(json!({"candidate":candidate,"sdpMLineIndex":index}));
                }
            }
            None
        });
        if !native {
            let s = stats.clone();
            pipeline
                .by_name("encoder")
                .unwrap()
                .static_pad("sink")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                    if let Some(gst::PadProbeData::Buffer(ref b)) = info.data {
                        if let Some(pts) = b.pts() {
                            let t = now();
                            let pts = pts.nseconds();
                            let mut s = s.lock().unwrap();
                            s.count("encoder_input", b.size() as u64, pts, t);
                            s.counts.get_mut("encoder_input").unwrap().caps =
                                pad.current_caps().map(|c| c.to_string());
                            if let Some(mut p) = s.pending.remove(&pts) {
                                s.phase(
                                    "reader_to_encoder_input_us",
                                    number(&p["read_done_ns"]),
                                    t,
                                );
                                p["encoder_input_ns"] = json!(t);
                                s.pending.insert(pts, p);
                            } else {
                                *s.phase_errors
                                    .entry("encoder_input_pts_unmatched_or_duplicate".into())
                                    .or_default() += 1;
                            }
                            if b.offset() > s.last_ordinal
                                && b.offset() <= s.counts["readbacks_delivered"].frames
                            {
                                s.dropped += b.offset() - s.last_ordinal - 1;
                                s.last_ordinal = b.offset();
                            } else {
                                s.missing_ordinal += 1;
                            }
                            s.encoder_pts.insert(pts);
                            if s.encoder_pts.len() > 1024 {
                                s.encoder_pts.pop_first();
                            }
                        }
                    }
                    gst::PadProbeReturn::Ok
                });
        }
        let s = stats.clone();
        let codec = o.codec.clone();
        pipeline
            .by_name("parser")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                if let Some(gst::PadProbeData::Buffer(ref b)) = info.data {
                    if let (Some(pts), Ok(map)) = (b.pts(), b.map_readable()) {
                        let vcl = map.as_slice().windows(4).any(|w| {
                            w[..3] == [0, 0, 1]
                                && if codec == "h264" {
                                    (1..=5).contains(&(w[3] & 31))
                                } else {
                                    (w[3] >> 1) & 63 <= 31
                                }
                        });
                        if vcl {
                            let t = now();
                            let pts = pts.nseconds();
                            let mut s = s.lock().unwrap();
                            s.count("encoded_vcl_au", b.size() as u64, pts, t);
                            s.counts.get_mut("encoded_vcl_au").unwrap().caps =
                                pad.current_caps().map(|c| c.to_string());
                            if let Some(p) = s.pending.remove(&pts) {
                                if !native {
                                    s.phase(
                                        "reader_to_encoder_output_us",
                                        number(&p["read_done_ns"]),
                                        t,
                                    );
                                    s.phase(
                                        "capture_complete_to_encoder_output_us",
                                        number(&p["captured_ns"]),
                                        t,
                                    );
                                    if let Some(a) = p["encoder_input_ns"].as_u64() {
                                        s.phase("encoder_input_to_output_us", a, t);
                                    } else {
                                        *s.phase_errors
                                            .entry("encoder_output_without_input_timestamp".into())
                                            .or_default() += 1;
                                    }
                                }
                            } else {
                                *s.phase_errors
                                    .entry("encoder_output_pts_unmatched".into())
                                    .or_default() += 1;
                            }
                            if !s.encoder_pts.remove(&pts) {
                                s.unmatched += 1;
                            }
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });
        let s = stats.clone();
        pipeline
            .by_name("pay")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                move |_, info| {
                    let record = |b: &gst::BufferRef| {
                        let Ok(m) = b.map_readable() else { return };
                        let p = m.as_slice();
                        if p.len() < 12 {
                            return;
                        }
                        let mut s = s.lock().unwrap();
                        s.rtp["packets"] = json!(number(&s.rtp["packets"]) + 1);
                        s.rtp["bytes"] = json!(number(&s.rtp["bytes"]) + b.size() as u64);
                        if p[1] & 128 != 0 {
                            let ts = u32::from_be_bytes(p[4..8].try_into().unwrap());
                            let pts = b.pts().map(|p| p.nseconds());
                            s.rtp["marker_access_units"] =
                                json!(number(&s.rtp["marker_access_units"]) + 1);
                            if let Some((old, old_pts)) = s.rtp_prev {
                                let delta = ts.wrapping_sub(old);
                                if delta == 0 {
                                    s.rtp["duplicate_marker_timestamp"] =
                                        json!(number(&s.rtp["duplicate_marker_timestamp"]) + 1);
                                }
                                if let (Some(a), Some(b)) = (old_pts, pts) {
                                    let error = (delta as f64
                                        - (b as f64 - a as f64) * 90000.0 / 1e9)
                                        .abs();
                                    if error > 1.01 {
                                        s.rtp["marker_pts_delta_mismatch_over_1_tick"] = json!(
                                            number(&s.rtp["marker_pts_delta_mismatch_over_1_tick"])
                                                + 1
                                        );
                                    }
                                    s.rtp["marker_pts_delta_max_error_ticks"] = json!(error.max(
                                        s.rtp["marker_pts_delta_max_error_ticks"]
                                            .as_f64()
                                            .unwrap_or(0.0)
                                    ));
                                }
                                let h = s.rtp["timestamp_delta_ticks"].as_object_mut().unwrap();
                                let key = if h.contains_key(&delta.to_string()) || h.len() < 512 {
                                    delta.to_string()
                                } else {
                                    "other".into()
                                };
                                let n = h.get(&key).map(number).unwrap_or(0);
                                h.insert(key, json!(n + 1));
                            }
                            s.rtp_prev = Some((ts, pts));
                        }
                    };
                    match info.data {
                        Some(gst::PadProbeData::Buffer(ref b)) => record(b),
                        Some(gst::PadProbeData::BufferList(ref l)) => {
                            for b in l.iter() {
                                record(b)
                            }
                        }
                        _ => (),
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        let mut random = [0u8; 18];
        File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut random))
            .map_err(|e| e.to_string())?;
        let id = random.iter().map(|b| format!("{b:02x}")).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let mut session = Self {
            id,
            options: o.clone(),
            pipeline,
            source,
            peer,
            stats,
            stop,
            capture: None,
            reader: None,
            lease: Some(lease),
            generation,
            last: Instant::now(),
            offering: false,
            closed: false,
            native,
            program: program.clone(),
            factory,
            device,
            level,
            pipe_bytes: 0,
            exit: None,
        };
        session
            .pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&program);
        cmd.args([
            "--width",
            &o.width.to_string(),
            "--fps",
            &o.fps.to_string(),
            "--eye",
            &o.eye,
        ]);
        if native {
            cmd.args(["--codec", &o.codec, "--bitrate", &o.bitrate.to_string()]);
        }
        if program.file_name().is_some_and(|s| s == "nv12-capture-gpu") {
            for k in [
                "TU_A081_KMS_DEVICE",
                "ENABLE_GAMESCOPE_WSI",
                "VK_ICD_FILENAMES",
                "LD_PRELOAD",
            ] {
                cmd.env_remove(k);
            }
            cmd.env(
                "VK_DRIVER_FILES",
                "/opt/mix-steamvr-turnip/share/vulkan/icd.d/freedreno_icd.aarch64.json",
            )
            .env("DISABLE_GAMESCOPE_WSI", "1");
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let stdout = child.stdout.take().unwrap();
        session.capture = Some(child);
        session.pipe_bytes =
            unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETPIPE_SZ, 1048576) };
        if session.pipe_bytes != 1048576 {
            return Err("Capture pipe size must be 1048576".into());
        }
        let p = session.pipeline.clone();
        let src = session.source.clone();
        let stats = session.stats.clone();
        let stop = session.stop.clone();
        session.reader = Some(std::thread::spawn(move || {
            if let Err(e) = read_capture(stdout, p, src, stats.clone(), stop.clone(), o, native) {
                if !stop.load(Ordering::Relaxed) {
                    stats.lock().unwrap().error = Some(e);
                }
            }
        }));
        Ok(session)
    }
    fn negotiate(&mut self) {
        if self.offering
            || self.closed
            || self
                .pipeline
                .by_name("pay")
                .unwrap()
                .static_pad("src")
                .unwrap()
                .current_caps()
                .is_none()
        {
            return;
        }
        self.offering = true;
        let peer = self.peer.downgrade();
        let stats = self.stats.clone();
        let promise = gst::Promise::with_change_func(move |r| {
            let result = r
                .map_err(|e| format!("{e:?}"))
                .and_then(|r| r.ok_or("No offer reply".into()))
                .and_then(|r| {
                    r.get::<rtc::WebRTCSessionDescription>("offer")
                        .map_err(|e| e.to_string())
                });
            match result {
                Ok(offer) => {
                    if let Some(p) = peer.upgrade() {
                        p.emit_by_name::<()>(
                            "set-local-description",
                            &[&offer, &None::<gst::Promise>],
                        );
                        match offer.sdp().as_text() {
                            Ok(s) => {
                                stats.lock().unwrap().offer = Some(json!({"type":"offer","sdp":s}))
                            }
                            Err(e) => stats.lock().unwrap().error = Some(e.to_string()),
                        }
                    }
                }
                Err(e) => stats.lock().unwrap().error = Some(e),
            }
        });
        self.peer
            .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
    }
    fn tick(&mut self, control: &Control) {
        if self.closed {
            return;
        }
        self.negotiate();
        if let Some(bus) = self.pipeline.bus() {
            for m in bus.iter() {
                if let gst::MessageView::Error(e) = m.view() {
                    self.stats.lock().unwrap().error = Some(e.error().to_string());
                }
            }
        }
        let pc = self
            .peer
            .property::<rtc::WebRTCPeerConnectionState>("connection-state");
        let revoked = !control
            .state()
            .is_ok_and(|s| s["enabled"] == true && number(&s["generation"]) == self.generation);
        if revoked
            || stopped()
            || self.last.elapsed() > Duration::from_secs(10)
            || self.stats.lock().unwrap().error.is_some()
            || matches!(
                pc,
                rtc::WebRTCPeerConnectionState::Failed | rtc::WebRTCPeerConnectionState::Closed
            )
        {
            self.close();
        }
    }
    fn state(&self) -> String {
        if self.stats.lock().unwrap().error.is_some() {
            "failed".into()
        } else if self.closed {
            "closed".into()
        } else {
            format!(
                "{:?}",
                self.peer
                    .property::<rtc::WebRTCPeerConnectionState>("connection-state")
            )
            .to_ascii_lowercase()
        }
    }
    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(c) = &mut self.capture {
            stop_child(c, false);
            self.exit = c.try_wait().ok().flatten().and_then(|s| s.code());
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        self.capture = None;
        self.lease = None;
        self.closed = true;
    }
    fn stats(&self) -> Value {
        let state = self.state();
        let s = self.stats.lock().unwrap();
        let timing: serde_json::Map<String, Value> =
            s.timing.iter().map(|(k, v)| (k.clone(), pct(v))).collect();
        let phases: serde_json::Map<String, Value> = s
            .latency
            .iter()
            .map(|(k, v)| {
                let mut p = pct(v);
                p["total_matched"] = json!(s.phase_counts.get(k).copied().unwrap_or(0));
                (k.clone(), p)
            })
            .collect();
        let counts: serde_json::Map<String, Value> = s
            .counts
            .iter()
            .map(|(k, v)| (k.clone(), v.value()))
            .collect();
        json!({"id":self.id,"state":state,"error":s.error,"requested":{"width":self.options.width,"height":self.options.height,"fps":self.options.fps,"bitrate":self.options.bitrate,"codec":self.options.requested,"eye":self.options.eye},"encoder":{"device":self.device,"factory":self.factory,"hardware":"Qualcomm Venus","bitrate_requested":self.options.bitrate,"codec":self.options.codec,"fallback_reason":self.options.fallback,"h264_level":if self.options.codec=="h264"{Some(self.level)}else{None}},"capture":s.capture,"capture_age_ms":age(number(&s.capture["captured_ns"]),now()),"capture_errors":s.capture_errors,"capture_program":self.program,"capture_pipe":{"requested_bytes":1048576,"actual_bytes":self.pipe_bytes,"reader_buffer_bytes":1048576,"payload":if self.native{"compressed access units"}else{"NV12"},"input_memory":if self.native{"GPU DMA-BUF -> Venus"}else{"CPU copy"}},"timing":timing,"counts":counts,"phase_latency":{"clock":"device CLOCK_MONOTONIC","unit":"microseconds","scope":"latest up to 300 measured samples per stage; exact PTS match","capture_complete":"GPU/CPU readback completed, before stdout delivery","encoder_output":if self.native{"native appsink entry after hardware encoder and parser"}else{"parser src VCL probe"},"native_encoder":self.native,"end_to_end":false,"pending_pts":s.pending.len(),"pending_limit":1024,"errors":s.phase_errors,"stages":phases},"encoded_pts_unmatched":s.unmatched,"appsrc_flow_errors":s.flow_errors,"appsrc_dropped_confirmed":if s.missing_ordinal==0{Some(if self.native{0}else{s.dropped})}else{None},"appsrc_drop_scope":"source ordinals skipped before later encoder input; pending tail excluded","encoder_missing_source_ordinal":s.missing_ordinal,"appsrc_queued_buffers":self.source.current_level_buffers(),"rtp":s.rtp,"capture_exit":self.exit,"atomic_snapshot":false,"scope":"real readbacks; encoded frames; not physical presentation","gstreamer":gst::version_string().to_string(),"backend":"rust"})
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}
fn read_capture(
    stdout: ChildStdout,
    pipeline: gst::Pipeline,
    source: app::AppSrc,
    stats: Arc<Mutex<Stats>>,
    stop: Arc<AtomicBool>,
    o: Options,
    native: bool,
) -> Result<(), String> {
    let mut input = BufReader::with_capacity(
        1048576,
        InterruptRead {
            pipe: stdout,
            stop: stop.clone(),
        },
    );
    while !stop.load(Ordering::Relaxed) && !stopped() {
        let mut line = vec![];
        input
            .by_ref()
            .take(8193)
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if line.is_empty() || line.len() > 8192 || !line.ends_with(b"\n") {
            return Err("Capture metadata missing or too long".into());
        }
        let meta: Value = serde_json::from_slice(&line).map_err(|e| e.to_string())?;
        let expected = o.width * o.height * 3 / 2;
        let size = meta["length"].as_u64().ok_or("Capture length missing")?;
        if size != 0 && size != expected {
            return Err("Unexpected NV12 size".into());
        }
        let size = if native {
            if meta["native_encoder"] != true || meta["codec"] != o.codec {
                return Err("Invalid native encoder metadata".into());
            }
            meta["native_payload_length"]
                .as_u64()
                .filter(|s| *s > 0 && *s <= 16 * 1024 * 1024)
                .ok_or("Invalid native access unit length")?
        } else {
            size
        };
        let mut payload = vec![0; size as usize];
        input.read_exact(&mut payload).map_err(|e| e.to_string())?;
        let read_done = now();
        {
            let mut s = stats.lock().unwrap();
            s.capture = meta.clone();
            if let Some(error) = meta["capture_error"].as_str() {
                *s.capture_errors.entry(error.into()).or_default() += 1;
            }
            for key in [
                "capture_us",
                "acquire_us",
                "convert_us",
                "gpu_import_us",
                "gpu_dispatch_us",
                "gpu_readback_us",
            ] {
                if let Some(v) = meta[key].as_f64() {
                    sample(&mut s.timing, key, v);
                }
            }
        }
        if size == 0 {
            continue;
        }
        if meta["format"] != "NV12"
            || meta["colorimetry"] != "bt601"
            || number(&meta["width"]) != o.width
            || number(&meta["height"]) != o.height
        {
            return Err("Unexpected capture format".into());
        }
        let captured = number(&meta["captured_ns"]);
        let t = now();
        let Some(running) = pipeline.current_running_time().map(|t| t.nseconds()) else {
            continue;
        };
        let Some(age) = t.checked_sub(captured) else {
            return Err("Future capture timestamp".into());
        };
        let Some(pts) = running.checked_sub(age) else {
            continue;
        };
        let mut b = gst::Buffer::from_mut_slice(payload);
        {
            let b = b.get_mut().unwrap();
            b.set_pts(gst::ClockTime::from_nseconds(pts));
            b.set_dts(None);
            b.set_duration(None);
            let mut s = stats.lock().unwrap();
            s.count("readbacks_delivered", expected, pts, captured);
            b.set_offset(s.counts["readbacks_delivered"].frames);
            b.set_offset_end(s.counts["readbacks_delivered"].frames + 1);
            if captured > 0 && captured <= read_done {
                s.phase("capture_complete_to_reader_us", captured, read_done);
                if native {
                    let begin = number(&meta["encoder_input_ns"]);
                    let end = number(&meta["encoder_output_ns"]);
                    if !(captured <= begin && begin <= end && end <= read_done) {
                        return Err("Invalid native timestamp order".into());
                    }
                    s.phase("encoder_input_to_output_us", begin, end);
                    s.phase("capture_complete_to_encoder_output_us", captured, end);
                    s.phase("encoded_output_to_reader_us", end, read_done);
                    s.count("encoder_input", expected, pts, begin);
                    s.counts.get_mut("encoder_input").unwrap().caps =
                        Some("NV12 / DMA-BUF imported by native Venus".into());
                    s.encoder_pts.insert(pts);
                }
                if s.pending
                    .insert(
                        pts,
                        json!({"captured_ns":captured,"read_done_ns":read_done}),
                    )
                    .is_some()
                {
                    *s.phase_errors
                        .entry("duplicate_source_pts".into())
                        .or_default() += 1;
                    s.pending.remove(&pts);
                }
                if s.pending.len() > 1024 {
                    s.pending.pop_first();
                    *s.phase_errors
                        .entry("pending_pts_evicted".into())
                        .or_default() += 1;
                }
            } else {
                *s.phase_errors
                    .entry("capture_timestamp_invalid".into())
                    .or_default() += 1;
            }
        }
        let result = source.push_buffer(b);
        let mut s = stats.lock().unwrap();
        if result.is_ok() {
            s.count("appsrc_submitted", size, pts, now());
        } else {
            s.flow_errors += 1;
            if !stop.load(Ordering::Relaxed) {
                return Err(format!("Appsrc flow: {result:?}"));
            }
        }
    }
    Ok(())
}
pub struct Manager {
    session: Mutex<Option<Session>>,
    control: Control,
    program: PathBuf,
}
impl Manager {
    pub fn new(control: Control, program: PathBuf) -> Arc<Self> {
        let manager = Arc::new(Self {
            session: Mutex::new(None),
            control,
            program,
        });
        let m = manager.clone();
        std::thread::spawn(move || {
            while !stopped() {
                if let Some(s) = &mut *m.session.lock().unwrap() {
                    s.tick(&m.control);
                }
                pause(100);
            }
            m.close();
        });
        manager
    }
    pub fn close(&self) {
        if let Some(s) = &mut *self.session.lock().unwrap() {
            s.close();
        }
    }
    fn create(&self, options: Options) -> Result<Session, String> {
        let (lock, lease, generation) = self
            .control
            .reserve("screen", options.width, options.height, options.fps)
            .map_err(|e| e.to_string())?;
        let mut s = Session::new(options, self.program.clone(), lease, generation)?;
        drop(lock);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(8) {
            s.tick(&self.control);
            if s.closed {
                return Err(s
                    .stats
                    .lock()
                    .unwrap()
                    .error
                    .clone()
                    .unwrap_or("Video stopped during startup".into()));
            }
            if s.stats.lock().unwrap().offer.is_some() {
                return Ok(s);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err("Offer timeout".into())
    }
    pub fn api(&self, method: &str, path: &str, v: Value) -> Result<(u16, Value), String> {
        if method == "POST" && path == "/api/stop" {
            self.close();
            return Ok((200, json!({"closed":true})));
        }
        let mut slot = self.session.lock().unwrap();
        if method == "POST" && path == "/api/session" {
            if slot.as_ref().is_some_and(|s| !s.closed) {
                return Ok((
                    409,
                    json!({"error":"One viewer already owns the video session"}),
                ));
            }
            let mut options = Options::parse(&v)?;
            let result = self.create(options.clone());
            let s = match result {
                Ok(s) => s,
                Err(e) if options.codec == "h265" => {
                    options.codec = "h264".into();
                    options.fallback = Some("hardware_hevc_unavailable".into());
                    self.create(options)
                        .map_err(|e2| format!("{e}; H264: {e2}"))?
                }
                Err(e) => return Err(e),
            };
            let body = json!({"id":s.id,"offer":s.stats.lock().unwrap().offer,"codec":s.options.codec,"fallback_reason":s.options.fallback,"eye":s.options.eye});
            *slot = Some(s);
            return Ok((201, body));
        }
        let Some(s) = &mut *slot else {
            return Ok((404, json!({"error":"No such session"})));
        };
        let prefix = format!("/api/session/{}", s.id);
        let Some(route) = path.strip_prefix(&prefix) else {
            return Ok((404, json!({"error":"No such session"})));
        };
        s.last = Instant::now();
        match (method, route) {
            ("DELETE", "") => {
                s.close();
                Ok((204, Value::Null))
            }
            ("GET", "/stats") => Ok((200, s.stats())),
            ("GET", "/ice") => {
                let state = s.state();
                Ok((
                    200,
                    json!({"candidates":s.stats.lock().unwrap().ice,"state":state}),
                ))
            }
            _ if s.closed => Ok((410, json!({"error":"Session closed"}))),
            ("POST", "/answer") => {
                let text = v["sdp"]
                    .as_str()
                    .filter(|s| s.len() <= 200000)
                    .ok_or("Invalid SDP answer")?;
                if v["type"] != "answer" {
                    return Err("Invalid SDP answer type".into());
                }
                let msg =
                    sdp::SDPMessage::parse_buffer(text.as_bytes()).map_err(|e| e.to_string())?;
                let answer = rtc::WebRTCSessionDescription::new(rtc::WebRTCSDPType::Answer, msg);
                s.peer.emit_by_name::<()>(
                    "set-remote-description",
                    &[&answer, &None::<gst::Promise>],
                );
                Ok((204, Value::Null))
            }
            ("POST", "/ice") => {
                let line = v["sdpMLineIndex"]
                    .as_u64()
                    .filter(|i| *i < 4)
                    .ok_or("Invalid ICE line")? as u32;
                let candidate = v["candidate"]
                    .as_str()
                    .filter(|s| s.len() <= 4096)
                    .ok_or("Invalid ICE candidate")?;
                s.peer
                    .emit_by_name::<()>("add-ice-candidate", &[&line, &candidate]);
                Ok((204, Value::Null))
            }
            _ => Ok((404, json!({"error":"Unknown endpoint"}))),
        }
    }
}
