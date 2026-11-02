#!/usr/bin/env python3
"""Build the native AArch64 readback helper against the existing Frame sysroot."""
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
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()
out = a.output.resolve()
if out.exists(): p.error('Output already exists; choose a new directory')
ini = ROOT / 'build/steamos-turnip-split-kms-r1/deps/frame-aarch64.ini'
config = configparser.ConfigParser(); config.read(ini)
compiler = ast.literal_eval(config['binaries']['c'])
cflags = ast.literal_eval(config['built-in options']['c_args'])
ldflags = ast.literal_eval(config['built-in options']['c_link_args'])
sysroot = Path(ast.literal_eval(config['properties']['sys_root']))
out.mkdir(parents=True)
bundle = out / 'mix-remote-view'; bundle.mkdir()
command = [compiler, *cflags, '-I'+str(sysroot/'usr/include/libdrm'), '-O2', '-Wall', '-Wextra', '-Werror',
           '-std=c11', str(HERE/'capture.c'), *ldflags, '-Wl,-z,defs', '-ldrm', '-lturbojpeg', '-o', str(bundle/'capture')]
subprocess.run(command, check=True)
for name in ('server.py', 'index.html', 'capture.c', 'build.py', 'install.py', 'a081-remote-view.service',
             'sources.json', 'desktop-rotation.py', 'README.md'):
    shutil.copyfile(HERE/name, bundle/name)
subprocess.run(['aarch64-linux-gnu-readelf','-h',str(bundle/'capture')],check=True,stdout=(out/'elf-header.txt').open('w'))
subprocess.run(['aarch64-linux-gnu-readelf','-d',str(bundle/'capture')],check=True,stdout=(out/'elf-dynamic.txt').open('w'))
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
files = {f.name: sha(f) for f in sorted(bundle.iterdir())}
manifest = {'architecture':'AArch64','command':command,'files_sha256':files,'device_tested':False,
            'source':'A081 verified DRM readback path; no Reframed remote screen server exists in pinned c08eb',
            'dependencies':['Python 3 standard library','libdrm.so.2','libturbojpeg.so.0','Frame libc'],
            'output':{'width':1280,'height':672,'fps_target':5,'stereo':'two real panel eyes, CCW 90 degrees each, left/right','desktop':'whole actual panel, CCW 90 degrees'}}
(bundle/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
files['manifest.json']=sha(bundle/'manifest.json')
(bundle/'SHA256SUMS').write_text(''.join(f'{v}  {k}\n' for k,v in sorted(files.items())))
archive = out/'mix-remote-view-arm64.tar.gz'
def root_metadata(info):
    info.uid = info.gid = 0
    info.uname = info.gname = 'root'
    info.mode = 0o755 if info.isdir() or info.name.endswith('/capture') else 0o644
    return info
with tarfile.open(archive,'w:gz') as t: t.add(bundle,arcname='mix-remote-view',filter=root_metadata)
(out/'manifest.json').write_text(json.dumps({**manifest,'archive':str(archive),'archive_sha256':sha(archive)},indent=2)+'\n')
print(json.dumps({'archive':str(archive),'sha256':sha(archive),'bytes':archive.stat().st_size},indent=2))
