use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
pub static STOP: AtomicBool = AtomicBool::new(false);
pub extern "C" fn shutdown(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}
pub fn now() -> u64 {
    clock(libc::CLOCK_MONOTONIC)
}
pub fn clock(id: i32) -> u64 {
    unsafe {
        let mut t: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(id, &mut t);
        t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
    }
}
pub fn stopped() -> bool {
    STOP.load(Ordering::Relaxed)
}
pub fn pause(ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while !stopped() && Instant::now() < end {
        std::thread::sleep(
            Duration::from_millis(20).min(end.saturating_duration_since(Instant::now())),
        );
    }
}
pub fn text(p: impl AsRef<Path>) -> String {
    fs::read_to_string(p).unwrap_or_default().trim().to_string()
}
pub fn value(p: impl AsRef<Path>) -> Value {
    fs::read(p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}
pub fn entries(p: impl AsRef<Path>) -> Vec<std::path::PathBuf> {
    fs::read_dir(p)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|x| x.path())
        .collect()
}
pub fn fresh(stamp: u64, t: u64, limit: u64) -> bool {
    stamp > 0 && stamp <= t && t - stamp < limit
}
pub fn age(stamp: u64, t: u64) -> Value {
    if stamp > 0 {
        json!((t as f64 - stamp as f64) / 1e6)
    } else {
        Value::Null
    }
}
pub fn number(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}
pub fn trusted(p: &Path, uid: u32, private: bool) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| {
        m.is_file() && m.uid() == uid && m.mode() & if private { 0o077 } else { 0o022 } == 0
    })
}
pub fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 160
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
pub fn session(path: &Path) -> Option<Value> {
    if !trusted(path, 0, false) {
        return None;
    }
    let v = value(path);
    if v["boot_id"].as_str() != Some(text("/proc/sys/kernel/random/boot_id").as_str()) {
        return None;
    }
    Some(v)
}
pub fn tail_json(p: &Path) -> Vec<Value> {
    use std::io::{Seek, SeekFrom};
    let Ok(mut f) = fs::File::open(p) else {
        return vec![];
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = size.saturating_sub(65536);
    let _ = f.seek(SeekFrom::Start(start));
    let mut b = Vec::new();
    let _ = f.take(65536).read_to_end(&mut b);
    let slice = if start > 0 {
        b.iter()
            .position(|x| *x == b'\n')
            .map(|i| &b[i + 1..])
            .unwrap_or(&[])
    } else {
        &b
    };
    slice
        .split_inclusive(|x| *x == b'\n')
        .filter(|l| l.ends_with(b"\n"))
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect()
}
pub fn user_child(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(1000) != 0
                || libc::setuid(1000) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
pub fn stop_child(child: &mut Child, user: bool) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let signal = |pid: u32, sig: i32| {
        if user {
            let mut c = Command::new(std::env::current_exe().unwrap());
            c.args(["--signal-child", &pid.to_string(), &sig.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            user_child(&mut c);
            let _ = c.status();
        } else {
            unsafe {
                libc::kill(pid as i32, sig);
            }
        }
    };
    signal(child.id(), libc::SIGTERM);
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    signal(child.id(), libc::SIGKILL);
    let _ = child.wait();
}
pub fn readable(fd: i32, ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut p, 1, ms) > 0 }
}
pub fn lines_from<R: Read + AsRawFd>(
    r: &mut R,
    pending: &mut Vec<u8>,
    limit: usize,
) -> io::Result<Vec<Vec<u8>>> {
    if !readable(r.as_raw_fd(), 100) {
        return Ok(vec![]);
    }
    let mut b = [0; 65536];
    let n = r.read(&mut b)?;
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    pending.extend_from_slice(&b[..n]);
    if pending.len() > limit {
        return Err(io::Error::other("observer output too large"));
    }
    let mut out = vec![];
    while let Some(i) = pending.iter().position(|x| *x == b'\n') {
        out.push(pending.drain(..=i).collect());
    }
    Ok(out)
}
pub fn finite(v: f32) -> Value {
    if v.is_finite() {
        json!(v)
    } else {
        Value::Null
    }
}
pub fn u32le(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(p[at..at + 4].try_into().unwrap())
}
pub fn u64le(p: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(p[at..at + 8].try_into().unwrap())
}
pub fn f32le(p: &[u8], at: usize) -> f32 {
    f32::from_bits(u32le(p, at))
}
