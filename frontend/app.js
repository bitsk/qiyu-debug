'use strict';
const $ = id => document.getElementById(id);
const fragment = new URLSearchParams(location.hash.slice(1));
let state = null, received = 0, paused = true, lastImage = null;
let previewTransport = 'webrtc', videoReady = false, videoToken = '';
let history = [], lastSequence = null, previousSession = null, deliveryAgeBound = 0;
let yaw = .65, pitch = .38, distance = 1.65, drag = null;
const colors = ['#f27d88','#42dca8','#6cb6ff'];
if(fragment.has('token')) historyReplace();
function historyReplace() { window.history.replaceState(null,'',location.pathname); }
const num = (x,d=2) => typeof x === 'number' && Number.isFinite(x) ? x.toFixed(d) : '—';
const vec = (v,d=3) => Array.isArray(v) ? v.map(x=>num(x,d)).join('  ') : '—';
function headers(){return {};}

function quatToEulerDeg(q) {
  if (!Array.isArray(q) || q.length < 4 || !q.every(Number.isFinite)) return null;
  const [x, y, z, w] = q;
  const sinrCosp = 2 * (w * x + y * z);
  const cosrCosp = 1 - 2 * (x * x + y * y);
  const roll = Math.atan2(sinrCosp, cosrCosp) * (180 / Math.PI);
  const sinp = 2 * (w * y - z * x);
  const pitchDeg = (Math.abs(sinp) >= 1 ? Math.sign(sinp) * (Math.PI / 2) : Math.asin(sinp)) * (180 / Math.PI);
  const sinyCosp = 2 * (w * z + x * y);
  const cosyCosp = 1 - 2 * (y * y + z * z);
  const yawDeg = Math.atan2(sinyCosp, cosyCosp) * (180 / Math.PI);
  return [roll, pitchDeg, yawDeg];
}

function updateVideoStandby() {
  const standby = $('video-standby');
  if (!standby) return;
  const showStandby = previewTransport === 'webrtc' && !videoReady;
  standby.hidden = !showStandby;
  if (showStandby) {
    const mode = state?.mode;
    $('video-standby-title').textContent = mode === 'replay'
      ? '历史回放模式 · 实时 WebRTC 已禁用'
      : mode === 'offline'
      ? '离线验证模式 · 实时 WebRTC 未开启'
      : '等待独立 8091 WebRTC 视频服务就绪';
    $('video-standby-desc').textContent = mode === 'replay'
      ? '回放模式不连接实时 8091 视频流。若传入了 --image 历史截图，可切换至 JPEG 模式查看。'
      : mode === 'offline'
      ? '当前以 --offline 运行，不探测设备或连接 8091 编码服务。可切换至 JPEG 维护回退检查界面。'
      : '同源 /video/ 代理正在等待本机 8091 WebRTC 服务响应；您也可以切换至 JPEG 维护回退。';
  }
}

function videoMessage(type, value={}) {
  $('video-frame').contentWindow?.postMessage({type, ...value}, location.origin);
}
function authenticateVideo() {
  if(videoReady && state && !videoToken) {
    // Public same-origin proxy supplies its private upstream credential.
    // This marker only unlocks the embedded player's existing UI.
    const marker = 'public-debug-console-same-origin-video';
    videoMessage('mix-video-auth', {token: marker}); videoToken = marker;
  }
}
$('video-frame').addEventListener('load', () => {
  videoReady = false; videoToken = ''; videoMessage('mix-video-parent-ready');
  updateVideoStandby();
});
window.addEventListener('message', event => {
  if(event.origin !== location.origin || event.source !== $('video-frame').contentWindow) return;
  const data = event.data;
  if(!data || typeof data !== 'object') return;
  if(data.type === 'mix-video-ready') {
    videoReady = true;
    authenticateVideo();
    updateVideoStandby();
  }
  if(data.type === 'mix-video-size' && Number.isInteger(data.height) && data.height > 0) {
    $('video-frame').style.height = Math.max(280, Math.min(1800, data.height)) + 'px';
  }
  if(previewTransport !== 'webrtc') return;
  if(data.type === 'mix-video-status' && typeof data.text === 'string') {
    $('preview-source').textContent = 'WebRTC · ' + data.text.slice(0, 100);
    if ($('video-dot')) $('video-dot').className = 'dot ' + (data.live ? '' : 'amber');
  }
  if(data.type === 'mix-video-stats') {
    if(data.observation)mixDashboard.video(data.observation);
    const r=data.inbound, p=data.presentation;
    $('preview-metrics').textContent=`${num(r?.frameWidth,0)} × ${num(r?.frameHeight,0)} · 解码 ${num(r?.decoded_fps,1)} / 呈现 ${num(p?.presented_fps,1)} fps`;
    const eye=data.observation?.server?.requested?.eye;
    const mode=data.mode==='stereo'?(eye==='left'?'单眼 · 左眼':eye==='right'?'单眼 · 右眼':'横屏左右眼'):data.mode==='desktop'?'维护桌面':'画面模式待确认';
    $('preview-message').textContent=mode+' · 并发扫描读取，非原子截图';
  }
});
videoMessage('mix-video-parent-ready');

async function api(path, options={}){
  const r = await fetch(path,{...options,headers:{...headers(),...options.headers},signal:AbortSignal.timeout(2000)});
  if(!r.ok) throw Error('HTTP '+r.status);
  return r;
}
$('export').onclick=()=>{
  if(!state)return;
  const blob=new Blob([JSON.stringify({...state,exported_at:new Date().toISOString()},null,2)],{type:'application/json'});
  const url=URL.createObjectURL(blob),a=document.createElement('a');a.href=url;a.download='mix-debug-snapshot.json';a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);
};

function updateControllerVisuals(side, c) {
  const m = c.valid_mask || 0;
  const pills = $(side + '-mask-pills');
  if (pills) {
    pills.querySelectorAll('.mask-pill').forEach(pill => {
      const bit = Number(pill.dataset.bit);
      pill.classList.toggle('active', Boolean(c.fresh && (m & bit)));
    });
  }
  const dot = $(side + '-stick-dot');
  if (dot) {
    if (c.fresh && (m & 16) && Array.isArray(c.stock_joystick_raw) && c.stock_joystick_raw.length >= 2) {
      const [rx, ry] = c.stock_joystick_raw;
      // The existing controls ABI is signed int8 XY, not an inferred range.
      let nx = rx / 128, ny = ry / 128;
      nx = Math.max(-1, Math.min(1, nx));
      ny = Math.max(-1, Math.min(1, ny));
      dot.style.left = (50 + nx * 36) + '%';
      dot.style.top = (50 - ny * 36) + '%';
      dot.classList.add('active');
    } else {
      dot.style.left = '50%';
      dot.style.top = '50%';
      dot.classList.remove('active');
    }
  }
}

function renderControllers(s) {
  const elapsed=Math.max(0,performance.now()-received)+deliveryAgeBound;
  const bool=v=>v===true?'是':v===false?'否':'未知';
  for(const c of s.controllers||[]){
    const side=c.hand===0?'left':'right';
    const live=s.mode==='live'&&Number.isFinite(elapsed);
    const fresh=live&&c.fresh&&Number.isFinite(c.age_ms)&&c.age_ms+elapsed<100;
    const statusFresh=live&&c.status_fresh&&Number.isFinite(c.status_age_ms)&&c.status_age_ms+elapsed<1500;
    const tracking=controllerView(c);
    const valid=tracking.valid;
    const controlsFresh=live&&c.input_active&&Number.isFinite(c.controls_age_ms)&&c.controls_age_ms+elapsed<100;
    let m=controlsFresh?c.valid_mask||0:0;
    if(!Number.isFinite(c.mode1_age_ms)||c.mode1_age_ms+elapsed>=100)m&=~15;
    if(!Number.isFinite(c.joystick_age_ms)||c.joystick_age_ms+elapsed>=100)m&=~16;
    const worker=elapsed>=1500?'状态已过期':({running:'运行中',not_started:'未启动',stopped:'已停止',waiting:'等待输出'}[c.worker_state]||'未启动');
    $(side+'-worker').textContent=worker;
    $(side+'-status').textContent=tracking.label;
    $(side+'-status').className='badge '+(valid?(tracking.kind==='predicted'?'predicted':'live'):'');
    $(side+'-link').textContent=statusFresh?`${bool(c.link_present)} / ${bool(c.tracking_enabled)}`:'未知 / 未知';
    $(side+'-state').textContent=`${statusFresh?(c.ctrl_state===5?'5 · 活跃':c.ctrl_state===7?'7 · 休眠':c.ctrl_state??'未知'):'未知'} / ${fresh&&c.clock_valid?'时钟有效':'时钟未确认'}`;
    $(side+'-power').textContent=statusFresh?`${c.head_power_state??'—'} / ${c.gzll_raw_state??'—'}`:'—';
    $(side+'-radio').textContent=live&&c.radio_fresh&&Number.isFinite(c.radio_age_ms)&&c.radio_age_ms+elapsed<1500?`${c.radio_spi_bytes??'—'} bytes · ${num(c.radio_age_ms+elapsed,0)} ms`:'未观测 / 已过期';
    $(side+'-validity').textContent=valid?'有效 / 有效':'无效 / 无效';
    $(side+'-position').textContent=valid?vec(c.position):'—';
    $(side+'-orientation').textContent=valid?vec(c.orientation):'—';
    $(side+'-confidence').textContent=`${fresh?num(c.confidence,2):'—'} / ${Number.isFinite(c.age_ms)?num(c.age_ms+elapsed,0)+' ms':'—'}`;
    $(side+'-buttons').textContent=`${m&1?c.stock_buttons_raw:'—'} / ${m&2?c.stock_touch_raw:'—'}`;
    $(side+'-stick').textContent=m&16?vec(c.stock_joystick_raw,0):'—';
    $(side+'-fingers').textContent=`${m&4?c.stock_index_finger_raw:'—'} / ${m&8?c.stock_middle_finger_raw:'—'}`;
    $(side+'-battery').textContent=controlsFresh&&c.battery_percent!=null?c.battery_percent+'%':'未提供';
    $(side+'-reason').textContent=valid?`共同追踪空间 · ${c.optical_valid?'近期光学更新':'原厂预测姿态'}`:statusFresh&&c.ctrl_state===7?'休眠缓存不作为实时姿态':c.worker_state==='not_started'?'当前会话未启动手柄 SDK':c.worker_state==='stopped'?'会话已结束，输入与定位已释放':'等待当前会话的新鲜有效数据';
    $(side+'-sdk-raw').textContent=c.sdk_position?`原始 SDK XYZ: ${vec(c.sdk_position)}\nWXYZ: ${vec(c.sdk_orientation_wxyz)}\nconfidence: ${num(c.sdk_confidence,2)} · 记录时效 ${num(c.sdk_query_age_ms+elapsed,0)} ms`:'未提供';
    updateControllerVisuals(side,{...c,fresh:controlsFresh,valid_mask:m});
  }
}

function render(s){
  const h=s.head, replay=s.mode==='replay';
  mixDashboard.lastSystem=s.system;mixDashboard.pose(s,deliveryAgeBound);
  $('mode').textContent=replay?'历史数据回放':s.mode==='offline'?'离线界面':'实时观察';
  $('mode').className='pill '+(replay?'replay':s.mode==='live'?'live':'');
  $('connection').textContent='调试服务已连接';
  if ($('ws-bound')) $('ws-bound').textContent = `WS ≤ ${num(deliveryAgeBound, 1)} ms`;
  $('session').textContent=s.session|| (replay?'真实记录回放 · 不代表当前设备状态':'等待真实追踪会话');
  updateVideoStandby();
  poseBadges(poseView());
  $('position').textContent=vec(h.position);
  $('rotation').textContent=vec(h.orientation,2);
  const euler = quatToEulerDeg(h.orientation);
  const eulerStr = euler ? `R ${num(euler[0],1)}°  P ${num(euler[1],1)}°  Y ${num(euler[2],1)}°` : '—';
  if ($('euler-angles')) $('euler-angles').textContent = eulerStr;
  if ($('euler-hud')) $('euler-hud').textContent = euler ? eulerStr : 'R —° · P —° · Y —°';

  $('quality').textContent=`${num(h.quality,0)} / ${h.tracking_state??'—'}`;
  $('age').textContent=replay?'历史记录':num(h.age_ms,1)+' ms';
  $('head-reason').textContent=h.valid?'姿态有效 · 精度未校准':h.recorded_valid?'历史记录有效；不作为实时输入':(h.reasons||[]).join(' · ');
  $('packet-rate').textContent=num(s.packet_hz,1);$('camera-age').textContent=num(h.camera_age_ms,2);
  $('clock-bracket').textContent=num(h.clock_bracket_us,2);
  $('memory').textContent=num(s.system.memory_used_mb,0)+' MB';
  $('memory-label').textContent=s.system.scope==='headset'?'头显内存':'开发主机内存（非头显）';
  const temps=s.system.temperatures||[];
  const skin=temps.filter(x=>/skin/.test(x.name)&&Number.isFinite(x.celsius));
  const meaningful=skin.length?skin:temps.filter(x=>/cpu|gpu/.test(x.name)&&Number.isFinite(x.celsius));
  $('temperature').textContent=meaningful.length?`${skin.length?'外壳最高':'芯片最高'} · ${num(Math.max(...meaningful.map(x=>x.celsius)),1)} °C`:'温度未读到';
  $('gyro').textContent=vec(h.gyro,2);$('accel').textContent=vec(h.accel,2);
  $('warning').textContent=h.warning_flags==null?'—':'0x'+h.warning_flags.toString(16);
  $('sequence').textContent=h.sequence??'—';$('source-name').textContent=replay?'历史 QVR 数据':s.mode==='offline'?'未接入':'QVR / FastRPC';
  renderControllers(s);
  updateSceneSamples(s);
  if(previewTransport==='jpeg') {
    const pr=s.preview;
    $('preview-source').textContent=pr.source==='recorded-image'?'历史截图 · 非实时':pr.source==='live-kms'?'真实 KMS 扫描画面':'未接入画面源';
    $('preview-metrics').textContent=`${pr.width??'—'} × ${pr.height??'—'} · ${num(pr.actual_fps,1)} fps · JPEG / CPU`;
  }
  $('footer-state').textContent=(s.mode==='live'?'实时观察':'离线验证')+' · '+new Date().toLocaleTimeString();
  if(previousSession!==h.session){history=[];lastSequence=null;previousSession=h.session;}
  if(h.sequence!=null&&h.sequence!==lastSequence){
    const t=performance.now();history.push({t,v:h.gyro});history=history.filter(x=>t-x.t<20000);lastSequence=h.sequence;

  }
}

// One acknowledged state at a time; the server samples latest data at <=60 Hz.
// Elapsed since the preceding ACK bounds delivery age, including server wait.
let stateSocket=null, reconnectTimer=null, retryDelay=500, ackSent=0, nextStateSequence=1;
let pageActive=true;
function closeTelemetry(label, retry=true){
  clearTimeout(reconnectTimer);reconnectTimer=null;
  const ws=stateSocket;stateSocket=null;
  if(ws)ws.close(1000,'viewer paused');
  received=-Infinity;
  for(const t of Object.values(sceneTracks))t.continuous=false;
  if(state?.head)state.head.valid=false;
  $('connection').textContent=label;
  if(retry&&pageActive&&!document.hidden){
    reconnectTimer=setTimeout(connectTelemetry,retryDelay);
    retryDelay=Math.min(5000,retryDelay*2);
  }
}
function connectTelemetry(){
  clearTimeout(reconnectTimer);reconnectTimer=null;
  if(stateSocket||!pageActive||document.hidden)return;
  $('connection').textContent='正在连接调试服务';
  const url=new URL('/ws/state',location.href);url.protocol=location.protocol==='https:'?'wss:':'ws:';
  const ws=new WebSocket(url);stateSocket=ws;ackSent=performance.now();nextStateSequence=1;
  ws.onopen=()=>{
    if(stateSocket!==ws)return;
    ackSent=performance.now();ws.send(JSON.stringify({ack:0}));
  };
  ws.onmessage=event=>{
    if(stateSocket!==ws||document.hidden)return;
    try{
      const message=JSON.parse(event.data);
      if(message.type!=='state'||message.sequence!==nextStateSequence||!message.state?.head)throw Error('Invalid state');
      const now=performance.now();deliveryAgeBound=Math.max(0,now-ackSent-Math.max(0,Math.min(now-ackSent,message.before_sample_ms||0)))+1;state=message.state;received=now;
      // A fresh server sample may arrive late over TCP. Never revive it as live.
      if(state.mode==='live'){
        const h=state.head;
        if(!Number.isFinite(h.age_ms)||h.age_ms+deliveryAgeBound>=100){
          h.valid=false;h.reasons=[...new Set([...(h.reasons||[]),'transport_stale'])];
        }

      }
      state.transport={protocol:'websocket',sequence:message.sequence,delivery_age_bound_ms:deliveryAgeBound};
      render(state);authenticateVideo();retryDelay=500;
      nextStateSequence++;ackSent=performance.now();ws.send(JSON.stringify({ack:message.sequence}));
    }catch(error){closeTelemetry('数据连接中断，正在重连');}
  };
  ws.onclose=()=>{if(stateSocket===ws)closeTelemetry('连接中断，正在重连');};
  ws.onerror=()=>{if(stateSocket===ws)closeTelemetry('连接中断，正在重连');};
}
setInterval(()=>{
  if(stateSocket&&performance.now()-ackSent>(stateSocket.readyState===WebSocket.CONNECTING?5000:2000)){
    closeTelemetry('数据推送超时，正在重连');
  }
},250);
document.addEventListener('visibilitychange',()=>{
  if(document.hidden)closeTelemetry('页面已暂停',false);
  else {retryDelay=500;connectTelemetry();}
});
window.addEventListener('pagehide',()=>{pageActive=false;closeTelemetry('页面已暂停',false);});
window.addEventListener('pageshow',()=>{pageActive=true;connectTelemetry();});

async function preview(){
  if(previewTransport==='jpeg'&&!paused&&!document.hidden){try{
    const r=await api('/frame.jpg'),blob=await r.blob();
    if(previewTransport==='jpeg'&&!paused) {
      const url=URL.createObjectURL(blob),img=$('preview');
      try { await new Promise((resolve,reject)=>{img.onload=resolve;img.onerror=reject;img.src=url;}); }
      catch(error) { URL.revokeObjectURL(url); throw error; }
      if(lastImage)URL.revokeObjectURL(lastImage);lastImage=url;
      if(previewTransport==='jpeg'&&!paused) {
        img.hidden=false;$('preview-empty').hidden=true;
        $('preview-message').textContent=state?.mode==='replay'?'历史截图；独立于姿态回放时间':'并发扫描读取，非原子截图';zoom();
      }
    }
  }catch(e){if(previewTransport==='jpeg'){$('preview').hidden=true;$('preview-empty').hidden=false;$('preview-message').textContent='画面不可用 · '+e.message;}}}
  setTimeout(preview,1000/(state?.preview.profile.fps||5));
}

function setTransportMode(mode) {
  previewTransport = mode === 'jpeg' ? 'jpeg' : 'webrtc';
  $('preview-transport').value = previewTransport;
  paused = true;
  $('pause').hidden = previewTransport !== 'jpeg';
  $('pause').textContent = '开始 JPEG 预览';
  $('jpeg-panel').hidden = previewTransport !== 'jpeg';
  $('video-wrap').hidden = previewTransport !== 'webrtc';
  updateVideoStandby();
  if(previewTransport==='jpeg') {
    videoMessage('mix-video-stop');$('preview-source').textContent='JPEG · 已暂停';
    $('preview-metrics').textContent='JPEG / CPU · 维护回退';$('preview-message').textContent='点击开始 JPEG 预览';
  } else {
    $('preview-source').textContent='WebRTC · 尚未观看';$('preview-metrics').textContent='H.264 / H.265 / WebRTC';
    $('preview-message').textContent='点击开始观看；JPEG 默认关闭';authenticateVideo();
    videoMessage('mix-video-parent-ready');
  }
}
$('preview-transport').onchange = () => setTransportMode($('preview-transport').value);
if ($('standby-switch-jpeg')) {
  $('standby-switch-jpeg').onclick = () => setTransportMode('jpeg');
}
$('pause').onclick=()=>{if(previewTransport!=='jpeg')return;paused=!paused;$('pause').textContent=paused?'开始 JPEG 预览':'暂停 JPEG 预览';$('preview-message').textContent=paused?'预览已暂停 · 最后画面已冻结':'恢复采集';};
$('apply').onclick=async()=>{
  if(previewTransport!=='jpeg')return;
  $('apply').disabled=true;
  try{await api('/api/preview',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({width:+$('width').value,fps:+$('fps').value,quality:+$('quality-select').value})});$('preview-message').textContent='已更新采集设置';}
  catch(e){$('preview-message').textContent='设置未应用 · '+e.message;}finally{$('apply').disabled=false;}
};
function zoom(){const img=$('preview'),z=$('zoom').value;img.style.maxWidth=z==='fit'?'100%':'none';img.style.width=z==='fit'?'100%':img.naturalWidth*Number(z)+'px';img.style.height=z==='fit'?'100%':'auto';}
$('zoom').onchange=zoom;

function view(y,p,d,activeId){
  yaw=y;pitch=p;distance=d;
  for(const id of ['orbit','front','top','side']){
    const btn=$(id); if(btn) btn.classList.toggle('active', id===activeId);
  }
}
$('orbit').onclick=()=>view(.65,.38,1.65,'orbit');
$('front').onclick=()=>view(0,0,1.65,'front');
$('top').onclick=()=>view(0,1.55,1.65,'top');
if($('side')) $('side').onclick=()=>view(Math.PI/2,0,1.65,'side');
$('reset').onclick=()=>{view(.65,.38,1.65,'orbit');clearSceneTrails();fitScene();};
$('trail-clear').onclick=clearSceneTrails;
$('scene-fit').onclick=fitScene;

const canvas=$('scene'),ctx=canvas.getContext('2d');
canvas.onpointerdown=e=>{drag=[e.clientX,e.clientY];canvas.setPointerCapture(e.pointerId);};
canvas.onpointermove=e=>{if(!drag)return;yaw-=(e.clientX-drag[0])*.006;pitch=Math.max(-1.4,Math.min(1.55,pitch+(e.clientY-drag[1])*.006));drag=[e.clientX,e.clientY];};
canvas.onpointerup=()=>drag=null;canvas.onpointercancel=()=>drag=null;
canvas.addEventListener('wheel',e=>{e.preventDefault();distance=Math.max(.35,Math.min(8,distance*Math.exp(e.deltaY*.001)));},{passive:false});
function size(c){const d=devicePixelRatio||1,w=c.clientWidth,h=c.clientHeight;if(c.width!==Math.round(w*d)||c.height!==Math.round(h*d)){c.width=Math.round(w*d);c.height=Math.round(h*d);}const x=c.getContext('2d');x.setTransform(d,0,0,d,0,0);return [w,h,x];}

let lastVisiblePose=null, holdStarted=null;
function poseView(now=performance.now()){
  const head=state?.head, fresh=now-received<250;
  const age=Number.isFinite(head?.age_ms)?head.age_ms+now-received+deliveryAgeBound:Infinity;
  const valid=Boolean(fresh&&finitePose(head)&&head.coordinate_profile==='android-portrait-v1'&&((state.mode==='live'&&head.valid&&age<100)||(state.mode==='replay'&&head.recorded_valid)));
  if(lastVisiblePose&&lastVisiblePose.session!==state?.session){lastVisiblePose=null;holdStarted=null;}
  if(valid&&state.mode==='live'){
    lastVisiblePose={head:{position:[...head.position],orientation:[...head.orientation]},session:state.session,expires:now+100-age};holdStarted=null;
  }else if(lastVisiblePose&&holdStarted===null){holdStarted=now;}
  const held=Boolean(!valid&&state?.mode==='live'&&lastVisiblePose&&now<Math.min(lastVisiblePose.expires+500,holdStarted+500));
  return {valid,fresh,age,held,displayHead:valid?head:held?lastVisiblePose.head:null};
}
function poseBadges(view){
  const historical=state?.mode==='replay';
  const label=view.valid?(historical?'历史有效':'有效'):!view.fresh||view.age>=100&&Number.isFinite(state?.head?.age_ms)?'已过期':'无效 / 未接入';
  $('head-status').textContent=label;$('head-status').className='badge '+(view.valid?'live':'');
  if ($('head-status-mini')) $('head-status-mini').textContent = view.valid ? '有效 <100ms' : (view.held ? '平滑保持中' : '100ms 门限');
  const count=(view.valid?1:0)+(state?.controllers||[]).filter(c=>controllerView(c).valid).length;
  $('pose-state').textContent=(historical?'历史空间':'共同空间')+' · '+(count?count+'/3 有效':'等待姿态');
  $('pose-state').className='pill '+(count?(historical?'replay':'live'):'');
}
let visualPose=null, visualAt=0, visualSession=null;
function smoothPose(target,now){
  if(!target){visualPose=null;visualAt=now;return null;}
  const jump=visualPose&&Math.hypot(...target.position.map((v,i)=>v-visualPose.position[i]))>.5;
  if(!visualPose||visualSession!==state?.session||jump){
    visualPose={position:[...target.position],orientation:[...target.orientation]};visualSession=state?.session;
  }else{
    const alpha=1-Math.exp(-Math.max(0,Math.min(50,now-visualAt))/25);
    visualPose.position=visualPose.position.map((v,i)=>v+alpha*(target.position[i]-v));
    const dot=visualPose.orientation.reduce((v,x,i)=>v+x*target.orientation[i],0),sign=dot<0?-1:1;
    const q=visualPose.orientation.map((v,i)=>v+alpha*(sign*target.orientation[i]-v));
    const length=Math.hypot(...q);visualPose.orientation=q.map(v=>v/length);
  }
  visualAt=now;return visualPose;
}

// Both wire adapters already use the running HMD's raw OpenVR origin, in metres.
// Do not apply the SDK->OpenVR conversion again or add the head translation.
const sceneColors={head:'#7dd3fc',left:'#c4a5ff',right:'#fbbf67'};
const sceneNames={head:'HMD · 头显',left:'L · 左手',right:'R · 右手'};
const sceneTracks=Object.fromEntries(['head','left','right'].map(key=>[key,{points:[],identity:null,token:null,continuous:false,epoch:null}]));
let sceneSession=null,sceneHeadSession=null,sceneCenter=null;
function finitePose(p){
  return Array.isArray(p?.position)&&p.position.length===3&&p.position.every(Number.isFinite)&&
    Array.isArray(p.orientation)&&p.orientation.length===4&&p.orientation.every(Number.isFinite)&&Math.abs(Math.hypot(...p.orientation)-1)<.01;
}
function controllerView(c,now=performance.now()){
  const elapsed=Math.max(0,now-received)+deliveryAgeBound;
  const live=state?.mode==='live'&&Number.isFinite(elapsed)&&elapsed<1500;
  const fresh=live&&c?.fresh&&Number.isFinite(c.age_ms)&&c.age_ms+elapsed<100;
  const statusFresh=live&&c?.status_fresh&&Number.isFinite(c.status_age_ms)&&c.status_age_ms+elapsed<1500;
  const sameSpace=c?.coordinate_profile===1&&c.session===state?.session;
  const valid=Boolean(fresh&&statusFresh&&sameSpace&&c.position_tracking&&c.orientation_valid&&finitePose(c));
  let label='等待姿态';
  if(state?.mode==='offline')label='离线 · 无实时手柄';
  else if(state?.mode==='replay')label='回放未提供手柄';
  else if(!live)label='数据已过期';
  else if(c?.worker_state==='stopped')label='追踪已停止';
  else if(c?.worker_state==='not_started')label='追踪未启动';
  else if(valid)label=c.optical_valid?'光学更新':'预测跟踪';
  else if(statusFresh&&c.ctrl_state===7)label='休眠';
  else if(statusFresh&&c.link_present===false)label='未连接';
  else if(fresh&&c.coordinate_profile!=null&&!sameSpace)label='空间不匹配';
  else if(statusFresh&&c.link_present)label='已连接 · 等待定位';
  else if(c?.age_ms!=null||!statusFresh)label='数据已过期';
  return {valid,fresh,statusFresh,label,age:Number.isFinite(c?.age_ms)?c.age_ms+elapsed:null,
    kind:valid?(c.optical_valid?'optical':'predicted'):'invalid',pose:valid?c:null};
}
function clearSceneTrails(){
  for(const t of Object.values(sceneTracks)){t.points=[];t.token=null;t.continuous=false;}
}
function updateSceneSamples(s){
  const now=performance.now();
  const session=s.mode+':'+s.session;
  if(sceneSession!==session||(s.head.session!=null&&sceneHeadSession!=null&&s.head.session!==sceneHeadSession)){
    clearSceneTrails();sceneCenter=null;sceneHeadSession=null;sceneSession=session;
    for(const t of Object.values(sceneTracks)){t.identity=null;t.epoch=null;}
  }
  if(s.head.session!=null)sceneHeadSession=s.head.session;
  function sample(key,pose,valid,token,identity,epoch,kind){
    const t=sceneTracks[key];
    if(identity!=null&&t.identity!==identity){t.points=[];t.token=null;t.continuous=false;t.identity=identity;}
    if(epoch!=null&&t.epoch!==epoch){t.continuous=false;t.epoch=epoch;}
    if(!valid||!finitePose(pose)){t.continuous=false;return;}
    if(!sceneCenter)sceneCenter=[...pose.position];
    if(token==null||token===t.token)return;
    t.token=token;
    const last=t.points.at(-1);
    if(last&&t.continuous&&now-last.t<32)return; // At most ~30 samples/s, independent of refresh rate.
    const gap=!t.continuous||!last||now-last.t>150||Math.hypot(...pose.position.map((x,i)=>x-last.p[i]))>.5;
    t.points.push({p:[...pose.position],t:now,gap,kind});t.continuous=true;
    if(t.points.length>2000)t.points.shift();
  }
  const hv=poseView(now);
  sample('head',s.head,hv.valid,s.head.sequence??s.head.sample_ns,s.head.session,null,'optical');
  for(const [hand,key] of [[0,'left'],[1,'right']]){
    const c=s.controllers?.find(x=>x.hand===hand),v=controllerView(c,now);
    sample(key,c,v.valid,c?.sequence??c?.sample_ns,c?.packet_session??c?.session,c?.tracking_epoch,v.kind);
  }
}
function fitScene(){
  const poses=[];const head=poseView().displayHead;
  if(finitePose(head))poses.push(head.position);
  for(const c of state?.controllers||[])if(controllerView(c).valid)poses.push(c.position);
  if(!poses.length)return;
  const lo=[0,1,2].map(i=>Math.min(...poses.map(p=>p[i]))),hi=[0,1,2].map(i=>Math.max(...poses.map(p=>p[i])));
  sceneCenter=lo.map((v,i)=>(v+hi[i])/2);
  distance=Math.max(1.4,Math.min(8,Math.hypot(...hi.map((v,i)=>v-lo[i]))*1.5+.7));
}
function renderSceneStatus(view,now){
  const ttl=Number($('trail-duration').value)*1000;
  for(const [key,t] of Object.entries(sceneTracks)){
    t.points=t.points.filter(p=>now-p.t<ttl);
    const c=key==='head'?null:state?.controllers?.find(c=>c.hand===(key==='left'?0:1));
    const v=key==='head'?{valid:view.valid,kind:view.valid?'optical':view.held?'held':'invalid',
      label:view.valid?(state?.mode==='replay'?'历史姿态':'定位有效'):view.held?'失效 · 上次姿态':!view.fresh?'数据已过期':'等待有效姿态',
      pose:view.valid?state.head:null,age:view.age}:controllerView(c,now);
    const el=$('scene-'+key+'-state');el.textContent=v.label;el.dataset.tracking=v.kind;
    $('scene-'+key+'-position').textContent=v.pose?'XYZ '+vec(v.pose.position,2)+' m':'XYZ —';
    const age=Number.isFinite(v.age)?num(v.age,0)+' ms · ':'';
    $('scene-'+key+'-detail').textContent=age+'轨迹 '+t.points.length+' 点';
  }
}

function draw(){
  const [w,h]=size(canvas);ctx.clearRect(0,0,w,h);
  const now=performance.now(),view=poseView(now),{valid,fresh,held}=view,head=smoothPose(view.displayHead,now);
  poseBadges(view);
  renderSceneStatus(view,now);
  const center=sceneCenter||[0,0,0];
  function project(p){
    const x=p[0]-center[0],y=p[1]-center[1],z=p[2]-center[2];
    const u=Math.cos(yaw)*x-Math.sin(yaw)*z,v=Math.sin(yaw)*x+Math.cos(yaw)*z;
    const yy=Math.cos(pitch)*y-Math.sin(pitch)*v,zz=Math.sin(pitch)*y+Math.cos(pitch)*v;
    const depth=distance-zz;if(depth<.08)return null;
    const f=Math.min(w,h)*1.1/depth;
    return [w/2+u*f,h*.52-yy*f,depth];
  }
  function line(a,b,color,width=1){
    a=project(a);b=project(b);if(!a||!b)return;
    ctx.beginPath();ctx.moveTo(a[0],a[1]);ctx.lineTo(b[0],b[1]);ctx.strokeStyle=color;ctx.lineWidth=width;ctx.stroke();
  }
  const placedLabels=[];
  function labelAt(point,text,color){
    if(!point)return;
    ctx.font='600 11px system-ui';
    const width=ctx.measureText(text).width+12;
    const x=Math.max(width/2+6,Math.min(w-width/2-6,point[0]));
    let y=Math.max(65,Math.min(h-42,point[1]));
    for(let i=0;i<6&&placedLabels.some(r=>Math.abs(x-r.x)<(width+r.width)/2+4&&Math.abs(y-r.y)<20);i++)y-=21;
    placedLabels.push({x,y,width});
    ctx.fillStyle='#0a1022dd';ctx.fillRect(x-width/2,y-13,width,19);
    ctx.fillStyle=color;ctx.textAlign='center';ctx.fillText(text,x,y);ctx.textAlign='start';
  }
  const gx=Math.round(center[0]*4)/4,gz=Math.round(center[2]*4)/4,gy=-.25;
  for(let i=-6;i<=6;i++){
    const major = i === 0;
    line([gx+i*.25,gy,gz-1.5],[gx+i*.25,gy,gz+1.5], major ? 'rgba(56, 189, 248, 0.32)' : 'rgba(148, 163, 196, 0.14)', major ? 1.4 : 1);
    line([gx-1.5,gy,gz+i*.25],[gx+1.5,gy,gz+i*.25], major ? 'rgba(56, 189, 248, 0.32)' : 'rgba(148, 163, 196, 0.14)', major ? 1.4 : 1);
  }
  for(let i=0;i<3;i++){
    const end=[0,0,0];end[i]=.4;line([0,0,0],end,colors[i],2);
    const p=project(end);if(p){ctx.fillStyle=colors[i];ctx.font='700 10.5px ui-monospace,monospace';ctx.fillText('XYZ'[i],p[0]+6,p[1]-5);}
  }
  const ttl=Number($('trail-duration').value)*1000;
  for(const [key,t] of Object.entries(sceneTracks)){
    if(!$("trail-"+key).checked)continue;
    for(let i=1;i<t.points.length;i++){
      const a=t.points[i-1],b=t.points[i];if(b.gap)continue;
      ctx.globalAlpha=.12+.6*Math.max(0,1-(now-b.t)/ttl);
      ctx.setLineDash(b.kind==='predicted'?[3,4]:[]);
      line(a.p,b.p,sceneColors[key],2);
    }
  }
  ctx.globalAlpha=1;ctx.setLineDash([]);
  if(valid||held){
    ctx.globalAlpha=held ? .4 : 1;
    const q=head.orientation,p=head.position;
    // Ground drop shadow line to aid 3D height perception
    line([p[0], gy, p[2]], p, 'rgba(167, 139, 250, 0.45)', 1.2);
    const shadow = project([p[0], gy, p[2]]);
    if (shadow) {
      ctx.beginPath(); ctx.arc(shadow[0], shadow[1], 4.5, 0, Math.PI * 2);
      ctx.fillStyle = 'rgba(52, 211, 153, 0.55)'; ctx.fill();
      ctx.beginPath(); ctx.arc(shadow[0], shadow[1], 9, 0, Math.PI * 2);
      ctx.strokeStyle = 'rgba(56, 189, 248, 0.35)'; ctx.lineWidth = 1; ctx.stroke();
    }
    function transform(v){const [x,y,z,w]=q,tx=2*(y*v[2]-z*v[1]),ty=2*(z*v[0]-x*v[2]),tz=2*(x*v[1]-y*v[0]);return [p[0]+v[0]+w*tx+y*tz-z*ty,p[1]+v[1]+w*ty+z*tx-x*tz,p[2]+v[2]+w*tz+x*ty-y*tx];}
    // View frustum rays
    const lensCenter = transform([0,0,-.07]);
    for(const ray of [[-.16,.10,-.42],[.16,.10,-.42],[.16,-.10,-.42],[-.16,-.10,-.42]]){
      line(lensCenter, transform(ray), 'rgba(56, 189, 248, 0.28)', 1);
    }
    const points=[[-.12,-.055,-.07],[.12,-.055,-.07],[.12,.055,-.07],[-.12,.055,-.07],[-.12,-.055,.055],[.12,-.055,.055],[.12,.055,.055],[-.12,.055,.055]].map(transform);
    const faces=[[0,1,2,3],[4,7,6,5],[0,4,5,1],[3,2,6,7],[0,3,7,4],[1,5,6,2]];
    faces.map((f,i)=>({f:f.map(j=>project(points[j])),i})).filter(o=>o.f.every(Boolean)).sort((a,b)=>b.f.reduce((n,p)=>n+p[2],0)-a.f.reduce((n,p)=>n+p[2],0)).forEach(({f,i})=>{
      ctx.beginPath();f.forEach((p,j)=>j?ctx.lineTo(p[0],p[1]):ctx.moveTo(p[0],p[1]));ctx.closePath();
      ctx.fillStyle=['rgba(45,112,138,0.85)','rgba(30,64,98,0.85)','rgba(24,52,82,0.85)','rgba(56,136,162,0.88)','rgba(38,84,118,0.85)','rgba(32,74,108,0.85)'][i];ctx.fill();
      ctx.strokeStyle='#7dd3fc';ctx.lineWidth=1.4;ctx.stroke();
    });
    line(lensCenter,transform([0,0,-.30]),'#34d399',2.4);
    const label=project(transform([0,.12,0]));
    labelAt(label,state.mode==='replay'?'HMD · 历史姿态':held?'HMD · 上次姿态':'HMD · 实时姿态',sceneColors.head);
    ctx.globalAlpha=1;
  }else if(!(state?.controllers||[]).some(c=>controllerView(c,now).valid)){
    ctx.fillStyle='#8b97b1';ctx.font='600 12.5px system-ui';ctx.textAlign='center';
    ctx.fillText('等待有效姿态样本',w/2,h/2-8);
    ctx.font='10.5px system-ui';ctx.fillStyle='#6c7891';
    ctx.fillText('头显与双手柄在同一追踪空间显示',w/2,h/2+14);
    ctx.textAlign='start';
    if(!fresh&&received){
      $('age').textContent='—';

    }
  }
  function drawControllers(){
    const hands=(state?.controllers||[]).map(c=>({c,v:controllerView(c,now)})).filter(x=>x.v.valid);
    hands.sort((a,b)=>(project(b.c.position)?.[2]||0)-(project(a.c.position)?.[2]||0));
    for(const {c,v} of hands){
      const key=c.hand===0?'left':'right',color=sceneColors[key],p=c.position,q=c.orientation;
      function transform(a){
        const [x,y,z,w]=q,tx=2*(y*a[2]-z*a[1]),ty=2*(z*a[0]-x*a[2]),tz=2*(x*a[1]-y*a[0]);
        return [p[0]+a[0]+w*tx+y*tz-z*ty,p[1]+a[1]+w*ty+z*tx-x*tz,p[2]+a[2]+w*tz+x*ty-y*tx];
      }
      const body=[[-.024,-.09,.018],[.024,-.09,.018],[.029,.015,-.015],[-.029,.015,-.015],[-.024,-.09,.052],[.024,-.09,.052],[.029,.015,.025],[-.029,.015,.025]].map(transform);
      const screen=body.slice(0,4).map(project);
      if(screen.every(Boolean)){
        ctx.beginPath();screen.forEach((p,i)=>i?ctx.lineTo(p[0],p[1]):ctx.moveTo(p[0],p[1]));ctx.closePath();
        ctx.fillStyle=color+'38';ctx.fill();
      }
      ctx.setLineDash(v.kind==='predicted'?[4,3]:[]);
      for(const [a,b] of [[0,1],[1,2],[2,3],[3,0],[4,5],[5,6],[6,7],[7,4],[0,4],[1,5],[2,6],[3,7]])line(body[a],body[b],color,1.6);
      const ring=Array.from({length:25},(_,i)=>transform([.06*Math.cos(i*Math.PI/12),.034,.06*Math.sin(i*Math.PI/12)-.025]));
      for(let i=1;i<ring.length;i++)line(ring[i-1],ring[i],color,2);
      // Native local -Z indicator only: not a calibrated grip/aim ray.
      line(transform([0,0,0]),transform([0,0,-.22]),color,1.4);
      line(transform([0,0,-.22]),transform([-.012,0,-.19]),color,1.4);
      line(transform([0,0,-.22]),transform([.012,0,-.19]),color,1.4);
      ctx.setLineDash([2,5]);ctx.globalAlpha=.35;line([p[0],gy,p[2]],p,color);ctx.globalAlpha=1;ctx.setLineDash([]);
      const label=project([p[0],p[1]+.14,p[2]]);
      labelAt(label,sceneNames[key]+' · '+v.label,color);
    }
  }
  drawControllers();
  if(state)renderControllers(state);
  drawChart();requestAnimationFrame(draw);
}
function drawChart(){mixDashboard.draw();}

connectTelemetry();preview();draw();
