'use strict';
(() => {
  const grid = document.getElementById('camera-grid');
  const toggle = document.getElementById('camera-toggle');
  const summary = document.getElementById('camera-summary');
  const note = document.getElementById('camera-note');
  const crosshairBtn = document.getElementById('camera-crosshair');
  let running = false, generation = 0, revision = 0, request = null, lastStatus = 0, timer = null;

  const groups = ['手柄短曝光 · 相位 0', '手柄短曝光 · 相位 1', 'SLAM 长曝光 · 相位 2', '用途未确认', '透视'];
  const rows = Array.from({length: 18}, (_, id) => {
    const group = id < 16 ? Math.floor(id/4) : 4;
    const slot = id < 16 ? id%4 : id-12;
    if (id%4 === 0) {
      const heading = document.createElement('h3');
      heading.className = 'camera-group-title'; heading.dataset.kind = ['controller','controller','slam','unclassified','imx'][group];
      heading.textContent = groups[group]; grid.append(heading);
    }
    const card = document.createElement('article');
    card.className = 'camera-card';
    card.dataset.kind = ['controller','controller','slam','unclassified','imx'][group];
    card.dataset.stream = String(id);
    card.dataset.live = 'false';

    const dot = document.createElement('span');
    dot.className = 'dot idle';
    const title = document.createElement('h3');
    title.textContent = slot < 4 ? `相机 ${slot} · ${group < 3 ? '相位 '+group : '未分类'}` : `透视${slot === 4 ? '右' : '左'} · ${slot} · IMX471`;
    title.prepend(dot);

    const headRight = document.createElement('div');
    headRight.className = 'panel-actions';
    const lumaBadge = document.createElement('span');
    lumaBadge.className = 'pill subtle mono';
    lumaBadge.textContent = 'Y —';
    const full = document.createElement('button');
    full.type = 'button';
    full.textContent = '放大';
    full.setAttribute('aria-label', `${title.textContent} 全屏`);
    full.onclick = async () => {
      try { if (document.fullscreenElement === card) await document.exitFullscreen(); else await card.requestFullscreen(); }
      catch { label.textContent = '当前浏览器无法全屏'; }
    };
    headRight.append(lumaBadge, full);

    const head = document.createElement('div');
    head.className = 'camera-card-head';
    head.append(title, headRight);

    const view = document.createElement('div');
    view.className = 'camera-view';
    const canvas = document.createElement('canvas');
    canvas.hidden = true;
    canvas.setAttribute('aria-label', title.textContent);
    const crosshair = document.createElement('div');
    crosshair.className = 'camera-crosshair-overlay';
    const empty = document.createElement('p');
    empty.className = 'camera-empty';
    empty.textContent = '尚未观看';
    const label = document.createElement('p');
    label.className = 'camera-metrics';
    label.textContent = '等待真实图像';

    view.append(canvas, crosshair, empty);
    card.append(head, view, label);
    grid.append(card);
    return {id, slot, card, dot, lumaBadge, canvas, empty, label, context: canvas.getContext('2d'),
      expires: 0, session: null, sequence: null, timestamps: [], image: null};
  });

  if (crosshairBtn) {
    crosshairBtn.onclick = () => {
      const active = grid.classList.toggle('show-crosshair');
      crosshairBtn.setAttribute('aria-pressed', String(active));
    };
  }
  document.querySelectorAll('[data-cam-filter]').forEach(btn => {
    btn.addEventListener('click', () => {
      document.querySelectorAll('[data-cam-filter]').forEach(b => b.classList.toggle('active', b === btn));
      grid.dataset.filter = btn.dataset.camFilter;
      generation++; clearTimeout(timer); request?.abort(); revision = 0;
      clearAll(running ? '等待所选用途的新帧' : '尚未观看');
      if (running && !document.hidden) loop();
    });
  });

  function streamMask() {
    const filter = grid.dataset.filter;
    return rows.reduce((mask, row) => mask | ((!filter || filter === 'all' || filter === 'live' || row.card.dataset.kind === filter) ? (1 << row.id) : 0), 0);
  }

  function clear(row, message) {
    row.canvas.hidden = true; row.empty.hidden = false; row.empty.textContent = message;
    row.card.dataset.live = 'false';
    row.dot.className = 'dot idle';
    row.lumaBadge.textContent = 'Y —';
    row.expires = 0; row.timestamps = []; row.session = null; row.sequence = null;
    row.label.textContent = '— fps · 无实时图像';
    row.context.clearRect(0, 0, row.canvas.width, row.canvas.height);
  }
  function clearAll(message) { rows.forEach(row => clear(row, message)); }

  function consume(buffer, serverNow, elapsed) {
    const bytes = new Uint8Array(buffer), view = new DataView(buffer);
    let offset = 0;
    while (offset < bytes.length) {
      if (bytes.length-offset < 48) throw Error('invalid_camera_packet');
      const magic = view.getUint32(offset, true), header = magic === 0x32504341 ? 104 : magic === 0x31504341 ? 48 : 0;
      if (!header || bytes.length-offset < header) throw Error('invalid_camera_header');
      const slot = view.getUint32(offset+4, true), session = view.getBigUint64(offset+8, true).toString();
      const sequence = view.getBigUint64(offset+16, true).toString(), stamp = view.getBigUint64(offset+24, true);
      const width = view.getUint32(offset+32, true), height = view.getUint32(offset+36, true);
      const channels = view.getUint32(offset+40, true), length = view.getUint32(offset+44, true);
      if (slot > 5 || width < 1 || width > 640 || height < 1 || height > 480 ||
          channels !== (slot < 4 ? 1 : 3) || length !== width*height*channels || offset+header+length > bytes.length) throw Error('invalid_camera_layout');
      const phase = header === 104 ? view.getUint32(offset+48, true) : 0xffffffff;
      const flags = header === 104 ? view.getUint32(offset+52, true) : 0;
      const exposure = header === 104 ? view.getBigUint64(offset+56, true) : 0n;
      const sof = header === 104 ? view.getBigUint64(offset+72, true) : 0n;
      if (flags & ~3 || (flags & 2 && (!(flags & 1) || phase > 2 || slot >= 4 || !exposure || !sof || sof > stamp))) throw Error('invalid_phase_metadata');
      const id = slot >= 4 ? slot+12 : flags & 2 ? phase*4+slot : slot+12;
      const row = rows[id], age = Number(serverNow-stamp)/1e6;
      // Clear all phases of this sensor on a producer generation change.
      for (const other of rows) if (other.slot === slot && other.session && other.session !== session) clear(other, '生产者已重启');
      if (age < 0 || age+elapsed >= 500) { clear(row, '图像已过期'); offset += header+length; continue; }
      if (row.session === session && row.sequence === sequence) { offset += header+length; continue; }
      if (row.session !== session) row.timestamps = [];
      if (row.canvas.width !== width || row.canvas.height !== height || !row.image) {
        row.canvas.width = width; row.canvas.height = height; row.image = row.context.createImageData(width, height);
      }
      const rgba = row.image.data;
      let lumaSum = 0;
      const totalPixels = width * height;
      for (let pixel = 0, source = offset+header; pixel < totalPixels; pixel++, source += channels) {
        const dest = pixel*4;
        const r = bytes[source];
        const g = bytes[source+(channels === 3 ? 1 : 0)];
        const b = bytes[source+(channels === 3 ? 2 : 0)];
        rgba[dest] = r; rgba[dest+1] = g; rgba[dest+2] = b; rgba[dest+3] = 255;
        lumaSum += channels === 3 ? ((r * 77 + g * 150 + b * 29) >> 8) : r;
      }
      row.context.putImageData(row.image, 0, 0);
      row.canvas.hidden = false; row.empty.hidden = true;
      row.card.dataset.live = 'true';
      row.dot.className = 'dot';
      row.lumaBadge.textContent = `Y ${Math.round(lumaSum / totalPixels)}`;
      row.session = session; row.sequence = sequence; row.expires = performance.now()+500-age-elapsed;
      row.timestamps.push(Number(stamp)/1e6); if (row.timestamps.length > 30) row.timestamps.shift();
      const times = row.timestamps, fps = times.length > 1 ? (times.length-1)*1000/(times.at(-1)-times[0]) : null;
      const exposureText = exposure ? ` · E ${(Number(exposure)/1000).toFixed(2)} µs` : ' · 曝光未知';
      const timingText = flags & 2 ? ` · SOF→取帧 ${(Number(stamp-sof)/1e6).toFixed(2)} ms` : ' · 无有效三相关联';
      row.label.textContent = `${width} × ${height} · ${fps === null ? '—' : fps.toFixed(1)} fps 到达 · 帧 ${sequence}${slot < 4 ? exposureText+timingText : ''}`;
      offset += header+length;
    }
  }

  async function loop() {
    const current = generation;
    if (!running || document.hidden) return;
    const started = performance.now();
    const controller = new AbortController(); request = controller;
    const timeout = setTimeout(() => controller.abort(), 2000);
    try {
      const r = await fetch(`/api/cameras/frames?after=${revision}&streams=${streamMask()}`, {headers: headers(), signal: controller.signal, cache: 'no-store'});
      if (!r.ok) throw Error(r.status === 503 ? '当前为离线模式，实时摄像头未开启' : r.status === 401 ? '访问密钥无效' : `HTTP ${r.status}`);
      const buffer = await r.arrayBuffer();
      if (generation !== current || !running || document.hidden) return;
      const next = Number(r.headers.get('X-Camera-Revision'));
      if (!Number.isSafeInteger(next) || next < 0) throw Error('invalid_camera_revision');
      // A restarted observer starts at revision zero; next request follows it.
      revision = next;
      if (r.status !== 204) {
        const serverWait = Number(r.headers.get('X-Camera-Wait-Ns') || 0)/1e6;
        if (!Number.isFinite(serverWait) || serverWait < 0) throw Error('invalid_camera_wait');
        // Exclude server long-poll waiting, retain network/send/decode time as an age bound.
        consume(buffer, BigInt(r.headers.get('X-Camera-Monotonic-Ns')), Math.max(0, performance.now()-started-serverWait));
      }
      if (performance.now()-lastStatus > 1000) {
        lastStatus = performance.now();
        const status = await fetch('/api/cameras', {headers: headers(), signal: controller.signal}).then(r => {
          if (!r.ok) throw Error(`HTTP ${r.status}`); return r.json();
        });
        if (generation !== current) return;
        const count = status.streams.filter(c => c.state === 'live' && (streamMask() & (1 << c.id))).length;
        summary.textContent = `${count} 路用途流有新图像 · Rust`;
        summary.className = 'pill subtle ' + (count > 0 ? 'live' : '');
        note.textContent = status.error ? `预览源：${status.error}` : '同一相机按 QCFT 三相分流：短 0、短 1、长 2；短相不等于左右手。用途未确认帧单列；相位关联不代表 LED 同步或定位通过。原始像素预览，非 H.264。';
      }
    } catch (error) {
      if (generation === current && running && !document.hidden) {
        clearAll('无实时图像'); summary.textContent = error.message; summary.className = 'pill subtle stale';
      }
    } finally {
      clearTimeout(timeout);
      if (generation === current && running && !document.hidden)
        timer = setTimeout(loop, Math.max(10, 1000/15-(performance.now()-started)));
    }
  }

  toggle.onclick = () => {
    running = !running; generation++; clearTimeout(timer); request?.abort(); revision = 0;
    grid.dataset.running = String(running);
    toggle.textContent = running ? '停止摄像头预览' : '原始像素诊断预览'; toggle.setAttribute('aria-pressed', String(running));
    clearAll(running ? '等待已开启的相机' : '预览已停止');
    summary.textContent = running ? '正在订阅' : '尚未观看';
    summary.className = 'pill subtle ' + (running ? 'live' : '');
    if (running) loop();
  };
  document.addEventListener('visibilitychange', () => {
    generation++; clearTimeout(timer); request?.abort(); clearAll(document.hidden ? '页面已隐藏' : '等待新图像');
    if (running && !document.hidden) { revision = 0; loop(); }
  });
  window.addEventListener('mix-video-off', () => { if(running) toggle.click(); });
  window.addEventListener('pagehide', () => { running = false; generation++; clearTimeout(timer); request?.abort(); clearAll('预览已停止'); });
  setInterval(() => {
    for (const row of rows) if (row.expires && performance.now() >= row.expires) clear(row, '图像已过期 · 等待新帧');
  }, 100);
})();
