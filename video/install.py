#!/usr/bin/env python3
"""Install managed service files only; preserve the separately installed lib tree."""
import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path, PurePosixPath
import secrets
import shutil
import stat
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--address', required=True)
p.add_argument('--token-file', type=Path, choices=(Path('/var/lib/mix-remote-video/access.token'),
                Path('/var/lib/mix-debug-console/access.token')),
                default=Path('/var/lib/mix-debug-console/access.token'))
a = p.parse_args()
ip = ipaddress.ip_address(a.address)
assert ip.version == 4 and ip.is_private and not ip.is_unspecified
assert os.geteuid() == 0 and os.uname().machine == 'aarch64'
assert Path('/etc/a081-rootfs-id').read_text().strip() == 'a081-steamos-arm64'
assert Path('/proc/device-tree/model').read_bytes().rstrip(b'\0') == b'iQIYI Qiyu MIX (A081)'
source = Path(__file__).resolve().parent
prefix = Path('/opt/mix-remote-video')
state = Path('/var/lib/mix-remote-video')
unit = Path('/etc/systemd/system/a081-remote-video.service')
checks = {}
for line in (source / 'SHA256SUMS').read_text().splitlines():
    digest, name = line.split('  ', 1)
    relative = PurePosixPath(name)
    assert not relative.is_absolute() and '..' not in relative.parts and relative.parts[0] != 'lib' and name not in checks
    path = source / name
    assert path.is_file() and not path.is_symlink()
    assert hashlib.sha256(path.read_bytes()).hexdigest() == digest
    checks[name] = digest
assert set(checks) == {str(path.relative_to(source)) for path in source.rglob('*') if path.is_file() and path.name != 'SHA256SUMS'}
managed = [*checks, 'SHA256SUMS']
# The dependency package may already own prefix/lib. Never recurse into it,
# replace it, or infer that its files are part of this service package.
for directory in (prefix, state):
    if directory.exists():
        info = directory.lstat()
        assert stat.S_ISDIR(info.st_mode) and info.st_uid == 0 and not info.st_mode & 0o022
    else:
        directory.mkdir(mode=0o700 if directory == state else 0o755)
for name in managed:
    destination = prefix / name
    if destination.exists():
        assert not destination.is_symlink() and destination.read_bytes() == (source / name).read_bytes(), 'Existing managed file differs: ' + name
    for parent in destination.parents:
        if parent == prefix: break
        if parent.exists():
            info = parent.lstat()
            assert stat.S_ISDIR(info.st_mode) and info.st_uid == 0 and not info.st_mode & 0o022
if unit.exists():
    assert not unit.is_symlink() and unit.read_bytes() == (source / unit.name).read_bytes()
if a.token_file.exists():
    info = a.token_file.lstat()
    assert stat.S_ISREG(info.st_mode) and info.st_uid == 0 and not info.st_mode & 0o077
    token = a.token_file.read_text().strip()
    assert len(token) >= 32 and token.isascii()
else:
    assert a.token_file == state / 'access.token', 'The selected debug token must already exist'
    token = secrets.token_urlsafe(32)
    with os.fdopen(os.open(a.token_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w') as file:
        file.write(token + '\n')
for name in managed:
    destination = prefix / name
    destination.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
    shutil.copyfile(source / name, destination)
    os.chown(destination, 0, 0)
    destination.chmod(0o755 if name in ('nv12-capture', 'mix-video-server') else 0o644)
environment = 'MIX_VIDEO_ADDRESS=' + str(ip) + '\nMIX_VIDEO_TOKEN_FILE=' + str(a.token_file) + '\n'
for name, value in [('service.env', environment), ('viewer-url.txt', 'http://' + str(ip) + ':8091/\n')]:
    destination = state / name
    if destination.exists():
        info = destination.lstat()
        assert stat.S_ISREG(info.st_mode) and info.st_uid == 0 and not info.st_mode & 0o077
    with os.fdopen(os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600), 'w') as file:
        file.write(value)
shutil.copyfile(prefix / unit.name, unit)
os.chown(unit, 0, 0)
unit.chmod(0o644)
subprocess.run(['systemctl', 'daemon-reload'], check=True)
print(json.dumps({'installed': str(prefix), 'service': unit.name, 'private_lib_unchanged': True,
                  'token_file': str(a.token_file), 'url_file': str(state / 'viewer-url.txt'),
                  'started': False, 'enabled': False}))
