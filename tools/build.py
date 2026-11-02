#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
import argparse,hashlib,json,shutil,subprocess,os
from pathlib import Path
r=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser();p.add_argument('--output',type=Path,required=True);p.add_argument('--target',default='aarch64-unknown-linux-gnu');p.add_argument('--cargo-target-dir',type=Path);p.add_argument('--sysroot',type=Path);a=p.parse_args()
if a.output.exists():p.error('Choose a new output directory')
a.output.mkdir(parents=True);cache=(a.cargo_target_dir or a.output/'cargo').resolve();env=dict(os.environ)
if a.target.startswith('aarch64'):
 env['CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER']='aarch64-linux-gnu-gcc'
if a.sysroot:
 env.update(PKG_CONFIG_ALLOW_CROSS='1',PKG_CONFIG_SYSROOT_DIR=str(a.sysroot),PKG_CONFIG_LIBDIR=str(a.sysroot/'usr/lib/pkgconfig'),CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS='-C link-arg=--sysroot='+str(a.sysroot)+' -C link-arg=-Wl,-rpath-link,'+str(a.sysroot/'usr/lib'))
cmd=['cargo','build','--locked','--release','--features','webrtc','--manifest-path',str(r/'backend/Cargo.toml'),'--target',a.target,'--target-dir',str(cache)];subprocess.run(cmd,env=env,check=True)
bundle=a.output/'debug';shutil.copytree(r/'frontend',bundle);shutil.copy2(cache/a.target/'release/mix-debug-server',bundle/'mix-debug-server')
(a.output/'build-manifest.json').write_text(json.dumps({'command':cmd,'files_sha256':{p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in bundle.iterdir()},'device_tested':False},indent=2)+'\n')
