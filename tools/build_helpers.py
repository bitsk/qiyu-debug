#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Build observation helpers using the shared device API and supplied OpenVR SDK."""
import argparse,hashlib,json,subprocess
from pathlib import Path
r=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser();p.add_argument('--output',type=Path,required=True);p.add_argument('--device',type=Path,default=r.parent/'qiyu-device-service');p.add_argument('--openvr-include',type=Path,required=True);p.add_argument('--cxx',default='aarch64-linux-gnu-g++');a=p.parse_args()
if a.output.exists():p.error('Choose a new output directory')
if not (a.openvr_include/'openvr.h').is_file():p.error('Supply an OpenVR SDK include directory')
a.output.mkdir(parents=True)
for name in ['camera-imx','vr-timing']:
 subprocess.run([a.cxx,'-O2','-std=c++17','-pthread','-I'+str(a.device/'include'),'-I'+str(a.openvr_include),str(r/'helpers'/(name+'.cpp')),'-ldl','-o',str(a.output/name)],check=True)
(a.output/'build-manifest.json').write_text(json.dumps({'device_tested':False,'files_sha256':{q.name:hashlib.sha256(q.read_bytes()).hexdigest() for q in a.output.iterdir() if q.is_file()},'openvr_header_sha256':hashlib.sha256((a.openvr_include/'openvr.h').read_bytes()).hexdigest()},indent=2)+'\n')
