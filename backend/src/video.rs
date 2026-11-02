//! One native NV12 / Venus H.264 / WebRTC pipeline for each verified phase.
use super::*;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as app;
use gstreamer_sdp as sdp;
use gstreamer_webrtc as rtc;
use serde_json::Value;
use std::{collections::BTreeMap, fs::File, os::fd::FromRawFd, sync::OnceLock};

#[derive(Default)]
struct Counters {
    offered: Option<String>,
    ice: Vec<Value>,
    error: Option<String>,
    submitted: u64,
    encoded: u64,
    packets: u64,
    bytes: u64,
    pending: BTreeMap<u64, Value>,
    recent: VecDeque<Value>,
    dq_ms: VecDeque<f64>,
    sof_ms: VecDeque<f64>,
    encode_ms: VecDeque<f64>,
    last_encoded_ns: u64,
}
fn sample(values: &mut VecDeque<f64>, v: f64) {
    if values.len() == 300 {
        values.pop_front();
    }
    values.push_back(v);
}
fn percentiles(values: &VecDeque<f64>) -> Value {
    let mut v: Vec<f64> = values.iter().copied().collect();
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return Value::Null;
    }
    json!({"samples":v.len(), "p50":v[(v.len()-1)/2], "p95":v[(v.len()-1)*95/100]})
}
struct Session {
    id: String,
    phase: usize,
    fps: u32,
    bitrate: u32,
    generation: u64,
    pipeline: gst::Pipeline,
    source: app::AppSrc,
    rtc: gst::Element,
    counters: Arc<Mutex<Counters>>,
    _lease: File,
    created: Instant,
    poll: Instant,
    last_push: Instant,
    last_group: [u64; 4],
    synced: u64,
    sync_rejected: u64,
    last_candidate: [u64; 4],
    offering: bool,
    next_sof: u64,
}
impl Session {
    fn new(phase: usize, fps: u32, bitrate: u32, fd: i32, generation: u64) -> Result<Self, String> {
        static INIT: OnceLock<Result<(), String>> = OnceLock::new();
        INIT.get_or_init(|| {
            gst::init().map_err(|e| e.to_string())?;
            std::thread::spawn(|| gst::glib::MainLoop::new(None, false).run());
            Ok(())
        })
        .clone()?;
        let lease_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if lease_fd < 0 {
            return Err("invalid_encoder_lease".into());
        }
        let lease = unsafe { File::from_raw_fd(lease_fd) };
        let mut factory = None;
        for feature in gst::Registry::get().features(gst::ElementFactory::static_type()) {
            let name = feature.name();
            if !name.starts_with("v4l2") || !name.ends_with("h264enc") {
                continue;
            }
            let Ok(element) = gst::ElementFactory::make(name.as_str()).build() else {
                continue;
            };
            let device = element.property::<String>("device");
            let Some(node) = std::path::Path::new(&device).file_name() else {
                continue;
            };
            if std::fs::read_to_string(format!(
                "/sys/class/video4linux/{}/name",
                node.to_string_lossy()
            ))
            .unwrap_or_default()
            .trim()
                == "qcom-venus-encoder"
            {
                factory = Some(name.to_string());
                break;
            }
        }
        let factory = factory.ok_or("venus_h264_encoder_unavailable")?;
        let description = format!("appsrc name=source is-live=true format=time block=false max-buffers=2 max-bytes=0 max-time=0 leaky-type=downstream ! {factory} name=encoder ! h264parse name=parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! rtph264pay name=pay pt=96 config-interval=-1 aggregate-mode=zero-latency ! application/x-rtp,media=video,encoding-name=H264,payload=96,clock-rate=90000 ! webrtcbin name=rtc bundle-policy=max-bundle");
        let pipeline = gst::parse::launch(&description)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "invalid_pipeline")?;
        let source = pipeline
            .by_name("source")
            .unwrap()
            .downcast::<app::AppSrc>()
            .unwrap();
        source.set_caps(Some(
            &gst::Caps::builder("video/x-raw")
                .field("format", "NV12")
                .field("width", 640i32)
                .field("height", 480i32)
                .field("framerate", gst::Fraction::new(fps as i32, 1))
                .field("colorimetry", "bt601")
                .field("interlace-mode", "progressive")
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .build(),
        ));
        pipeline.by_name("encoder").unwrap().set_property(
            "extra-controls",
            gst::Structure::builder("controls")
                .field("video_bitrate", bitrate as i32)
                .field("h264_profile", 1i32)
                .field("h264_level", 13i32)
                .field("video_gop_size", fps as i32)
                .build(),
        );
        let rtc = pipeline.by_name("rtc").unwrap();
        if let Some(transceiver) =
            rtc.emit_by_name::<Option<rtc::WebRTCRTPTransceiver>>("get-transceiver", &[&0i32])
        {
            transceiver.set_property("direction", rtc::WebRTCRTPTransceiverDirection::Sendonly);
        }
        let counts = Arc::new(Mutex::new(Counters::default()));
        let c = counts.clone();
        rtc.connect("on-ice-candidate", false, move |values| {
            if let (Ok(line), Ok(candidate)) = (values[1].get::<u32>(), values[2].get::<String>()) {
                let mut c = c.lock().unwrap();
                if c.ice.len() < 128 {
                    c.ice
                        .push(json!({"sdpMLineIndex":line,"candidate":candidate}));
                }
            }
            None
        });
        let c = counts.clone();
        pipeline
            .by_name("parse")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data {
                    if let (Some(pts), Ok(map)) = (buffer.pts(), buffer.map_readable()) {
                        if map
                            .as_slice()
                            .windows(4)
                            .any(|w| w[..3] == [0, 0, 1] && (1..=5).contains(&(w[3] & 31)))
                        {
                            let now = mono_ns();
                            let mut c = c.lock().unwrap();
                            c.encoded += 1;
                            c.last_encoded_ns = now;
                            if let Some(meta) = c.pending.get(&pts.nseconds()).cloned() {
                                sample(
                                    &mut c.dq_ms,
                                    (now.saturating_sub(meta["dq_ns"].as_u64().unwrap())) as f64
                                        / 1e6,
                                );
                                sample(
                                    &mut c.sof_ms,
                                    (now.saturating_sub(meta["oldest_sof_ns"].as_u64().unwrap()))
                                        as f64
                                        / 1e6,
                                );
                                sample(
                                    &mut c.encode_ms,
                                    (now.saturating_sub(meta["submit_ns"].as_u64().unwrap()))
                                        as f64
                                        / 1e6,
                                );
                            }
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });
        let c = counts.clone();
        pipeline
            .by_name("pay")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                move |_, info| {
                    let record = |b: &gst::BufferRef| {
                        if let Ok(m) = b.map_readable() {
                            let p = m.as_slice();
                            if p.len() < 12 || p[0] >> 6 != 2 {
                                return;
                            }
                            let mut c = c.lock().unwrap();
                            c.packets += 1;
                            c.bytes += p.len() as u64;
                            if p[1] & 128 != 0 {
                                if let Some(mut meta) = b
                                    .pts()
                                    .and_then(|pts| c.pending.get(&pts.nseconds()))
                                    .cloned()
                                {
                                    meta["rtp_timestamp"] =
                                        json!(u32::from_be_bytes(p[4..8].try_into().unwrap()));
                                    if c.recent.len() == 96 {
                                        c.recent.pop_front();
                                    }
                                    c.recent.push_back(meta);
                                }
                            }
                        }
                    };
                    match info.data {
                        Some(gst::PadProbeData::Buffer(ref b)) => record(b),
                        Some(gst::PadProbeData::BufferList(ref list)) => {
                            for b in list.iter() {
                                record(b);
                            }
                        }
                        _ => {}
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        // Drop always transitions to NULL before releasing the cross-process lease.
        let session = Self {
            id: format!("p{phase}-{:016x}", mono_ns()),
            phase,
            fps,
            bitrate,
            generation,
            pipeline,
            source,
            rtc,
            counters: counts.clone(),
            _lease: lease,
            created: Instant::now(),
            poll: Instant::now(),
            last_push: Instant::now() - Duration::from_secs(1),
            last_group: [0; 4],
            synced: 0,
            sync_rejected: 0,
            last_candidate: [0; 4],
            offering: false,
            next_sof: 0,
        };
        session
            .pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| e.to_string())?;
        Ok(session)
    }
    fn negotiate(&mut self) {
        if self.offering
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
        let r = self.rtc.downgrade();
        let counts = self.counters.clone();
        let promise = gst::Promise::with_change_func(move |reply| match reply {
            Ok(Some(reply)) => match reply.get::<rtc::WebRTCSessionDescription>("offer") {
                Ok(offer) => {
                    if let Some(r) = r.upgrade() {
                        r.emit_by_name::<()>(
                            "set-local-description",
                            &[&offer, &None::<gst::Promise>],
                        );
                        match offer.sdp().as_text() {
                            Ok(s) => counts.lock().unwrap().offered = Some(s),
                            Err(e) => counts.lock().unwrap().error = Some(e.to_string()),
                        }
                    }
                }
                Err(e) => counts.lock().unwrap().error = Some(e.to_string()),
            },
            _ => counts.lock().unwrap().error = Some("offer_failed".into()),
        });
        self.rtc
            .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
    }
    fn tick(&mut self, c: &Camera) -> Option<String> {
        self.negotiate();
        if self.poll.elapsed() > Duration::from_secs(10) {
            return Some("viewer_expired".into());
        }
        if matches!(
            self.rtc
                .property::<rtc::WebRTCPeerConnectionState>("connection-state"),
            rtc::WebRTCPeerConnectionState::Failed | rtc::WebRTCPeerConnectionState::Closed
        ) {
            return Some("peer_connection_closed".into());
        }
        if let Some(bus) = self.pipeline.bus() {
            for message in bus.iter() {
                match message.view() {
                    gst::MessageView::Error(e) => return Some(e.error().to_string()),
                    gst::MessageView::Eos(_) => return Some("end_of_stream".into()),
                    _ => {}
                }
            }
        }
        if let Some(e) = self.counters.lock().unwrap().error.clone() {
            return Some(e);
        }
        if self.last_push.elapsed() > Duration::from_secs(8) {
            return Some("verified_phase_source_timeout".into());
        }
        let (packet, metadata, stamps) = {
            let state = c.inner.lock().unwrap();
            let f: Vec<&Frame> = (0..4)
                .filter_map(|slot| state.frames[self.phase * 4 + slot].as_ref())
                .collect();
            if f.len() != 4
                || f.iter().any(|f| {
                    !fresh(f, mono_ns()) || f.width != 320 || f.height != 240 || f.flags & 3 != 3
                })
            {
                return None;
            }
            let stamps = std::array::from_fn(|i| f[i].sof_ns);
            if stamps == self.last_candidate {
                return None;
            }
            self.last_candidate = stamps;
            let min = *stamps.iter().min().unwrap();
            let max = *stamps.iter().max().unwrap();
            if max - min > 1_000_000 || (0..4).any(|i| stamps[i] <= self.last_group[i]) {
                self.sync_rejected += 1;
                return None;
            }
            // Anchors are per sensor; equal anchors across sensors are NOT required.
            self.synced += 1;
            if min < self.next_sof {
                return None;
            }
            let period = 1_000_000_000 / self.fps as u64;
            // Allow up to one period of producer jitter without a catch-up burst.
            self.next_sof = self
                .next_sof
                .max(min.saturating_sub(period))
                .saturating_add(period);
            let mut nv12 = vec![128u8; 640 * 480 * 3 / 2];
            for slot in 0..4 {
                for y in 0..240 {
                    let dst = (slot / 2 * 240 + y) * 640 + slot % 2 * 320;
                    nv12[dst..dst + 320]
                        .copy_from_slice(&f[slot].packet[104 + y * 320..104 + (y + 1) * 320]);
                }
            }
            let meta = json!({"dq_ns":f.iter().map(|x|x.timestamp).min().unwrap(),"oldest_sof_ns":min,
                "sof_spread_ns":max-min,"phase":self.phase,"submit_ns":mono_ns(),
                "cameras":f.iter().map(|x|json!({"session":x.session,"sequence":x.sequence,"sensor_sequence":x.sensor_sequence,"sof_ns":x.sof_ns,"exposure_ns":x.exposure_ns})).collect::<Vec<_>>()});
            (nv12, meta, stamps)
        };
        let running = self.pipeline.current_running_time()?.nseconds();
        let age = mono_ns().saturating_sub(metadata["dq_ns"].as_u64().unwrap());
        let pts = running.saturating_sub(age);
        let mut buffer = gst::Buffer::from_mut_slice(packet);
        {
            let b = buffer.get_mut().unwrap();
            b.set_pts(gst::ClockTime::from_nseconds(pts));
            b.set_duration(gst::ClockTime::from_nseconds(
                1_000_000_000 / self.fps as u64,
            ));
        }
        {
            let mut stats = self.counters.lock().unwrap();
            stats.pending.insert(pts, metadata);
            while stats.pending.len() > 128 {
                stats.pending.pop_first();
            }
        }
        if let Err(e) = self.source.push_buffer(buffer) {
            return Some(e.to_string());
        }
        self.counters.lock().unwrap().submitted += 1;
        self.last_group = stamps;
        self.last_push = Instant::now();
        None
    }
    fn status(&self) -> Value {
        let c = self.counters.lock().unwrap();
        json!({"id":self.id,"phase":self.phase,"state":"running","offer":c.offered,"ice":c.ice,"error":c.error,
            "width":640,"height":480,"requested_fps":self.fps,"bitrate":self.bitrate,"age_seconds":self.created.elapsed().as_secs_f64(),
            "synced_groups":self.synced,"sync_rejected_candidates":self.sync_rejected,"submitted":c.submitted,"encoded":c.encoded,"rtp_packets":c.packets,"rtp_bytes":c.bytes,
            "last_encoded_ns":c.last_encoded_ns,"stale":mono_ns().saturating_sub(c.last_encoded_ns)>MAX_AGE,
            "dq_to_encoded_ms":percentiles(&c.dq_ms),"oldest_sof_to_encoded_ms":percentiles(&c.sof_ms),"submit_to_encoded_ms":percentiles(&c.encode_ms),
            "rtp_sources":c.recent,"queued_buffers":self.source.current_level_buffers()})
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
#[derive(Default)]
pub struct Video {
    sessions: [Option<Session>; 3],
    last: [Option<Value>; 3],
    control_at: Option<Instant>,
    control: Option<Value>,
}
impl Video {
    pub fn active(&self) -> bool {
        self.sessions.iter().any(|s| s.is_some())
    }
    pub fn tick(&mut self, c: &Camera) {
        if self.sessions.iter().all(|s| s.is_none()) {
            return;
        }
        if self
            .control_at
            .is_none_or(|t| t.elapsed() > Duration::from_millis(100))
        {
            self.control = Some(match std::fs::read("/run/mix-video-control/state.json") {
                Ok(b) => serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    json!({"enabled":true,"generation":0})
                }
                Err(_) => Value::Null,
            });
            self.control_at = Some(Instant::now());
        }
        let mut active = false;
        for phase in 0..3 {
            if let Some(s) = &mut self.sessions[phase] {
                let revoked = self.control.as_ref().is_some_and(|v| {
                    v["enabled"] != true || v["generation"].as_u64() != Some(s.generation)
                });
                let error = if c.stop.load(Ordering::Relaxed) || revoked {
                    Some("manual_stop_or_generation_changed".into())
                } else {
                    s.tick(c)
                };
                if let Some(error) = error {
                    let mut status = s.status();
                    status["state"] = json!("closed");
                    status["error"] = json!(error);
                    self.last[phase] = Some(status);
                    self.sessions[phase] = None;
                } else {
                    active = true;
                }
            }
        }
        if active {
            c.inner.lock().unwrap().last_request = Some(Instant::now());
            c.changed.notify_all();
        }
    }
    pub fn api(
        &mut self,
        c: &Camera,
        method: &str,
        path: &str,
        body: Value,
        fd: i32,
        generation: u64,
    ) -> Result<Value, String> {
        let path = path.trim_start_matches('/');
        let parts: Vec<_> = path.split('/').collect();
        if method == "GET" && path.is_empty() {
            return Ok(
                json!({"available":true,"phases":(0..3).map(|p|self.sessions[p].as_ref().map(|s|json!({"id":s.id,"phase":p,"state":"running"})).or_else(||self.last[p].as_ref().map(|s|json!({"id":s["id"],"phase":p,"state":"closed","error":s["error"]})))).collect::<Vec<_>>()}),
            );
        }
        if method == "DELETE" && path.is_empty() {
            for s in &mut self.sessions {
                *s = None;
            }
            return Ok(json!({"closed":true}));
        }
        if method == "POST" && path.is_empty() {
            if !c.live {
                return Err("live_video_disabled_in_offline_mode".into());
            }
            let phase = body["phase"].as_u64().ok_or("invalid_phase")? as usize;
            let fps = body["fps"].as_u64().unwrap_or(30) as u32;
            let bitrate = body["bitrate"].as_u64().unwrap_or(3_000_000) as u32;
            if phase > 2
                || ![15, 25, 30].contains(&fps)
                || !(1_000_000..=8_000_000).contains(&bitrate)
            {
                return Err("invalid_video_parameters".into());
            }
            if self.sessions[phase].is_some() {
                return Err("phase_already_owned".into());
            }
            let s = Session::new(phase, fps, bitrate, fd, generation)?;
            let result = s.status();
            self.sessions[phase] = Some(s);
            self.control_at = None;
            c.inner.lock().unwrap().last_request = Some(Instant::now());
            c.changed.notify_all();
            return Ok(result);
        }
        let phase = (0..3)
            .find(|&p| self.sessions[p].as_ref().is_some_and(|s| s.id == parts[0]))
            .ok_or("session_not_found")?;
        if method == "DELETE" && parts.len() == 1 {
            self.sessions[phase] = None;
            return Ok(json!({"closed":true}));
        }
        let s = self.sessions[phase].as_mut().unwrap();
        s.poll = Instant::now();
        if method == "GET" && parts.len() == 1 {
            return Ok(s.status());
        }
        if method == "POST" && parts.len() == 2 {
            match parts[1] {
                "answer" => {
                    let text = body["sdp"].as_str().ok_or("missing_sdp")?;
                    let msg = sdp::SDPMessage::parse_buffer(text.as_bytes())
                        .map_err(|e| e.to_string())?;
                    let answer =
                        rtc::WebRTCSessionDescription::new(rtc::WebRTCSDPType::Answer, msg);
                    s.rtc.emit_by_name::<()>(
                        "set-remote-description",
                        &[&answer, &None::<gst::Promise>],
                    );
                }
                "ice" => {
                    let line = body["sdpMLineIndex"]
                        .as_u64()
                        .filter(|x| *x < 16)
                        .ok_or("invalid_ice_line")? as u32;
                    let candidate = body["candidate"]
                        .as_str()
                        .filter(|x| x.len() < 4096)
                        .ok_or("invalid_ice")?;
                    s.rtc
                        .emit_by_name::<()>("add-ice-candidate", &[&line, &candidate]);
                }
                _ => return Err("unknown_video_route".into()),
            }
            return Ok(json!({"ok":true}));
        }
        Err("unknown_video_route".into())
    }
}
