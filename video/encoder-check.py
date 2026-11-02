#!/usr/bin/env python3
"""Real Venus NV12/H.264 functional check, explicitly using a generated source.

This does not validate KMS capture, stereo throughput, WebRTC or browser FPS.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import threading
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--device', type=Path, required=True)
p.add_argument('--output', type=Path, required=True, help='New device-local evidence directory')
p.add_argument('--width', type=int, default=1280)
p.add_argument('--height', type=int, default=672)
p.add_argument('--fps', type=int, choices=(30, 60), default=60)
p.add_argument('--frames', type=int, default=300)
a = p.parse_args()
assert os.geteuid() == 0 and os.uname().machine == 'aarch64'
assert Path('/etc/a081-rootfs-id').read_text().strip() == 'a081-steamos-arm64'
assert re.fullmatch(r'/dev/video[0-9]+', str(a.device))
name = (Path('/sys/class/video4linux') / a.device.name / 'name').read_text().strip()
assert 'venus' in name.lower() and 'enc' in name.lower(), 'Select the actual Venus encoder node'
assert 320 <= a.width <= 1920 and 240 <= a.height <= 1080 and not (a.width % 2 or a.height % 2)
assert 60 <= a.frames <= 600 and a.output.is_absolute() and not a.output.exists()
a.output.mkdir(parents=True, mode=0o700)
# Dynamic V4L2 encoder factories must be discovered after the new kernel binds.
os.environ['GST_REGISTRY'] = str(a.output / 'gst-registry.bin')
import gi
gi.require_version('Gst', '1.0')
from gi.repository import Gst
Gst.init(None)
for factory in ('videotestsrc', 'capsfilter', 'v4l2h264enc', 'h264parse', 'filesink'):
    assert Gst.ElementFactory.find(factory), 'Missing GStreamer factory: ' + factory

stopped = threading.Event()
signal.signal(signal.SIGTERM, lambda *_: stopped.set())
signal.signal(signal.SIGINT, lambda *_: stopped.set())
lock = threading.Lock()
counts = {name: {'frames': 0, 'bytes': 0, 'pts': [], 'first_ns': 0, 'last_ns': 0}
          for name in ('nv12_input', 'encoded_vcl_au')}


def probe(_pad, info, which):
    buffer = info.get_buffer()
    if buffer is None:
        return Gst.PadProbeReturn.OK
    if which == 'encoded_vcl_au':
        ok, mapped = buffer.map(Gst.MapFlags.READ)
        if not ok:
            stopped.set()
            return Gst.PadProbeReturn.OK
        try:
            # Count only access units containing coded slices, not SPS/PPS-only buffers.
            vcl = any(1 <= mapped.data[m.end()] & 31 <= 5
                      for m in re.finditer(b'\x00\x00\x01', mapped.data) if m.end() < len(mapped.data))
        finally:
            buffer.unmap(mapped)
        if not vcl:
            return Gst.PadProbeReturn.OK
    now = time.monotonic_ns()
    with lock:
        c = counts[which]
        c['frames'] += 1
        c['bytes'] += buffer.get_size()
        c['pts'].append(int(buffer.pts))
        c.setdefault('negotiated_caps', _pad.get_current_caps().to_string())
        c['first_ns'] = c['first_ns'] or now
        c['last_ns'] = now
    return Gst.PadProbeReturn.OK


pipeline = Gst.parse_launch('videotestsrc name=source is-live=true pattern=ball ! capsfilter name=raw '
    '! v4l2h264enc name=encoder output-io-mode=mmap capture-io-mode=mmap '
    '! capsfilter name=coded ! h264parse name=parser config-interval=-1 '
    '! video/x-h264,stream-format=byte-stream,alignment=au ! filesink name=file sync=false')
pipeline.get_by_name('source').set_property('num-buffers', a.frames)
pipeline.get_by_name('raw').set_property('caps', Gst.Caps.from_string(
    f'video/x-raw,format=NV12,width={a.width},height={a.height},framerate={a.fps}/1,colorimetry=bt709'))
encoder = pipeline.get_by_name('encoder')
bound_device = encoder.get_property('device')
assert bound_device and Path(bound_device).resolve() == a.device.resolve(), (
    'The dynamic encoder factory is bound to a different device', bound_device, str(a.device))
controls = Gst.Structure.new_empty('controls')
for key, value in {'video_bitrate': 8000000, 'video_gop_size': a.fps,
                   'h264_profile': 1, 'h264_level': 13}.items():
    controls.set_value(key, value)
encoder.set_property('extra-controls', controls)
pipeline.get_by_name('coded').set_property('caps', Gst.Caps.from_string(
    'video/x-h264,profile=constrained-baseline,level=(string)4.2'))
encoded = a.output / 'venus-generated-source.h264'
pipeline.get_by_name('file').set_property('location', str(encoded))
pipeline.get_by_name('raw').get_static_pad('src').add_probe(Gst.PadProbeType.BUFFER, probe, 'nv12_input')
pipeline.get_by_name('parser').get_static_pad('src').add_probe(Gst.PadProbeType.BUFFER, probe, 'encoded_vcl_au')
result = {'scope': 'real encoder functional check with generated moving NV12; not real stereo FPS',
          'device': str(a.device), 'factory_bound_device': bound_device, 'device_name': name, 'gstreamer': Gst.version_string(),
          'width': a.width, 'height': a.height, 'requested_fps': a.fps,
          'requested_frames': a.frames, 'real_stereo_60fps_accepted': False}
started = time.monotonic()
try:
    assert pipeline.set_state(Gst.State.PLAYING) != Gst.StateChangeReturn.FAILURE
    bus = pipeline.get_bus()
    deadline = started + max(20, a.frames / a.fps + 15)
    eos = False
    while not stopped.is_set() and time.monotonic() < deadline:
        message = bus.timed_pop_filtered(200 * Gst.MSECOND, Gst.MessageType.ERROR | Gst.MessageType.EOS)
        if message is None:
            continue
        if message.type == Gst.MessageType.ERROR:
            error, detail = message.parse_error()
            raise RuntimeError(str(error) + '; ' + str(detail))
        eos = True
        break
    if not eos:
        raise RuntimeError('Stopped or timed out before encoder EOS')
except BaseException as error:
    result['error'] = repr(error)
finally:
    pipeline.set_state(Gst.State.NULL)
    result['encoder_wall_seconds'] = time.monotonic() - started

result['encoded_pts_match_input'] = counts['nv12_input']['pts'] == counts['encoded_vcl_au']['pts']
for key, count in counts.items():
    pts = count.pop('pts')
    count['pts_missing'] = sum(v == Gst.CLOCK_TIME_NONE for v in pts)
    count['pts_nonincreasing'] = sum(b <= a for a, b in zip(pts, pts[1:]))
    count['unique_pts'] = len(set(pts))
    span = (count.pop('last_ns') - count.pop('first_ns')) / 1e9
    count['observed_first_to_last_seconds'] = span
    count['observed_fps'] = (count['frames'] - 1) / span if count['frames'] > 1 and span > 0 else None
result['counts'] = counts
if encoded.exists():
    result['encoded_bytes'] = encoded.stat().st_size
    result['encoded_sha256'] = hashlib.sha256(encoded.read_bytes()).hexdigest()
if 'error' not in result:
    try:
        started = time.monotonic()
        command = ['ffmpeg', '-hide_banner', '-nostdin', '-v', 'error', '-threads', '4',
                   '-hwaccel', 'none', '-i', str(encoded), '-map', '0:v:0', '-an',
                   '-fps_mode', 'passthrough', '-f', 'framemd5', '-']
        decoded = subprocess.run(command, capture_output=True, text=True, timeout=30)
        (a.output / 'decoded-frame-md5.txt').write_text(decoded.stdout)
        (a.output / 'decode-stderr.log').write_text(decoded.stderr)
        rows = [line.split(',') for line in decoded.stdout.splitlines() if line and not line.startswith('#')]
        hashes = [row[-1].strip() for row in rows]
        result['decode'] = {'exit': decoded.returncode, 'frames': len(rows), 'distinct_frame_md5': len(set(hashes)),
                            'wall_seconds': time.monotonic() - started, 'software_decoder': True,
                            'fps_filter_or_duplicate_insertion': False}
        probed = subprocess.run(['ffprobe', '-v', 'error', '-select_streams', 'v:0',
                                '-show_entries', 'stream=codec_name,width,height,profile,level,r_frame_rate',
                                '-of', 'json', str(encoded)], capture_output=True, text=True, timeout=10, check=True)
        result['stream'] = json.loads(probed.stdout)['streams'][0]
        result['functional_pass'] = decoded.returncode == 0 and len(rows) == a.frames and len(set(hashes)) > 1 and all(
            c['frames'] == a.frames and not c['pts_missing'] and not c['pts_nonincreasing'] for c in counts.values()) and (
            result['stream']['width'], result['stream']['height']) == (a.width, a.height)
    except BaseException as error:
        result.update(error=repr(error), functional_pass=False)
else:
    result['functional_pass'] = False
(a.output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result, indent=2), flush=True)
raise SystemExit(0 if result['functional_pass'] else 1)
