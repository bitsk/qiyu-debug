'use strict';
(() => {
  const root = document.getElementById('phase-video-grid');
  const controlLabel = document.getElementById('encoder-state');
  const evidence = {format:'mix-phase-video-observation-v1', physical_present_verified:false,
    device_performance_verified:false, started:new Date().toISOString(), samples:[], errors:[]};
  const rows = [];
  let control = null, controlBusy = false, pollBusy = false, cpu = null;
  function renderControl() {
    const notice=document.getElementById('video-control-notice');
    notice.hidden=control?.enabled===true;
    document.getElementById('video-control-title').textContent=!control?'正在确认视频控制状态':control.offline?'离线模式未连接设备':'全部视频已手动关闭';
    document.getElementById('video-control-description').textContent=!control?'暂时无法开始观看，连接恢复后会自动更新状态。':control.offline?'连接设备后才能恢复视频启动权限。':`${control.release_confirmed?'编码器已释放。':'编码器正在释放或仍有其他占用。'}先恢复权限，再点击“开始观看”或启动三相视频。恢复视频不会启动追踪。`;
    for(const id of ['video-resume','video-resume-screen'])document.getElementById(id).disabled=controlBusy || !control || control.enabled || control.offline;
    document.getElementById('video-frame')?.contentWindow?.postMessage({type:'mix-video-control',enabled:control?.enabled??null,offline:!!control?.offline},location.origin);
  }
  const fmt = x => Number.isFinite(x) ? x.toFixed(1) : '—';
  const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
  async function api(path, method='GET', body, keepalive=false) {
    const t0=performance.now();
    const response = await fetch(path, {method, cache:'no-store', headers:body?{'Content-Type':'application/json'}:{},
      body:body?JSON.stringify(body):undefined, signal:keepalive?undefined:AbortSignal.timeout(10000), keepalive});
    const data = response.status===204 ? {} : await response.json();
    if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`);
    return {data,t0,t3:performance.now()};
  }
  function detached(row) {
    row.version++; row.pc?.close(); row.pc=null; row.id=null; row.previous=null;
    row.video.srcObject=null; row.video.hidden=true; row.matches.clear(); row.frames=[];
    row.presented=0; row.lastPresent=0; row.offset=null; row.seenIce=0;
  }
  async function stop(row) {
    const id=row.id || row.server?.id;
    detached(row);
    if(id) await api(`/api/cameras/webrtc/${id}`,'DELETE').catch(e=>{if(e.message!=='session_not_found')throw e;});
    row.server=null; row.state.textContent='已停止'; row.stats.textContent='编码器已释放（本路会话）';
  }
  async function begin(row, restart=false) {
    if(row.busy)return;
    row.busy=true; row.state.textContent=restart?'正在重启':'正在启动'; updateButtons(row);
    try {
      if(restart) await stop(row);
      const created=await api('/api/cameras/webrtc','POST',{phase:row.phase,fps:Number(row.fps.value),bitrate:Number(row.bitrate.value)*1000000});
      row.id=created.data.id; row.server=created.data;
      const version=++row.version;
      row.pc=new RTCPeerConnection({iceServers:[]});
      const pc=row.pc;
      pc.ontrack=event=>{if(row.version!==version)return;row.video.srcObject=event.streams[0] || new MediaStream([event.track]);row.video.play().catch(()=>{});};
      pc.onicecandidate=event=>{
        if(event.candidate && row.version===version) api(`/api/cameras/webrtc/${row.id}/ice`,'POST',event.candidate.toJSON()).catch(e=>{row.state.textContent=e.message;});
      };
      pc.onconnectionstatechange=()=>{if(row.version===version)row.state.textContent=pc.connectionState;};
      let status=created.data;
      for(let count=0; !status.offer && count<30; count++){
        await wait(200); if(row.version!==version)return;
        status=(await api(`/api/cameras/webrtc/${row.id}`)).data;
      }
      if(row.version!==version)return;
      if(!status.offer)throw new Error('SDP offer 超时');
      await pc.setRemoteDescription({type:'offer',sdp:status.offer});
      await pc.setLocalDescription(await pc.createAnswer());
      await api(`/api/cameras/webrtc/${row.id}/answer`,'POST',{sdp:pc.localDescription.sdp});
      row.seenIce=0; await incomingIce(row,status);
      const presented = (now, meta) => {
        if(row.version!==version)return;
        row.presented++; row.lastPresent=now; row.video.hidden=false;
        if(meta.rtpTimestamp!==undefined){row.frames.push({rtp:meta.rtpTimestamp,now});if(row.frames.length>120)row.frames.shift();}
        row.callback=row.video.requestVideoFrameCallback(presented);
      };
      if(row.video.requestVideoFrameCallback)row.callback=row.video.requestVideoFrameCallback(presented);
      row.state.textContent='等待本相位画面';
    } catch(error) {
      evidence.errors.push({phase:row.phase,at:new Date().toISOString(),error:error.message,operation:"start"});if(evidence.errors.length>64)evidence.errors.shift();
      const id=row.id;
      detached(row);
      if(id)await api(`/api/cameras/webrtc/${id}`,'DELETE').catch(()=>{});
      row.state.textContent=error.message;
    } finally {row.busy=false;updateButtons(row);}
  }
  async function incomingIce(row,status){
    if(!row.pc?.remoteDescription)return;
    const pc=row.pc, version=row.version, candidates=status.ice || [];
    while(row.seenIce<candidates.length){
      if(row.pc!==pc || row.version!==version)return;
      await pc.addIceCandidate(candidates[row.seenIce]);
      if(row.pc!==pc || row.version!==version)return;
      row.seenIce++;
    }
  }
  function updateButtons(row){
    row.start.disabled=row.busy || !control?.enabled || !!row.id || row.server?.state==='running';
    row.restart.disabled=row.busy || !control?.enabled;
    row.stop.disabled=row.busy || (!row.id && row.server?.state!=='running');
    row.fps.disabled=row.bitrate.disabled=row.busy || !!row.id;
  }
  for(let phase=0; phase<3; phase++){
    const card=document.createElement('article');card.className='phase-video-card';
    card.innerHTML=`<h3>${phase===2?'SLAM 长曝光':'手柄短曝光'} · 相位 ${phase}</h3>
      <p class="muted">上：相机 0 / 1　下：相机 2 / 3 · 640×480</p>
      <div class="phase-video-view"><video autoplay muted playsinline hidden aria-label="相位 ${phase} 视频"></video><p class="phase-state" role="status">未启动</p></div>
      <div class="phase-actions"><label>上限 <select class="phase-fps" aria-label="相位 ${phase} 帧率上限"><option>30</option><option>25</option><option>15</option></select> fps</label>
      <label>码率 <select class="phase-bitrate" aria-label="相位 ${phase} 码率"><option>3</option><option>1</option><option>2</option><option>4</option><option>6</option><option>8</option></select> Mbps</label>
      <button class="phase-start" type="button">启动</button><button class="phase-stop" type="button">停止</button><button class="phase-restart" type="button">重启</button></div>
      <p class="phase-stats mono">尚无实测数据</p>`;
    root.append(card);
    const row={phase,card,video:card.querySelector('video'),state:card.querySelector('.phase-state'),stats:card.querySelector('.phase-stats'),
      start:card.querySelector('.phase-start'),stop:card.querySelector('.phase-stop'),restart:card.querySelector('.phase-restart'),
      fps:card.querySelector('.phase-fps'),bitrate:card.querySelector('.phase-bitrate'),version:0,id:null,pc:null,busy:false,
      presented:0,lastPresent:0,frames:[],matches:new Map(),offset:null,seenIce:0,previous:null,server:null};
    row.start.onclick=()=>begin(row);
    row.restart.onclick=()=>begin(row,true);
    row.stop.onclick=async()=>{row.busy=true;updateButtons(row);try{await stop(row);}catch(e){row.state.textContent=e.message;}finally{row.busy=false;updateButtons(row);}};
    rows.push(row);updateButtons(row);
  }
  async function measure(row,result,version){
    const s=result.data;row.server=s;
    if(s.error)row.state.textContent=s.error;
    await incomingIce(row,s);
    if(row.version!==version)return;
    const span=result.t3-result.t0;
    if(!row.offset || span<row.offset.span || result.t3-row.offset.at>30000){
      const server=s.server_monotonic_ns/1e6;
      row.offset={low:result.t0-server,high:result.t3-server,span,at:result.t3};
    }
    for(const source of s.rtp_sources || [])row.matches.set(source.rtp_timestamp,source);
    while(row.matches.size>120)row.matches.delete(row.matches.keys().next().value);
    const delays=row.frames.map(f=>{const m=row.matches.get(f.rtp);return m?{low:f.now-m.dq_ns/1e6-row.offset.high,high:f.now-m.dq_ns/1e6-row.offset.low}:null;}).filter(Boolean);
    const stats=row.pc?await row.pc.getStats():new Map();
    if(row.version!==version)return;
    let received=0,decoded=0,dropped=0,decodeTotal=0,jitterTotal=0,jitterCount=0,rtt=null;
    stats.forEach(r=>{
      if(r.type==='inbound-rtp' && (r.kind==='video' || r.mediaType==='video')){
        received+=r.framesReceived||0;decoded+=r.framesDecoded||0;dropped+=r.framesDropped||0;
        decodeTotal+=r.totalDecodeTime||0;jitterTotal+=r.jitterBufferDelay||0;jitterCount+=r.jitterBufferEmittedCount||0;
      }
      if(r.type==='candidate-pair' && r.state==='succeeded' && r.nominated)rtt=(r.currentRoundTripTime??NaN)*1000;
    });
    const current={time:result.t3,synced:s.synced_groups,submitted:s.submitted,encoded:s.encoded,received,decoded,presented:row.presented,decodeTotal,jitterTotal,jitterCount};
    const prev=row.previous;row.previous=current;
    const dt=prev?(current.time-prev.time)/1000:0;
    const rate=key=>dt>0?(current[key]-prev[key])/dt:NaN;
    const decodeMs=prev && decoded>prev.decoded?(decodeTotal-prev.decodeTotal)*1000/(decoded-prev.decoded):NaN;
    const jitterMs=prev && jitterCount>prev.jitterCount?(jitterTotal-prev.jitterTotal)*1000/(jitterCount-prev.jitterCount):NaN;
    const latency=delays.length && row.lastPresent && performance.now()-row.lastPresent<1000?delays[delays.length-1]:null;
    const stale=s.stale || latency?.low>500 || !row.lastPresent || performance.now()-row.lastPresent>500;
    row.video.hidden=stale;
    if(stale)row.state.textContent='等待 / 画面已过期';else row.state.textContent='实时 · H.264 / WebRTC';
    row.stats.textContent=`源 0/1/2/3 ${(s.camera_ingress||[]).map(x=>fmt(x.preview_fps)).join(' / ')} fps\n配组 ${fmt(rate('synced'))} / 提交 ${fmt(rate('submitted'))} / 编码 ${fmt(rate('encoded'))} fps\n接收 ${fmt(rate('received'))} / 解码 ${fmt(rate('decoded'))} / 回调 ${fmt(rate('presented'))} fps · 丢帧 ${dropped}\nDQ→编码 P50/P95 ${fmt(s.dq_to_encoded_ms?.p50)} / ${fmt(s.dq_to_encoded_ms?.p95)} ms\n解码 ${fmt(decodeMs)} ms · 抖动缓冲 ${fmt(jitterMs)} ms · RTT ${fmt(rtt)} ms\nDQ→浏览器回调估计 ${latency?`${fmt(latency.low)}～${fmt(latency.high)} ms`:'—（等待 RTP 时间戳匹配）'}`;
    evidence.samples.push({wall_time:new Date().toISOString(),phase:row.phase,server:{...s,rtp_sources:undefined,offer:undefined,ice:undefined},backend_cpu:evidence.backend_cpu,browser:{...current,dropped,rtt_ms:rtt,decode_ms:decodeMs,jitter_ms:jitterMs,dq_to_callback_estimate_ms:latency,clock_rtt_ms:span}});
    if(evidence.samples.length>900)evidence.samples.shift();
  }
  async function poll(){
    if(pollBusy)return;pollBusy=true;
    try{
      const [controlResult,phaseResult]=await Promise.all([api('/api/video-control'),api('/api/cameras/webrtc')]);
      control=controlResult.data;const phase=phaseResult.data;
      renderControl();
      const elapsed=cpu?(phase.server_monotonic_ns-cpu.time)/1e9:0;
      const cpuPercent=elapsed>0?(phase.process_cpu_seconds-cpu.seconds)/elapsed*100:NaN;
      cpu={seconds:phase.process_cpu_seconds,time:phase.server_monotonic_ns};
      evidence.control=control;evidence.backend_cpu={percent_one_core:cpuPercent,process_cpu_seconds:cpu.seconds};
      const owners=[...(control.leases||[]).map(x=>x.owner),
        ...(control.encoder_owners||[]).filter(x=>!x.managed).map(x=>`未管理进程 ${x.pid}`),
        ...(control.owner_scan_complete===false?['句柄扫描不完整']:[])].join('、') || '无';
      controlLabel.textContent=control.offline?'离线预览 · 未访问设备':`${control.enabled?'允许启动':'手动关闭已锁定'} · 配额 ${control.leases.length}/${control.limit} · 占用 ${owners} · 调试进程 CPU ${fmt(cpuPercent)}%（单核=100%）${!control.enabled?` · ${control.release_confirmed?'编码器句柄已全部释放':'等待释放 / 存在其他占用'}`:''}`;
      document.getElementById('video-resume').disabled=controlBusy || control.enabled || control.offline;
      document.getElementById('video-off').disabled=controlBusy || control.offline;
      document.getElementById('screen-stop').disabled=controlBusy || control.offline;
      document.getElementById('encoder-limit').disabled=controlBusy || control.offline || control.leases.length>0;
      document.getElementById('encoder-limit').value=String(control.limit);
      for(const row of rows){
        const shared=phase.phases?.[row.phase];
        row.server=shared;
        if(!control.enabled && row.id){detached(row);row.state.textContent='全部视频已手动关闭';}
        if(row.id && !row.busy){
          const id=row.id, version=row.version;
          try{
            const result=await api(`/api/cameras/webrtc/${id}`);
            if(row.id===id && row.version===version && !row.busy)await measure(row,result,version);
          }catch(e){if(row.version===version && !row.busy){evidence.errors.push({phase:row.phase,at:new Date().toISOString(),error:e.message,operation:"poll"});if(evidence.errors.length>64)evidence.errors.shift();detached(row);row.state.textContent=`已结束：${e.message}`;}}
        }else if(!row.id && shared?.state==='running'){row.state.textContent='另一个页面持有本相位；可停止或重启';}else if(!row.id&&!row.busy&&shared?.state==='closed'){row.state.textContent=`已结束：${shared.error||'已停止'}`;}
        updateButtons(row);
      }
    }catch(e){control=null;renderControl();rows.forEach(updateButtons);controlLabel.textContent=`控制状态不可用：${e.message}`;}
    finally{pollBusy=false;}
  }
  async function configure(action,limit){
    if(controlBusy)return;controlBusy=true;renderControl();
    try{
      control=(await api('/api/video-control','POST',{action,limit})).data;
      if(action==='off'){
        rows.forEach(row=>{detached(row);row.state.textContent='全部视频已手动关闭';});
        window.dispatchEvent(new Event('mix-video-off'));
        document.getElementById('video-frame')?.contentWindow?.postMessage({type:'mix-video-stop',reason:'all-off'},location.origin);
      }
    }catch(e){controlLabel.textContent=e.message;}
    finally{controlBusy=false;renderControl();await poll();}
  }
  document.getElementById('video-off').onclick=()=>configure('off');
  document.getElementById('video-resume').onclick=()=>configure('resume');
  document.getElementById('video-resume-screen').onclick=()=>configure('resume');
  document.getElementById('encoder-limit').onchange=e=>configure('limit',Number(e.target.value));
  document.getElementById('screen-stop').onclick=async()=>{try{await api('/video/api/stop','POST',{});document.getElementById('video-frame')?.contentWindow?.postMessage({type:'mix-video-stop',reason:'screen-stop'},location.origin);await poll();}catch(e){controlLabel.textContent=e.message;}};
  document.getElementById('phase-export').onclick=()=>{
    const blob=new Blob([JSON.stringify(evidence,null,2)],{type:'application/json'});
    const url=URL.createObjectURL(blob),a=document.createElement('a');a.href=url;a.download=`mix-phase-video-${Date.now()}.json`;a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);
  };
  window.addEventListener('pagehide',()=>rows.forEach(row=>{if(row.id){const id=row.id;detached(row);api(`/api/cameras/webrtc/${id}`,'DELETE',undefined,true).catch(()=>{});}}));
  setInterval(()=>rows.forEach(row=>{if(row.lastPresent && performance.now()-row.lastPresent>500)row.video.hidden=true;}),200);
  setInterval(poll,1000);poll();
})();
