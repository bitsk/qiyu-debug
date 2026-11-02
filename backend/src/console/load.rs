use super::{telemetry::Telemetry, util::*};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    process::{Command, Stdio},
    sync::{atomic::Ordering, Arc},
};
type Sequence = unsafe extern "C" fn(i32, u32, *mut u64, *mut u64) -> i32;
type Master = unsafe extern "C" fn(i32) -> i32;
#[derive(Default)]
pub struct Load {
    previous_cpu: Option<(u64, u64)>,
    previous_gpu: Option<HashMap<String, Value>>,
    previous_display: Option<(String, u32, u64, u64)>,
    drm: Option<(usize, Sequence, Master)>,
}
fn num(path: impl AsRef<Path>) -> Option<u64> {
    text(path).parse().ok()
}
impl Load {
    pub fn system(&mut self, live: bool) -> Value {
        let mem: HashMap<_, _> = text("/proc/meminfo")
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once(':')?;
                Some((
                    k.to_string(),
                    v.split_whitespace().next()?.parse::<u64>().ok()?,
                ))
            })
            .collect();
        let mut temperatures = vec![];
        for p in entries("/sys/class/thermal") {
            if p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("thermal_zone")
            {
                if let Some(t) = num(p.join("temp")) {
                    temperatures
                        .push(json!({"name":text(p.join("type")),"celsius":t as f64/1000.0}));
                }
            }
        }
        let mut frequencies = vec![];
        let mut cooling = vec![];
        if live {
            for p in entries("/sys/devices/system/cpu/cpufreq") {
                if let (Some(current), Some(limit), Some(max)) = (
                    num(p.join("scaling_cur_freq")),
                    num(p.join("scaling_max_freq")),
                    num(p.join("cpuinfo_max_freq")),
                ) {
                    frequencies.push(json!({"name":p.file_name().unwrap().to_string_lossy(),"current_mhz":current as f64/1000.0,"limit_mhz":limit as f64/1000.0,"hardware_max_mhz":max as f64/1000.0}));
                }
            }
            for p in entries("/sys/class/devfreq") {
                if p.file_name().unwrap().to_string_lossy().ends_with(".gpu") {
                    frequencies.push(json!({"name":"GPU","current_mhz":num(p.join("cur_freq")).map(|v|v as f64/1e6),"limit_mhz":num(p.join("max_freq")).map(|v|v as f64/1e6),"hardware_max_mhz":text(p.join("available_frequencies")).split_whitespace().filter_map(|x|x.parse::<u64>().ok()).max().map(|v|v as f64/1e6)}));
                }
            }
            for p in entries("/sys/class/thermal") {
                if p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("cooling_device")
                {
                    if let (Some(cur), Some(max)) =
                        (num(p.join("cur_state")), num(p.join("max_state")))
                    {
                        cooling
                            .push(json!({"name":text(p.join("type")),"state":cur,"max_state":max}));
                    }
                }
            }
        }
        let total = mem.get("MemTotal").copied().unwrap_or(0);
        let avail = mem.get("MemAvailable").copied().unwrap_or(0);
        json!({"scope":if live{"headset"}else{"development-host"},"kernel":text("/proc/sys/kernel/osrelease"),"load":text("/proc/loadavg").split_whitespace().take(3).filter_map(|x|x.parse::<f64>().ok()).collect::<Vec<_>>(),"memory_used_mb":total.saturating_sub(avail)as f64/1024.0,"memory_total_mb":total as f64/1024.0,"temperatures":temperatures,"frequencies":frequencies,"cooling":cooling,"device_load":if live{self.sample()}else{json!({"available":false,"scope":"offline"})}})
    }
    fn sample(&mut self) -> Value {
        let values: Vec<_> = text("/proc/stat")
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .skip(1)
            .take(8)
            .filter_map(|x| x.parse::<u64>().ok())
            .collect();
        let mut cpu = None;
        if values.len() == 8 {
            let total = values.iter().sum();
            let idle = values[3] + values[4];
            if let Some((a, b)) = self.previous_cpu {
                if total > a && idle >= b {
                    cpu = Some(100.0 * (1.0 - (idle - b) as f64 / (total - a) as f64));
                }
            }
            self.previous_cpu = Some((total, idle));
        }
        let mut pressure = json!({});
        for name in ["cpu", "memory", "io"] {
            for line in text(format!("/proc/pressure/{name}")).lines() {
                let mut words = line.split_whitespace();
                if let Some(kind) = words.next() {
                    for p in words {
                        if let Some(v) =
                            p.strip_prefix("avg10=").and_then(|v| v.parse::<f64>().ok())
                        {
                            pressure[format!("{name}_{kind}_avg10")] = json!(v);
                        }
                    }
                }
            }
        }
        let clocks:Vec<_>=["video_cc_mvs0_clk","video_cc_mvs1_div2_clk"].iter().filter_map(|name|{let p=Path::new("/sys/kernel/debug/clk").join(name);Some(json!({"name":name,"rate_mhz":num(p.join("clk_rate"))?as f64/1e6,"enabled":num(p.join("clk_enable_count"))?>0}))}).collect();
        json!({"sampled_monotonic_ns":now().to_string(),"display":self.display(),"gpu":self.gpu(),"vpu":{"hardware_busy_percent":null,"reason":"Venus exposes no hardware busy-time counter in this kernel","clocks":clocks},"pressure":pressure,"cpu_percent":cpu})
    }
    fn display(&mut self) -> Value {
        let mut out = json!({"source":"DRM CRTC sequence/timestamp","scan_hz":null,"mode_hz":null});
        if self.drm.is_none() {
            unsafe {
                let h = libc::dlopen(c"libdrm.so.2".as_ptr(), libc::RTLD_NOW);
                if !h.is_null() {
                    let s = libc::dlsym(h, c"drmCrtcGetSequence".as_ptr());
                    let m = libc::dlsym(h, c"drmIsMaster".as_ptr());
                    if !s.is_null() && !m.is_null() {
                        self.drm = Some((
                            h as usize,
                            std::mem::transmute::<*mut libc::c_void, Sequence>(s),
                            std::mem::transmute::<*mut libc::c_void, Master>(m),
                        ));
                    } else {
                        libc::dlclose(h);
                    }
                }
            }
        }
        let Some((_, seq_fn, master)) = self.drm else {
            return out;
        };
        for p in entries("/sys/kernel/debug/dri") {
            if !text(p.join("name")).starts_with("msm-kms ") {
                continue;
            }
            if !text(p.join("clients"))
                .lines()
                .any(|l| l.split_whitespace().nth(3) == Some("y"))
            {
                continue;
            }
            let state = text(p.join("state"));
            let lines: Vec<_> = state.lines().collect();
            for (i, l) in lines.iter().enumerate() {
                let Some(id) = l
                    .strip_prefix("crtc[")
                    .and_then(|s| s.split_once(']'))
                    .and_then(|(s, _)| s.parse::<u32>().ok())
                else {
                    continue;
                };
                let block: Vec<_> = lines[i + 1..]
                    .iter()
                    .take_while(|l| !l.starts_with("crtc[") && !l.starts_with("connector["))
                    .collect();
                if !block.iter().any(|l| l.trim() == "active=1") {
                    continue;
                }
                let card = p.file_name().unwrap().to_string_lossy().to_string();
                let Ok(fd) = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                    .open(format!("/dev/dri/card{card}"))
                else {
                    continue;
                };
                let (mut seq, mut stamp) = (0, 0);
                unsafe {
                    if master(fd.as_raw_fd()) != 0
                        || seq_fn(fd.as_raw_fd(), id, &mut seq, &mut stamp) != 0
                    {
                        continue;
                    }
                }
                out["crtc"] = json!(id);
                out["sequence"] = json!(seq.to_string());
                out["timestamp_ns"] = json!(stamp.to_string());
                out["mode_hz"] = json!(block.iter().find_map(|l| l
                    .trim()
                    .strip_prefix("mode: ")
                    .and_then(|s| s.split_once("\": "))
                    .and_then(|(_, s)| s.split_whitespace().next())
                    .and_then(|s| s.parse::<u64>().ok())));
                if let Some((old_card, old_id, a, b)) = &self.previous_display {
                    if *old_card == card && *old_id == id && seq >= *a && stamp > *b {
                        out["scan_hz"] = json!((seq - *a) as f64 * 1e9 / (stamp - *b) as f64);
                    }
                }
                self.previous_display = Some((card, id, seq, stamp));
                return out;
            }
        }
        self.previous_display = None;
        out
    }
    fn gpu(&mut self) -> Value {
        let mut clients = HashMap::new();
        let mut pids = vec![];
        for p in entries("/sys/kernel/debug/dri") {
            if !text(p.join("name")).starts_with("msm ") {
                continue;
            }
            for line in text(p.join("clients")).lines() {
                if let Some(pid) = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    if !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
        }
        for pid in pids {
            let p = std::path::PathBuf::from(format!("/proc/{pid}"));
            let comm = text(p.join("comm"));
            for fd in entries(p.join("fd")) {
                if !fs::read_link(&fd).is_ok_and(|p| p.starts_with("/dev/dri")) {
                    continue;
                }
                let info = text(p.join("fdinfo").join(fd.file_name().unwrap()));
                let fields: HashMap<_, _> = info
                    .lines()
                    .filter_map(|l| l.split_once(':').map(|(k, v)| (k, v.trim())))
                    .collect();
                if fields.get("drm-driver") != Some(&"msm") {
                    continue;
                }
                if let (Some(id), Some(elapsed)) = (
                    fields.get("drm-client-id"),
                    fields
                        .get("drm-engine-gpu")
                        .and_then(|s| s.split_whitespace().next())
                        .and_then(|s| s.parse::<u64>().ok()),
                ) {
                    clients.entry(id.to_string()).or_insert_with(||json!({"elapsed":elapsed,"role":if comm=="vrcompositor"{"compositor"}else if comm.starts_with("nv12-capture"){"capture"}else{"other"},"pid":pid,"comm":comm,"sampled_ns":now()}));
                }
            }
        }
        let mut out = json!({"total_percent":null,"compositor_percent":null,"capture_percent":null,"other_percent":null,"source":"DRM fdinfo drm-engine-gpu cumulative submitted-work elapsed ns; deduplicated client IDs","scope":"Surviving clients only; includes GPU stalls/preemption, not hardware busy-cycle percentage. Counter resets/new clients start a new baseline.","clients":[]});
        if let Some(prev) = &self.previous_gpu {
            let mut rows = vec![];
            let mut totals = [0.0; 3];
            for (id, v) in &clients {
                if let Some(old) = prev.get(id) {
                    let dt = number(&v["sampled_ns"]).saturating_sub(number(&old["sampled_ns"]));
                    let a = number(&old["elapsed"]);
                    let b = number(&v["elapsed"]);
                    if dt == 0 || dt >= 10_000_000_000 || b < a {
                        continue;
                    }
                    let percent = (b - a) as f64 / dt as f64 * 100.0;
                    let role = match v["role"].as_str() {
                        Some("compositor") => 0,
                        Some("capture") => 1,
                        _ => 2,
                    };
                    totals[role] += percent;
                    let mut row = v.clone();
                    row["client_id"] = json!(id);
                    row["percent"] = json!(percent);
                    rows.push(row);
                }
            }
            if !clients.is_empty() || !prev.is_empty() {
                out["total_percent"] = json!(totals.iter().sum::<f64>());
                for (i, key) in ["compositor_percent", "capture_percent", "other_percent"]
                    .iter()
                    .enumerate()
                {
                    out[*key] = json!(totals[i]);
                }
            }
            out["clients"] = json!(rows);
        }
        self.previous_gpu = Some(clients);
        out
    }
}
pub fn observer(t: Arc<Telemetry>) {
    while !stopped() {
        pause(1000);
        if !fresh(t.requested.load(Ordering::Relaxed), now(), 10_000_000_000) {
            continue;
        }
        let Some(pid) = entries("/proc").into_iter().find(|p| {
            fs::metadata(p.join("comm")).is_ok_and(|m| m.uid() == 1000)
                && text(p.join("comm")) == "vrcompositor"
        }) else {
            continue;
        };
        let Some(s) = session(&t.session_file).filter(|s| s["cleanup_complete"] != true) else {
            continue;
        };
        let dir = s["directory"].as_str().unwrap_or("");
        let valid = [
            "/home/steamos/.local/state/mix-vr-shell-qvr-live/",
            "/home/steamos/.local/state/mix-vr-shell-controller/",
        ]
        .iter()
        .any(|prefix| {
            dir.strip_prefix(prefix)
                .is_some_and(|id| id.starts_with("vr-shell-") && safe_id(id))
        });
        if !valid {
            continue;
        }
        let mut c = Command::new(t.root.join("vr-timing"));
        c.arg("86400")
            .arg(pid.file_name().unwrap())
            .arg(&t.session_file)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LD_LIBRARY_PATH", "/opt/steamvr/bin/linuxarm64")
            .env("XDG_RUNTIME_DIR", "/run/user/1000")
            .env("VR_OVERRIDE", "/opt/steamvr")
            .env("VR_PATHREG_OVERRIDE", format!("{dir}/openvrpaths.vrpath"))
            .env("VR_CONFIG_PATH", format!("{dir}/config"))
            .env("VR_LOG_PATH", "/tmp/mix-openvr-observer")
            .env("STEAMVR_TOOLSDIR", "/opt/steamvr")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        user_child(&mut c);
        if let Ok(mut child) = c.spawn() {
            let mut output = child.stdout.take().unwrap();
            let mut pending = vec![];
            let mut previous = Value::Null;
            while !stopped() && fresh(t.requested.load(Ordering::Relaxed), now(), 10_000_000_000) {
                let Ok(lines) = lines_from(&mut output, &mut pending, 262144) else {
                    break;
                };
                for line in lines {
                    let Some(raw) = line.strip_prefix(b"MIX_TIMING ") else {
                        continue;
                    };
                    let Ok(mut v) = serde_json::from_slice::<Value>(raw) else {
                        continue;
                    };
                    let dt = v["monotonic_seconds"].as_f64().unwrap_or(0.0)
                        - previous["monotonic_seconds"].as_f64().unwrap_or(0.0);
                    let advance =
                        number(&v["frame_index"]).checked_sub(number(&previous["frame_index"]));
                    v["frame_index_fps"] = json!(advance
                        .filter(|_| !previous.is_null()
                            && previous["frame_valid"] == true
                            && v["frame_valid"] == true
                            && dt > 0.0
                            && dt < 3.0)
                        .map(|n| n as f64 / dt));
                    v["application_fps"] = json!(number(&v["submits"])
                        .checked_sub(number(&previous["submits"]))
                        .filter(|_| number(&v["stats_pid"]) > 0
                            && previous["stats_pid"] == v["stats_pid"]
                            && dt > 0.0
                            && dt < 3.0)
                        .map(|n| n as f64 / dt));
                    v["received_monotonic"] = json!(now() as f64 / 1e9);
                    v["compositor_pid"] = json!(pid
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .parse::<u32>()
                        .unwrap_or(0));
                    v["scope"]=json!("OpenVR frame-index advances per observed second; not physical scanout. GPU/CPU times may include scheduling.");
                    if v["frame_valid"] != true || advance == Some(0) {
                        for k in [
                            "compositor_gpu_ms",
                            "compositor_cpu_ms",
                            "total_render_gpu_ms",
                            "present_call_ms",
                            "wait_present_ms",
                        ] {
                            v[k] = Value::Null;
                        }
                    }
                    previous = v.clone();
                    t.data.lock().unwrap().native = v;
                }
            }
            stop_child(&mut child, true);
        }
        t.data.lock().unwrap().native = Value::Null;
    }
}
