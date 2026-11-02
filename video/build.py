#!/usr/bin/env python3
"""Build the isolated real-KMS NV12/WebRTC service; do not replace private libs."""
import argparse
import ast
import configparser
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--output', required=True, type=Path)
p.add_argument('--backend', required=True, type=Path, help='Fresh cross-built mix-debug-server binary')
a = p.parse_args()
out = a.output.resolve()
assert out.is_relative_to(ROOT / 'build') and not out.exists()
config = configparser.ConfigParser()
config.read(ROOT / 'build/steamos-turnip-split-kms-r1/deps/frame-aarch64.ini')
compiler = ast.literal_eval(config['binaries']['c'])
cflags = ast.literal_eval(config['built-in options']['c_args'])
ldflags = ast.literal_eval(config['built-in options']['c_link_args'])
sysroot = Path(ast.literal_eval(config['properties']['sys_root']))
bundle = out / 'mix-remote-video'
bundle.mkdir(parents=True)
command = [compiler, *cflags, '-I' + str(sysroot / 'usr/include/libdrm'),
           '-O3', '-Wall', '-Wextra', '-Werror', '-std=c11', str(HERE / 'nv12-capture.c'),
           *ldflags, '-Wl,-z,defs', '-ldrm', '-lyuv', '-o', str(bundle / 'nv12-capture')]
with (out / 'compile.log').open('w') as log:
    subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
backend = a.backend.resolve()
assert backend.is_file() and backend.read_bytes()[:6] == b'\x7fELF\x02\x01'
shutil.copyfile(backend, bundle/'mix-video-server')
for name in ('nv12-capture.c', 'build.py', 'build-v4l2.py', 'install.py', 'a081-remote-video.service', 'README.md'):
    shutil.copyfile(HERE / name, bundle / name)
shutil.copytree(HERE / 'web', bundle / 'web')
for source in bundle.glob('*.py'):
    compile(source.read_text(), str(source), 'exec')
subprocess.run(['node', '--check', str(bundle / 'web/app.js')], check=True)
with (out / 'nv12-capture.elf.txt').open('w') as log:
    subprocess.run(['aarch64-linux-gnu-readelf', '-h', '-d', '-V', str(bundle / 'nv12-capture')], stdout=log, check=True)
assert 'AArch64' in (out / 'nv12-capture.elf.txt').read_text()
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
files = {str(path.relative_to(bundle)): sha(path) for path in sorted(bundle.rglob('*')) if path.is_file()}
manifest = {'format': 'a081-remote-video-v1', 'prefix': '/opt/mix-remote-video',
    'architecture': 'AArch64', 'runtime_backend': 'Rust', 'backend_source': str(backend), 'device_tested': False, 'files_sha256': files, 'compiler_argv': command,
    'capture_source': 'Existing A081 GETFB2/PRIME CPU readback, tiled rotation and libyuv NV12 conversion',
    'atomic_snapshot': False, 'complete_compositor_frames_verified': False,
    'frame_counts_separate': ['capture readbacks', 'encoded VCL access units', 'browser received',
                              'browser decoded', 'browser requestVideoFrameCallback presented'],
    'defaults_requested': {'width': 1280, 'fps': 60, 'bitrate': 8000000}, 'requested_fps_is_not_acceptance': True,
    'capture_program_allowlist': ['/opt/mix-remote-video/nv12-capture',
        '/opt/mix-remote-video-gpu/nv12-capture-gpu',
        '/opt/mix-remote-video-gpu-specialized/nv12-capture-gpu'],
    'phase_latency': {'clock': 'device CLOCK_MONOTONIC', 'match': 'exact PTS',
        'pending_limit': 1024, 'recent_samples_per_stage': 300, 'end_to_end': False},
    'browser_latency': ['jitter buffer mean over stats interval', 'decode mean over stats interval',
                        'selected ICE/STUN round-trip time'],
    'resolution_widths': [640,960,1280,1600,1920,2560,3200,3664],
    'native_stereo_dimensions': [3664,1920], 'max_fps_above_2560': 30,
    'bitrate_mbps_range': [1,100], 'jpeg_default': False, 'libraries_shipped': False,
    'codecs': ['auto','h264','h265'], 'eyes': ['stereo','left','right'],
    'native_mono_dimensions': [1832,1920],
    'gpu_path': 'Native C / Vulkan export DMA-BUF / Venus import / compressed AU pipe; Rust HTTP/signaling/ownership',
    'dependencies': ['Frame libdrm and libyuv', 'GStreamer 1.24.2 + native Rust bindings',
                     'Private /opt/mix-remote-video/lib WebRTC/DTLS/SRTP plugin package',
                     'Real Venus V4L2 H.264/H.265 encoder', 'Read-only root KMS access']}
(bundle / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
checks = dict(files, **{'manifest.json': sha(bundle / 'manifest.json')})
(bundle / 'SHA256SUMS').write_text(''.join(f'{digest}  {name}\n' for name, digest in sorted(checks.items())))


def metadata(info):
    info.uid = info.gid = 0
    info.uname = info.gname = 'root'
    info.mode = 0o755 if info.isdir() or info.name.endswith(('/nv12-capture','/mix-video-server')) else 0o644
    return info


archive = out / 'mix-remote-video-arm64.tar.gz'
with tarfile.open(archive, 'w:gz') as stream:
    stream.add(bundle, arcname='mix-remote-video', filter=metadata)
(out / 'manifest.json').write_text(json.dumps(dict(manifest, archive_sha256=sha(archive)), indent=2) + '\n')
print(json.dumps({'archive': str(archive), 'sha256': sha(archive), 'bytes': archive.stat().st_size}, indent=2))
