use super::util::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
const AGE: u64 = 100_000_000;
const STATUS_AGE: u64 = 1_500_000_000;
pub fn selection(config: &Path) -> Option<Value> {
    for p in [
        config.to_path_buf(),
        PathBuf::from("/run/mix-vr-shell-controller/session.json"),
    ] {
        let Some(d) = session(&p) else { continue };
        let trial = d["trial"].as_str().unwrap_or("");
        let endpoint = d["controller_pose_socket"].as_str().unwrap_or("");
        if d["controller_plan"].is_null()
            || !trial.starts_with("vr-shell-")
            || !safe_id(trial)
            || d["raw"] != format!("/opt/mix-vr-shell-controller/tracking/runs/{trial}")
            || !endpoint
                .strip_prefix("/run/a081-controller-pose-")
                .and_then(|s| s.strip_suffix("/tracking.sock"))
                .is_some_and(safe_id)
        {
            continue;
        }
        return Some(d);
    }
    None
}
pub fn decode(p: &[u8]) -> Option<Value> {
    if p.len() != 96
        || u32le(p, 0) != 0x43503841
        || ![1, 2].contains(&u32le(p, 4))
        || u32le(p, 8) != 96
        || u32le(p, 48) > 1
        || (u32le(p, 4) == 1 && (u32le(p, 60) != 0 || u32le(p, 92) != 0))
        || u32le(p, 12) & !(if u32le(p, 4) == 2 { 127 } else { 63 }) != 0
    {
        return None;
    }
    let vals: Vec<f32> = [56, 64, 68, 72, 76, 80, 84, 88]
        .iter()
        .map(|&i| f32le(p, i))
        .collect();
    if vals.iter().any(|x| !x.is_finite()) {
        return None;
    }
    Some(
        json!({"wire_version":u32le(p,4),"controls":u32le(p,60),"joystick":u32le(p,92),"hand":u32le(p,48),"flags":u32le(p,12),"packet_session":u64le(p,16).to_string(),"sequence":u64le(p,24),"sample_ns":u64le(p,32),"send_ns":u64le(p,40),"coordinate_profile":u32le(p,52),"confidence":vals[0],"position":&vals[1..4],"orientation":&vals[4..8]}),
    )
}
fn packet_controls(p: &Value) -> Option<Value> {
    if p["wire_version"] != 2 {
        return None;
    }
    let controls = number(&p["controls"]);
    let stick = number(&p["joystick"]);
    let mask = if number(&p["flags"]) & 64 != 0 { (controls >> 24) & 31 } else { 0 };
    // The producer expires each Nordic field before packing it. Receipt here
    // is the relay send time, not a fabricated radio timestamp. Always prefer
    // v2 (even mask=0) over the low-frequency diagnostic log fallback.
    Some(json!({"kind":"controller_controls","version":1,"hand":p["hand"],
        "input_mode":"live","input_source":"controller_pose_v2","input_sampled_only":false,
        "valid_mask":mask,"stock_enable":number(&p["flags"])&1!=0,
        "observation_monotonic_ns":p["send_ns"],"mode1_receipt_monotonic_ns":p["send_ns"],
        "joystick_receipt_monotonic_ns":p["send_ns"],
        "stock_buttons_raw":controls&240,"stock_touch_raw":(controls>>8)&15,
        "stock_index_finger_raw":(controls>>12)&15,"stock_middle_finger_raw":(controls>>16)&15,
        "stock_joystick_raw":[stick as u8 as i8,(stick>>8) as u8 as i8]}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_v2() -> Vec<u8> {
        // Captured from the current A081 relay, left hand, flags=127.
        let hex = "4138504302000000600000007f0000005dd66abb01000000c96d000000000000cd10f6aa460000002948f7aa4600000000000000010000000000803f0002001f20be27bef369c1be76104abc418a8b3d1fa667bf68e5cfbea251dd3d00000000";
        (0..hex.len()).step_by(2).map(|i|u8::from_str_radix(&hex[i..i+2],16).unwrap()).collect()
    }

    #[test]
    fn current_relay_controls_and_legacy_pose() {
        let mut bytes = live_v2();
        let packet = decode(&bytes).unwrap();
        let controls = packet_controls(&packet).unwrap();
        assert_eq!(controls["valid_mask"], 31);
        assert_eq!(controls["stock_touch_raw"], 2);
        bytes[92..96].copy_from_slice(&0x7f80u32.to_le_bytes());
        assert_eq!(packet_controls(&decode(&bytes).unwrap()).unwrap()["stock_joystick_raw"], json!([-128,127]));
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert!(decode(&bytes).is_none()); // v2 fields cannot leak into v1.
        bytes[12..16].copy_from_slice(&63u32.to_le_bytes());
        bytes[60..64].fill(0);
        bytes[92..96].fill(0);
        assert!(packet_controls(&decode(&bytes).unwrap()).is_none());
        bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn packet_controls_expire_and_do_not_require_valid_pose() {
        let mut c = Controllers::default();
        c.connected = true;
        c.packets[0] = decode(&live_v2()).unwrap();
        c.packets[0]["send_ns"] = json!(now());
        c.packets[0]["sample_ns"] = json!(now()-1_000_000);
        c.packets[0]["flags"] = json!(65); // Connected controls, no valid pose.
        c.statuses[0] = json!({"observed_boot_ns":clock(libc::CLOCK_BOOTTIME),"ctrl_state":5,"tracking_enabled":true});
        let s = c.snapshot();
        assert_eq!(s[0]["input_active"], true);
        assert_eq!(s[0]["position_tracking"], false);
        c.packets[0]["flags"] = json!(1);
        assert_eq!(c.snapshot()[0]["valid_mask"], 0);
        c.packets[0]["flags"] = json!(65);
        c.packets[0]["send_ns"] = json!(now()-AGE-1);
        assert_eq!(c.snapshot()[0]["input_active"], false);
        c.connected = false;
        assert_eq!(c.snapshot()[0]["position_tracking"], false);
    }
}
#[derive(Clone)]
pub struct Controllers {
    pub selected: Value,
    pub connected: bool,
    pub packets: [Value; 2],
    statuses: [Value; 2],
    poses: [Value; 2],
    controls: [Value; 2],
    radio: Value,
}
impl Default for Controllers {
    fn default() -> Self {
        Self {
            selected: Value::Null,
            connected: false,
            packets: [Value::Null, Value::Null],
            statuses: [Value::Null, Value::Null],
            poses: [Value::Null, Value::Null],
            controls: [Value::Null, Value::Null],
            radio: Value::Null,
        }
    }
}
impl Controllers {
    pub fn logs(&mut self, selected: Value, explicit: Option<&Path>, boot: Option<&str>) {
        if self.selected["trial"] != selected["trial"] || selected["cleanup_complete"] == true {
            self.packets = [Value::Null, Value::Null];
            self.connected = false;
        }
        self.selected = selected;
        self.statuses = [Value::Null, Value::Null];
        self.poses = [Value::Null, Value::Null];
        self.controls = [Value::Null, Value::Null];
        self.radio = Value::Null;
        if let Some(raw) = self.selected["raw"].as_str() {
            let raw = Path::new(raw);
            for mut r in tail_json(&raw.join("controller-sdk.log")) {
                let Some(h) = r["hand"].as_u64().filter(|h| *h < 2).map(|x| x as usize) else {
                    continue;
                };
                match r["kind"].as_str() {
                    Some("controller_sdk_pose") => self.poses[h] = r,
                    Some("controller_status") => {
                        r["observed_boot_ns"] = self.poses[h]["query_boottime_ns"].clone();
                        self.statuses[h] = r;
                    }
                    Some("controller_controls") if r["version"] == 1 => self.controls[h] = r,
                    _ => (),
                }
            }
            for r in tail_json(&raw.join("controller-radio.log")) {
                if r["kind"] == "nordic_rx"
                    || (r["kind"] == "radio_timestamp_tx" && r["spi_bytes"] == 72)
                {
                    self.radio = r;
                    self.radio_controls();
                }
            }
        }
        if boot == Some(text("/proc/sys/kernel/random/boot_id").as_str()) {
            if let Some(p) = explicit {
                for r in tail_json(p) {
                    if r["kind"] == "controller_controls"
                        && r["version"] == 1
                        && r["session"] == self.selected["trial"]
                        && !self.selected.is_null()
                    {
                        if let Some(h) = r["hand"].as_u64().filter(|h| *h < 2) {
                            self.controls[h as usize] = r;
                        }
                    }
                }
            }
        }
    }
    fn radio_controls(&mut self) {
        let raw = self.radio["rx"].as_str().unwrap_or("");
        // nordic_rx logs one 36-byte slice; timestamp exchanges log both
        // slices of the 72-byte transfer. Validate each independently.
        if ![72, 144].contains(&raw.len()) || !raw.is_ascii() {
            return;
        }
        let b: Option<Vec<u8>> = (0..raw.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&raw[i..i + 2], 16).ok())
            .collect();
        let Some(b) = b else { return };
        for packet in b.chunks_exact(36) {
            self.radio_control_packet(packet);
        }
    }
    fn radio_control_packet(&mut self, b: &[u8]) {
        if b[..2] != [0xa5, 0x5a] || self.radio["completion_ns"].as_u64().is_none() {
            return;
        }
        let mut p = b[2..].to_vec();
        let key = p[1];
        for i in 0..34 {
            if i != 1 {
                p[i] ^= key;
            }
        }
        if p[..33].iter().fold(0u8, |s, x| s.wrapping_add(*x)) != p[33] || (p[0] >> 4) & 7 != 7 {
            return;
        }
        let h = (p[0] >> 7) as usize;
        let c = &mut self.controls[h];
        if c.is_null() {
            *c = json!({"kind":"controller_controls","version":1,"hand":h,"input_mode":"live","valid_mask":0,"stock_enable":false});
        }
        c["observation_monotonic_ns"] = self.radio["completion_ns"].clone();
        c["input_source"] = json!("sampled_radio_log");
        c["input_sampled_only"] = json!(true);
        c["stock_enable"] = json!(self.statuses[h]["tracking_enabled"] == true);
        match p[2] & 15 {
            1 => {
                c["mode1_receipt_monotonic_ns"] = self.radio["completion_ns"].clone();
                c["stock_buttons_raw"] = json!(p[27] & 240);
                c["stock_touch_raw"] = json!(p[27] & 15);
                c["stock_index_finger_raw"] = json!(p[28] & 15);
                c["stock_middle_finger_raw"] = json!(p[28] >> 4);
                c["valid_mask"] = json!(number(&c["valid_mask"]) | 15);
            }
            2 => {
                c["joystick_receipt_monotonic_ns"] = self.radio["completion_ns"].clone();
                c["stock_joystick_raw"] = json!([p[28] as i8, p[27] as i8]);
                c["valid_mask"] = json!(number(&c["valid_mask"]) | 16);
            }
            _ => (),
        }
    }
    pub fn snapshot(&self) -> Value {
        let now = now();
        let boot = clock(libc::CLOCK_BOOTTIME);
        let connected = self.connected && self.selected["cleanup_complete"] != true;
        let worker = if connected
            && self
                .packets
                .iter()
                .any(|p| fresh(number(&p["send_ns"]), now, AGE))
        {
            "running"
        } else if self.selected["cleanup_complete"] == true {
            "stopped"
        } else if self.selected["controller_pose_socket"]
            .as_str()
            .is_some_and(|p| Path::new(p).exists())
        {
            "waiting"
        } else {
            "not_started"
        };
        json!((0..2).map(|h|{let status=&self.statuses[h];let pose=&self.poses[h];let empty=Value::Null;let p=if connected{&self.packets[h]}else{&empty};let status_fresh=connected&&fresh(number(&status["observed_boot_ns"]),boot,STATUS_AGE);let flags=number(&p["flags"]);let live=connected&&fresh(number(&p["sample_ns"]),now,AGE)&&fresh(number(&p["send_ns"]),now,AGE)&&number(&p["sample_ns"])<=number(&p["send_ns"]);let clock_ok=live&&flags&8!=0;let awake=status_fresh&&status["ctrl_state"]==5&&status["tracking_enabled"]==true;let norm=p["orientation"].as_array().filter(|a|a.len()==4).map(|a|a.iter().map(|x|x.as_f64().unwrap_or(f64::NAN).powi(2)).sum::<f64>());let valid=clock_ok&&awake&&flags&31==31&&p["coordinate_profile"]==1&&p["confidence"].as_f64().unwrap_or(0.0)>=0.1&&norm.is_some_and(|n|(n-1.0).abs()<0.01);
 let mut c=packet_controls(p).unwrap_or_else(||if self.controls[h].is_object(){self.controls[h].clone()}else{json!({})});let mut mask=number(&c["valid_mask"])&31;let observed=number(&c["observation_monotonic_ns"]);let limit=number(&c["max_age_ns"]).min(AGE);let limit=if c["max_age_ns"].is_null(){AGE}else{limit};if !connected||!awake||c["input_mode"]!="live"||!fresh(observed,now,AGE)||c["stock_enable"]!=true{mask=0;}for(field,bits)in[("mode1_receipt_monotonic_ns",15),("joystick_receipt_monotonic_ns",16)]{if !fresh(number(&c[field]),now,limit){mask&=!bits;}}
 let sf=|key:&str|if status_fresh{status[key].clone()}else{Value::Null};let extra=json!({"valid_mask":mask,"input_active":mask!=0,"controls_age_ms":age(observed,now),"mode1_age_ms":age(number(&c["mode1_receipt_monotonic_ns"]),now),"joystick_age_ms":age(number(&c["joystick_receipt_monotonic_ns"]),now),"hand":h,"worker_state":worker,"session":self.selected["trial"],"packet_session":p["packet_session"],"sequence":p["sequence"],"sample_ns":p["sample_ns"],"tracking_epoch":sf("epoch"),"fresh":live,"age_ms":age(number(&p["sample_ns"]),now),"status_fresh":status_fresh,"status_age_ms":age(number(&status["observed_boot_ns"]),boot),"link_present":sf("link_present"),"tracking_enabled":sf("tracking_enabled"),"ctrl_state":sf("ctrl_state"),"head_power_state":sf("head_power"),"gzll_raw_state":sf("gzll_raw"),"transport_fresh":sf("transport_fresh"),"imu_fresh":sf("imu_fresh"),"clock_valid":clock_ok,"orientation_valid":valid,"position_tracking":valid,"optical_valid":valid&&flags&32!=0,"position":if valid{p["position"].clone()}else{Value::Null},"orientation":if valid{p["orientation"].clone()}else{Value::Null},"confidence":p["confidence"],"coordinate_profile":p["coordinate_profile"],"radio_fresh":connected&&fresh(number(&self.radio["completion_ns"]),now,STATUS_AGE),"radio_age_ms":age(number(&self.radio["completion_ns"]),now),"radio_spi_bytes":self.radio["spi_bytes"],"sdk_position":pose["position_sdk"],"sdk_orientation_wxyz":pose["quaternion_wxyz_sdk"],"sdk_confidence":pose["confidence"],"sdk_query_age_ms":age(number(&pose["query_boottime_ns"]),boot),"sdk_pose_accepted":pose["sdk_pose_accepted"]==true&&fresh(number(&pose["query_boottime_ns"]),boot,AGE)&&awake,"reason":if worker=="not_started"{"worker_not_started"}else if worker=="stopped"{"worker_stopped"}else if status_fresh&&status["ctrl_state"]==7{"sleep_cached_pose"}else if valid{"valid"}else{"awaiting_fresh_valid_pose"}});c.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());c}).collect::<Vec<_>>())
    }
}
