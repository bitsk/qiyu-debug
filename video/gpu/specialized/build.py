#!/usr/bin/env python3
"""Cross-build private GPU capture; no device, no display-master changes."""
import argparse
import ast
import configparser
import hashlib
import json
from pathlib import Path
import re
import shutil
import struct
import subprocess
import tarfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
out = args.output.resolve()
assert out.is_relative_to(ROOT / 'build') and not out.exists()
bundle = out / 'mix-remote-video-gpu-specialized'
bundle.mkdir(parents=True)
glslang = ROOT / 'build/xr-tracking-r1/live-stream/build-deps/usr/bin/glslang'
shader = bundle / 'rgba-to-nv12.spv'
command = [str(glslang), '-V', '--target-env', 'vulkan1.1', str(HERE / 'rgba-to-nv12.comp'), '-o', str(shader)]
subprocess.run(command, check=True)
code = shader.read_bytes()
assert len(code) % 4 == 0 and code[:4] == bytes.fromhex('03022307')
(bundle / 'shader.h').write_text('static const uint32_t nv12_shader[] = {\n' +
    ',\n'.join('0x%08x' % word for word in struct.unpack('<' + 'I' * (len(code)//4), code)) + '\n};\n')
for name in ('nv12-capture-gpu.c', 'gpu-nv12.h', 'rgba-to-nv12.comp', 'cpu-source.sha256', 'build.py'):
    shutil.copyfile(HERE / name, bundle / name)
config = configparser.ConfigParser()
cross = ROOT / 'build/steamos-turnip-split-kms-r1/deps/frame-aarch64.ini'
config.read(cross)
compiler = ast.literal_eval(config['binaries']['c'])
cflags = ast.literal_eval(config['built-in options']['c_args'])
ldflags = ast.literal_eval(config['built-in options']['c_link_args'])
sysroot = Path(ast.literal_eval(config['properties']['sys_root']))
compile_cmd = [compiler, *cflags, '-I' + str(sysroot / 'usr/include/libdrm'),
    '-O3', '-Wall', '-Wextra', '-Werror', '-std=c11', str(bundle / 'nv12-capture-gpu.c'),
    *ldflags, '-Wl,-z,defs', '-ldrm', '-lvulkan', '-o', str(bundle / 'nv12-capture-gpu')]
with (out / 'compile.log').open('w') as log:
    subprocess.run(compile_cmd, stdout=log, stderr=subprocess.STDOUT, check=True)
elf = subprocess.check_output(['aarch64-linux-gnu-readelf', '-h', '-d', '-V',
                               str(bundle / 'nv12-capture-gpu')], text=True)
(out / 'elf.txt').write_text(elf)
assert 'AArch64' in elf
versions = set(re.findall(r'GLIBC_([0-9.]+)', elf))
assert all(tuple(map(int, v.split('.'))) <= (2, 39) for v in versions)
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
baseline = HERE.parent
assert (HERE/'nv12-capture-gpu.c').read_bytes() == (baseline/'nv12-capture-gpu.c').read_bytes()
manifest = dict(schema=1,
    candidate_change='Only specialize exact pipeline output width and height; preserve input/output pixel arithmetic, capture pacing, vblank/content deduplication and queue priority',
    source_baseline='build/steamos-remote-video-gpu-final-r1/mix-remote-video-gpu-arm64.tar.gz',
    source_baseline_sha256='dfdaa0353780d14c75ab597f2bdf95ed7ea3fbacca284cdd7bd7c3a8ad9fd522',
    baseline_source_sha256={name:sha(baseline/name) for name in ('nv12-capture-gpu.c','gpu-nv12.h','rgba-to-nv12.comp')},
    specialization={'constant_id_0':'uint32 output width','constant_id_1':'uint32 output height','push_constant_layout_bytes':24}, prefix='/opt/mix-remote-video-gpu-specialized', device_tested=False,
    source_sha256={p.name: sha(p) for p in sorted(bundle.iterdir()) if p.is_file()},
    compiler_argv=compile_cmd, shader_compiler_argv=command, shader_compiler_sha256=sha(glslang),
    frame_cross_file_sha256=sha(cross), glibc_versions=sorted(versions),
    backend='Vulkan compute; read-only imported linear KMS DMA-BUF storage buffer to host-visible NV12',
    reference_driver='build/steamos-turnip-split-kms-r1/source/src/freedreno/vulkan/tu_buffer.cc:150',
    input_contract='Actual GETFB2 pitch/offset/1920x3664 linear ARGB/XRGB8 or10bit; A081 split msm-kms primary and validated msm renderD128, matched to Vulkan render major/minor',
    producer_sync='Wait for already-published DMA-BUF implicit producer fences with POLLIN, then foreign-queue acquire/release. Does not freeze compositor reuse.',
    output_sync='Own compute fence wait then noncoherent invalidate when needed; only NV12 bytes copied to stdout',
    atomic_snapshot=False, complete_compositor_frames_verified=False,
    requested_fps_is_not_acceptance=True, source_sequence_gate=True, complete_nv12_content_deduplication=True,
    width_multiple=4, no_surface_or_swapchain=True, no_modeset_or_master=True,
    required_device_environment={'VK_DRIVER_FILES':'/opt/mix-steamvr-turnip/share/vulkan/icd.d/freedreno_icd.aarch64.json',
        'DISABLE_GAMESCOPE_WSI':'1'},
    required_child_environment_unset=['TU_A081_KMS_DEVICE','ENABLE_GAMESCOPE_WSI','VK_ICD_FILENAMES','LD_PRELOAD'],
    installation='Extract privately to /opt/mix-remote-video-gpu-specialized; invoke binary explicitly. No service or existing capture replacement.')
(bundle / 'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
(bundle / 'SHA256SUMS').write_text(''.join(f'{sha(p)}  {p.name}\n' for p in sorted(bundle.iterdir()) if p.is_file() and p.name!='SHA256SUMS'))
def ownership(info):
    info.uid=info.gid=0
    info.uname=info.gname='root'
    info.mode=0o755 if info.isdir() or info.name.endswith('/nv12-capture-gpu') else 0o644
    return info
archive=out/'mix-remote-video-gpu-specialized-arm64.tar.gz'
with tarfile.open(archive,'w:gz') as tar:tar.add(bundle,arcname='mix-remote-video-gpu-specialized',filter=ownership)
delivery=dict(archive=str(archive),sha256=sha(archive),bytes=archive.stat().st_size,
    executable_sha256=sha(bundle/'nv12-capture-gpu'),device_tested=False)
(out/'delivery.json').write_text(json.dumps(delivery,indent=2)+'\n')
print(json.dumps(delivery,indent=2))
