use super::{control::Control, util::*};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read},
    os::fd::AsRawFd,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};
struct State {
    profile: Value,
    generation: u64,
    touched: u64,
    last: u64,
    sequence: u64,
    jpeg: Option<Arc<Vec<u8>>>,
    meta: Value,
}
pub struct Preview {
    state: Mutex<State>,
    capture: Option<PathBuf>,
    image: Option<PathBuf>,
    control: Control,
}
impl Preview {
    pub fn new(capture: Option<PathBuf>, image: Option<PathBuf>, control: Control) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                profile: json!({"width":960,"fps":5,"quality":65}),
                generation: 0,
                touched: 0,
                last: 0,
                sequence: 0,
                jpeg: None,
                meta: json!({"state":"unavailable"}),
            }),
            capture,
            image,
            control,
        })
    }
    pub fn start(self: &Arc<Self>) {
        let p = self.clone();
        std::thread::spawn(move || p.run());
    }
    pub fn configure(&self, v: Value) -> io::Result<Value> {
        if v.as_object().is_none_or(|o| o.len() != 3)
            || ![640, 960, 1280, 1920].contains(&number(&v["width"]))
            || ![2, 5, 10, 15].contains(&number(&v["fps"]))
            || !v["quality"]
                .as_u64()
                .is_some_and(|q| (40..=90).contains(&q))
        {
            return Err(io::Error::other("Unsupported capture profile"));
        }
        let mut s = self.state.lock().unwrap();
        s.profile = v;
        s.generation += 1;
        s.jpeg = None;
        s.meta = json!({"state":"reconfiguring"});
        drop(s);
        Ok(self.status())
    }
    pub fn status(&self) -> Value {
        let s = self.state.lock().unwrap();
        let mut v = s.meta.clone();
        v["profile"] = s.profile.clone();
        v["sequence"] = json!(s.sequence);
        v["age_ms"] = age(s.last, now());
        v["encoding"] = json!("JPEG / CPU");
        v["atomic_snapshot"] = json!(false);
        v["source"] = json!(if self.image.is_some() {
            "recorded-image"
        } else if self.capture.is_some() {
            "live-kms"
        } else {
            "unavailable"
        });
        v
    }
    pub fn frame(&self) -> Option<Arc<Vec<u8>>> {
        let mut s = self.state.lock().unwrap();
        s.touched = now();
        if self.image.is_some() || fresh(s.last, now(), 2_000_000_000) {
            s.jpeg.clone()
        } else {
            None
        }
    }
    pub fn mime(&self) -> &'static str {
        if self
            .image
            .as_ref()
            .is_some_and(|p| p.extension().is_some_and(|s| s.eq_ignore_ascii_case("png")))
        {
            "image/png"
        } else {
            "image/jpeg"
        }
    }
    fn run(self: Arc<Self>) {
        if let Some(path) = &self.image {
            if let Ok(b) = fs::read(path) {
                if b.starts_with(&[0xff, 0xd8]) || b.starts_with(b"\x89PNG\r\n\x1a\n") {
                    let mut s = self.state.lock().unwrap();
                    s.jpeg = Some(Arc::new(b));
                    s.last = now();
                    s.sequence = 1;
                    s.meta = json!({"state":"recorded","actual_fps":0,"resized":false});
                }
            }
            return;
        }
        let Some(capture) = &self.capture else { return };
        while !stopped() {
            let (profile, generation, touched) = {
                let s = self.state.lock().unwrap();
                (s.profile.clone(), s.generation, s.touched)
            };
            if !self.control.state().is_ok_and(|v| v["enabled"] == true)
                || !fresh(touched, now(), 5_000_000_000)
            {
                let mut s = self.state.lock().unwrap();
                s.jpeg = None;
                s.meta = json!({"state":"idle"});
                drop(s);
                pause(200);
                continue;
            }
            let mut cmd = Command::new(capture);
            cmd.args([
                "--width",
                &number(&profile["width"]).to_string(),
                "--fps",
                &number(&profile["fps"]).to_string(),
                "--quality",
                &number(&profile["quality"]).to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
            if let Ok(mut child) = cmd.spawn() {
                let mut stdout = child.stdout.take().unwrap();
                let mut pending = vec![];
                let mut meta: Option<(Value, usize)> = None;
                let mut previous = 0;
                loop {
                    let end = {
                        let s = self.state.lock().unwrap();
                        stopped()
                            || s.generation != generation
                            || !fresh(s.touched, now(), 5_000_000_000)
                    };
                    if end || !self.control.state().is_ok_and(|v| v["enabled"] == true) {
                        break;
                    }
                    if !readable(stdout.as_raw_fd(), 100) {
                        continue;
                    }
                    let mut b = [0; 65536];
                    let Ok(n) = stdout.read(&mut b) else { break };
                    if n == 0 {
                        break;
                    }
                    pending.extend_from_slice(&b[..n]);
                    if pending.len() > 4 * 1024 * 1024 + 4096 {
                        break;
                    }
                    let mut invalid = false;
                    loop {
                        if meta.is_none() {
                            let Some(i) = pending.iter().position(|x| *x == b'\n') else {
                                if pending.len() > 4096 {
                                    invalid = true;
                                }
                                break;
                            };
                            if i > 4096 {
                                invalid = true;
                                break;
                            }
                            let line: Vec<_> = pending.drain(..=i).collect();
                            let Ok(v) = serde_json::from_slice::<Value>(&line) else {
                                invalid = true;
                                break;
                            };
                            let Some(size) = v["length"].as_u64().filter(|n| *n <= 4 * 1024 * 1024)
                            else {
                                invalid = true;
                                break;
                            };
                            meta = Some((v, size as usize));
                        }
                        let size = meta.as_ref().unwrap().1;
                        if pending.len() < size {
                            break;
                        }
                        let mut v = meta.take().unwrap().0;
                        let frame: Vec<_> = pending.drain(..size).collect();
                        let t = now();
                        v["actual_fps"] = json!(if previous > 0 {
                            1e9 / (t - previous) as f64
                        } else {
                            0.0
                        });
                        previous = t;
                        let mut s = self.state.lock().unwrap();
                        s.meta = v;
                        s.jpeg = (!frame.is_empty()).then(|| Arc::new(frame));
                        if s.jpeg.is_some() {
                            s.sequence += 1;
                            s.last = t;
                        }
                    }
                    if invalid {
                        break;
                    }
                }
                stop_child(&mut child, false);
            }
            let mut s = self.state.lock().unwrap();
            s.jpeg = None;
            s.meta = json!({"state":"waiting"});
            drop(s);
            pause(200);
        }
    }
}
