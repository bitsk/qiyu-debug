#!/usr/bin/env python3
"""Install a new private candidate, without starting or enabling services."""
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess

assert os.geteuid() == 0 and os.uname().machine == 'aarch64'
assert Path('/etc/a081-rootfs-id').read_text().strip() == 'a081-steamos-arm64'
assert Path('/proc/device-tree/model').read_bytes().rstrip(b'\0') == b'iQIYI Qiyu MIX (A081)'
source = Path(__file__).resolve().parent
prefix = Path('/opt/mix-debug-console')
state = Path('/var/lib/mix-debug-console')
unit = Path('/etc/systemd/system/a081-debug-console.service')
assert not prefix.exists() and not state.exists() and not unit.exists()
assert Path('/etc/systemd/system/a081-remote-view.service').is_file()
checks = {}
for line in (source/'SHA256SUMS').read_text().splitlines():
    digest, name = line.split('  ', 1)
    assert '/' not in name and name not in checks
    f = source/name
    assert f.is_file() and not f.is_symlink()
    assert hashlib.sha256(f.read_bytes()).hexdigest() == digest
    checks[name] = digest
assert set(checks) == {f.name for f in source.iterdir() if f.is_file() and f.name != 'SHA256SUMS'}
shutil.copytree(source, prefix)
for f in [prefix, *prefix.iterdir()]:
    os.chown(f, 0, 0); f.chmod(0o755 if f.is_dir() or f.name in ('capture', 'vr-timing', 'camera-imx', 'mix-debug-server') else 0o644)
state.mkdir(mode=0o700)
token = secrets.token_urlsafe(32)
with os.fdopen(os.open(state/'access.token', os.O_WRONLY|os.O_CREAT|os.O_EXCL, 0o600), 'w') as f: f.write(token+'\n')
shutil.copyfile(prefix/unit.name, unit)
subprocess.run(['systemctl', 'daemon-reload'], check=True)
print(json.dumps({'installed':str(prefix),'token_file':str(state/'access.token'),
    'port':8090,'started':False,'enabled':False,'device_tested':False}))
