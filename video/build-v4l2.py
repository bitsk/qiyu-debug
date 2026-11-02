#!/usr/bin/env python3
"""Build the private 1.24.2 V4L2 plugin with balanced encoder pool references."""
import argparse
import ast
import configparser
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[2]
SHA = '6e347c72d4b8b2886d890ffe9f6767a9edb02f201588e8c3a572dcd08d9852bd'
URL = 'https://gstreamer.freedesktop.org/src/gst-plugins-good/gst-plugins-good-1.24.2.tar.xz'
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--archive', required=True, type=Path)
p.add_argument('--output', required=True, type=Path)
a = p.parse_args()
out = a.output.resolve()
assert out.is_relative_to(ROOT / 'build') and not out.exists()
assert hashlib.sha256(a.archive.read_bytes()).hexdigest() == SHA
out.mkdir(parents=True)
with tarfile.open(a.archive) as archive:
    for entry in archive:
        path = (out / entry.name).resolve()
        assert path.is_relative_to(out)
        if entry.isdir():
            path.mkdir(parents=True, exist_ok=True)
        elif entry.isfile():
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(archive.extractfile(entry).read())
        else:
            raise ValueError('Unexpected non-regular archive entry')
src = out / 'gst-plugins-good-1.24.2'
encoder = src / 'sys/v4l2/gstv4l2videoenc.c'
before = encoder.read_text()
old = '''      if (opool)
        gst_object_unref (opool);
    }
  }

  if (task_state == GST_TASK_STOPPED'''
new = '''    }
    if (opool)
      gst_object_unref (opool);
  }

  if (task_state == GST_TASK_STOPPED'''
assert before.count(old) == 1
encoder.write_text(before.replace(old, new))
config = configparser.ConfigParser()
config.read(ROOT / 'build/steamos-turnip-split-kms-r1/deps/frame-aarch64.ini')
headers = ROOT / 'build/steamos-gst-webrtc-r1/frame-generated-headers'
assert (headers / 'glibconfig.h').is_file()
config['built-in options']['c_args'] = repr(
    ast.literal_eval(config['built-in options']['c_args']) + ['-I' + str(headers)])
cross = out / 'cross.ini'
with cross.open('w') as f:
    config.write(f)
command = ['meson', 'setup', str(out / 'obj'), str(src), '--cross-file', str(cross),
           '--wrap-mode=nodownload', '--buildtype=release', '-Dauto_features=disabled',
           '-Dv4l2=enabled', '-Dorc=disabled', '-Dtests=disabled', '-Ddoc=disabled']
with (out / 'build.log').open('w') as log:
    subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
    subprocess.run(['ninja', '-C', str(out / 'obj'), 'sys/v4l2/libgstvideo4linux2.so'],
                   stdout=log, stderr=subprocess.STDOUT, check=True)
plugin = out / 'obj/sys/v4l2/libgstvideo4linux2.so'
manifest = {'source_url': URL, 'source_sha256': SHA,
            'fix': 'Unref opool on every frame, including the already-active path',
            'plugin': str(plugin), 'plugin_sha256': hashlib.sha256(plugin.read_bytes()).hexdigest(),
            'meson_argv': command, 'device_tested': False}
(out / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
print(json.dumps(manifest, indent=2))
