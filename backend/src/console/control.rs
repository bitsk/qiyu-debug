use super::util::*;
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::PathBuf,
};
#[derive(Clone)]
pub struct Control {
    pub live: bool,
    path: PathBuf,
}
impl Control {
    pub fn new(live: bool) -> io::Result<Self> {
        let path = PathBuf::from("/run/mix-video-control");
        if live {
            match fs::create_dir(&path) {
                Ok(()) => fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e),
            }
            let m = fs::symlink_metadata(&path)?;
            if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
                return Err(io::Error::other("unsafe video control directory"));
            }
        }
        Ok(Self { live, path })
    }
    fn open(&self, name: &str) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.path.join(name))
    }
    pub fn lock(&self) -> io::Result<File> {
        if !self.live {
            return Err(io::Error::other("video disabled offline"));
        }
        let f = self.open("control.lock")?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(f)
    }
    pub fn state(&self) -> io::Result<Value> {
        if !self.live {
            return Ok(json!({"enabled":false,"generation":0,"limit":4,"offline":true}));
        }
        let p = self.path.join("state.json");
        let v = match fs::read(&p) {
            Ok(b) => serde_json::from_slice::<Value>(&b).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                json!({"enabled":true,"generation":0,"limit":4})
            }
            Err(e) => return Err(e),
        };
        if !v["enabled"].is_boolean()
            || v["generation"].as_u64().is_none()
            || !v["limit"].as_u64().is_some_and(|x| (1..=4).contains(&x))
        {
            return Err(io::Error::other("invalid control state"));
        }
        Ok(v)
    }
    fn leases(&self) -> io::Result<Vec<Value>> {
        let mut out = vec![];
        for slot in 0..4 {
            let mut f = self.open(&format!("lease-{slot}"))?;
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::WouldBlock {
                    return Err(e);
                }
                let mut b = String::new();
                Read::by_ref(&mut f).take(4096).read_to_string(&mut b)?;
                let mut v =
                    serde_json::from_str::<Value>(&b).unwrap_or(json!({"owner":"initializing"}));
                v["slot"] = json!(slot);
                out.push(v);
            }
        }
        Ok(out)
    }
    pub fn owners() -> (Vec<Value>, bool) {
        let devices: Vec<String> = entries("/sys/class/video4linux")
            .iter()
            .filter(|p| text(p.join("name")) == "qcom-venus-encoder")
            .filter_map(|p| {
                p.file_name()
                    .map(|n| format!("/dev/{}", n.to_string_lossy()))
            })
            .collect();
        let mut out = vec![];
        let mut complete = true;
        for p in entries("/proc") {
            let Some(pid) = p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let mut used = vec![];
            match fs::read_dir(p.join("fd")) {
                Ok(fds) => {
                    for fd in fds {
                        match fd.and_then(|f| fs::read_link(f.path())) {
                            Ok(target) => {
                                let t = target.to_string_lossy().to_string();
                                if devices.contains(&t) && !used.contains(&t) {
                                    used.push(t);
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                            Err(_) => complete = false,
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(_) => complete = false,
            }
            if !used.is_empty() {
                out.push(json!({"pid":pid,"name":text(p.join("comm")),"devices":used}));
            }
        }
        (out, complete)
    }
    fn managed(mut pid: u64, pids: &[u64]) -> bool {
        for _ in 0..32 {
            if pids.contains(&pid) {
                return true;
            }
            if pid <= 1 {
                break;
            }
            let s = text(format!("/proc/{pid}/stat"));
            pid = s
                .rsplit_once(')')
                .and_then(|(_, s)| s.split_whitespace().nth(1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        }
        false
    }
    pub fn status(&self) -> io::Result<Value> {
        let mut v = self.state()?;
        if !self.live {
            v["leases"] = json!([]);
            v["encoder_owners"] = json!([]);
            v["release_confirmed"] = json!(false);
            return Ok(v);
        }
        let _lock = self.lock()?;
        v = self.state()?;
        let leases = self.leases()?;
        let (mut owners, complete) = Self::owners();
        let pids: Vec<u64> = leases.iter().map(|x| number(&x["pid"])).collect();
        for owner in &mut owners {
            owner["managed"] = json!(Self::managed(number(&owner["pid"]), &pids));
        }
        v["release_confirmed"] = json!(complete && leases.is_empty() && owners.is_empty());
        v["owner_scan_complete"] = json!(complete);
        v["configured_pixel_rate"] = json!(leases
            .iter()
            .map(|x| number(&x["pixels_per_second"]))
            .sum::<u64>());
        v["leases"] = json!(leases);
        v["encoder_owners"] = json!(owners);
        v["capacity_verified"] = json!(false);
        Ok(v)
    }
    pub fn configure(&self, action: &str, limit: Option<u64>) -> io::Result<Value> {
        {
            let _lock = self.lock()?;
            let mut s = self.state()?;
            match action {
                "off" | "resume" => {
                    s["enabled"] = json!(action == "resume");
                    s["generation"] = json!(number(&s["generation"])
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("generation overflow"))?);
                }
                "limit" => {
                    if !limit.is_some_and(|x| (1..=4).contains(&x)) || !self.leases()?.is_empty() {
                        return Err(io::Error::other(
                            "Stop all streams before setting limit 1..4",
                        ));
                    }
                    s["limit"] = json!(limit);
                }
                _ => return Err(io::Error::other("unknown video control action")),
            }
            let mut f = self.open("state.tmp")?;
            f.set_len(0)?;
            f.write_all(serde_json::to_string(&s)?.as_bytes())?;
            f.sync_all()?;
            fs::rename(self.path.join("state.tmp"), self.path.join("state.json"))?;
        }
        self.status()
    }
    // Caller keeps the creation lock until its pipeline owns the lease fd.
    pub fn reserve(&self, owner: &str, w: u64, h: u64, fps: u64) -> io::Result<(File, File, u64)> {
        let lock = self.lock()?;
        let s = self.state()?;
        if s["enabled"] != true {
            return Err(io::Error::other(
                "All video is manually disabled; resume first",
            ));
        }
        let leases = self.leases()?;
        let pids: Vec<u64> = leases.iter().map(|x| number(&x["pid"])).collect();
        let (owners, complete) = Self::owners();
        if !complete
            || owners
                .iter()
                .any(|x| !Self::managed(number(&x["pid"]), &pids))
        {
            return Err(io::Error::other(
                "Unmanaged encoder owner or incomplete owner scan",
            ));
        }
        if leases.len() >= number(&s["limit"]) as usize {
            return Err(io::Error::other("Encoder quota reached"));
        }
        for slot in 0..number(&s["limit"]) {
            let mut f = self.open(&format!("lease-{slot}"))?;
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                continue;
            }
            let g = number(&s["generation"]);
            let v = json!({"owner":owner,"pid":std::process::id(),"generation":g,"width":w,"height":h,"requested_fps":fps,"pixels_per_second":w*h*fps,"created_monotonic":now()as f64/1e9});
            f.set_len(0)?;
            f.write_all(serde_json::to_string(&v)?.as_bytes())?;
            return Ok((lock, f, g));
        }
        Err(io::Error::other("Encoder quota reached"))
    }
}
