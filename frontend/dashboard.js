'use strict';
// All histories are bounded observations. Missing or stale samples stay gaps.
window.mixDashboard = (() => {
  const $ = id => document.getElementById(id), finite = Number.isFinite;
  const palette = ['#a78bfa','#34d399','#38bdf8','#fbbf24','#fb7185'];
  const scaled = (v,n) => finite(v) ? v/n : null;
  const show = (v,d=1) => finite(v) ? v.toFixed(d) : '—';
  let poses=[], videos=[], events=[], previousVideo=null, previousHead=null, sourceSession=null;
  let lastPose=0, lastDraw=0, frozenAt=null, span=60000, lastVideoAt=null, lastHeadAt=null, headDeadline=null;
  let sharedHoverRatio = null;

  const specs = [
    ['native-frame-rates','pose',['nativeScan','nativeFrames','appFrames'],['显示扫描','SteamVR 原生','应用提交'],'fps'],
    ['compositor-timing','pose',['compGpu','compCpu','presentCall','waitPresent'],['合成 GPU','合成 CPU','Present 调用','等待 Present'],'ms'],
    ['gpu-load-history','pose',['gpuLoad','compositorLoad','captureLoad','cpuLoad','cpuPressure'],['GPU 总占时','SteamVR','采集','CPU','CPU 等待压力'],'%'],
    ['vpu-clocks','pose',['vpu0','vpu1'],['MVS0','MVS1 /2'],'MHz'],
    ['vpu-pressure','video',['vpu50','vpu95'],['编码 P50 / 帧预算','编码 P95 / 帧预算'],'%'],
    ['encoder-queue','video',['queued','pending','encoded'],['排队缓冲','待匹配输出','编码 fps'],'帧 / fps'],
    ['chart','pose',['gx','gy','gz'],['X','Y','Z'],'rad/s',true],
    ['frame-rates','video',['attempts','distinct','encoded','decoded','presented'],['采集尝试','内容变化','编码','解码','呈现'],'fps'],
    ['pipeline-latency','video',['capture','pipe','encode','total'],['采集 P50','进程传输 P50','编码 P50','采集至编码 P50'],'ms'],
    ['network-latency','video',['rtt','jitter','decode'],['ICE 往返','抖动缓冲','解码'],'ms'],
    ['frame-intervals','video',['interval50','interval95','intervalMax'],['P50','P95','最大'],'ms'],
    ['pose-age','pose',['age','camera','delivery'],['样本年龄','相机间隔','WS 传输年龄上界'],'ms'],
    ['clock-timing','pose',['bracket','limit'],['IPC 时钟夹区间','有效门限'],'µs'],
    ['position-history','pose',['px','py','pz'],['X','Y','Z'],'m',true],
    ['accel-history','pose',['ax','ay','az'],['X','Y','Z'],'m/s²',true],
    ['temperature-history','pose',['cpuTemp','gpuTemp','skinTemp'],['CPU 最高','GPU 最高','外壳最高'],'°C'],
    ['frequency-history','pose',['cpu0','cpu4','cpu7','gpu'],['小核','大核','超大核','GPU'],'MHz']
  ].map(([id,source,keys,labels,unit,signed]) => ({id,source,keys,labels,unit,signed,hover:null,valueNodes:[],headValueNode:null}));

  function event(text) {
    events.push({at:new Date().toISOString(),text});if(events.length>60)events.shift();
    const list=$('event-list');if(!list)return;list.replaceChildren();
    for(const item of events.slice(-14).reverse()) {
      const li=document.createElement('li'),t=document.createElement('time'),s=document.createElement('span');
      t.textContent=new Date(item.at).toLocaleTimeString();s.textContent=item.text;li.append(t,s);list.append(li);
    }
  }

  function trim(rows,now){while(rows.length && (now-rows[0].t>120000 || rows.length>650))rows.shift();}
  function maxTemp(rows,regex){const a=rows.filter(x=>regex.test(x.name)&&finite(x.celsius)).map(x=>x.celsius);return a.length?Math.max(...a):null;}

  function renderSocBars(row, sys) {
    const container = $('soc-live-bars');
    if (!container) return;
    const cooling = (sys?.cooling || []).filter(x => x.state > 0 && /cpu|gpu/i.test(x.name));
    const badge = $('soc-cooling-badge');
    if (badge) {
      badge.textContent = cooling.length ? `温控激活 (${cooling.length})` : '温控正常';
      badge.className = 'pill subtle ' + (cooling.length ? 'replay' : 'live');
    }
    const items = [
      ['CPU 小核 / 大核', row.cpu0, 1805, `${show(row.cpu0,0)} / ${show(row.cpu4,0)} MHz`, palette[2]],
      ['CPU 超大核', row.cpu7, 2842, `${show(row.cpu7,0)} MHz`, palette[0]],
      ['Adreno GPU 频率', row.gpu, 670, `${show(row.gpu,0)} MHz`, palette[1]],
      ['芯片最高温度', Math.max(row.cpuTemp ?? -Infinity, row.gpuTemp ?? -Infinity), 95,
        `CPU ${show(row.cpuTemp,1)}° · 外壳 ${show(row.skinTemp,1)}°C`,
        (row.cpuTemp > 78 ? palette[4] : palette[3])]
    ];
    container.replaceChildren();
    for (const [name, val, maxVal, text, color] of items) {
      const div = document.createElement('div'); div.className = 'stage-row';
      const label = document.createElement('span'), track = document.createElement('div'),
            fill = document.createElement('div'), value = document.createElement('strong');
      label.textContent = name; track.className = 'stage-track'; fill.className = 'stage-fill';
      fill.style.width = (finite(val) && val > 0 ? Math.min(100, val / maxVal * 100) : 0) + '%';
      fill.style.background = color;
      value.textContent = text;
      track.append(fill); div.append(label, track, value); container.append(div);
    }
  }

  function pose(s,delivery){
    const now=performance.now(),h=s.head;lastHeadAt=now;
    if(sourceSession!==s.session){sourceSession=s.session;poses=[];previousHead=null;event(s.session?'追踪会话已连接':'等待追踪会话');}
    const fresh=finite(h.age_ms)&&h.age_ms>=0&&finite(delivery)&&h.age_ms+delivery<100;
    const valid=Boolean(h.valid)&&fresh;
    headDeadline=s.mode==='live'&&fresh?now+100-h.age_ms-delivery:null;
    const label=s.mode==='replay'?'历史回放':valid?'有效':!finite(h.age_ms)?'未接入':fresh?'无效':'过期';
    if(previousHead!==label){event('头显姿态 · '+label);previousHead=label;}
    $('overview-pose').textContent=label;
    $('overview-quality').textContent='质量 '+show(h.quality,0)+' · '+show(h.age_ms)+' ms';
    $('overview-status').textContent=s.mode==='live'?(fresh?'QVR / IMU · 已接收':finite(h.age_ms)?'追踪数据已过期':'等待追踪数据'):s.mode==='replay'?'历史回放 · 非实机验收':'离线 · 无实时数据';

    const ageBar = $('pose-age-bar');
    if (ageBar) {
      const totalAge = finite(h.age_ms) ? h.age_ms + (finite(delivery) ? delivery : 0) : null;
      const pct = totalAge !== null ? Math.max(4, Math.min(100, (100 - Math.min(100, totalAge)))) : 0;
      ageBar.style.width = (s.mode === 'replay' && h.recorded_valid ? 100 : pct) + '%';
      ageBar.style.background = valid ? 'var(--green)' : (totalAge !== null && totalAge < 150 ? 'var(--amber)' : 'var(--rose)');
    }

    const temp=s.system?.temperatures||[],freq=s.system?.frequencies||[],load=s.system?.device_load||{},native=s.native_frames||{};
    $('native-scan').textContent=show(load.display?.scan_hz,1)+' Hz';
    $('native-mode').textContent='当前模式 '+show(load.display?.mode_hz,0)+' Hz';
    $('native-fps').textContent=show(native.available?native.frame_index_fps:null,1)+' fps';
    $('gpu-load').textContent=show(load.gpu?.total_percent,1)+'%';
    if (s.system?.kernel && $('kernel-version')) {
      $('kernel-version').textContent = `${s.system.kernel} · 只读观察`;
    }
    const clock=n=>{const c=load.vpu?.clocks?.find(x=>x.name===n);return c?(c.enabled?c.rate_mhz:0):null;};
    const getFreq=n=>freq.find(x=>x.name===n)?.current_mhz??null;
    const row = {t:now,nativeScan:load.display?.scan_hz,nativeFrames:native.available?native.frame_index_fps:null,appFrames:native.available?native.application_fps:null,
      compGpu:native.available?native.compositor_gpu_ms:null,compCpu:native.available?native.compositor_cpu_ms:null,presentCall:native.available?native.present_call_ms:null,waitPresent:native.available?native.wait_present_ms:null,
      gpuLoad:load.gpu?.total_percent,compositorLoad:load.gpu?.compositor_percent,captureLoad:load.gpu?.capture_percent,cpuLoad:load.cpu_percent,cpuPressure:load.pressure?.cpu_some_avg10,vpu0:clock('video_cc_mvs0_clk'),vpu1:clock('video_cc_mvs1_div2_clk'),valid:valid||Boolean(h.recorded_valid),gx:valid?h.gyro?.[0]:null,gy:valid?h.gyro?.[1]:null,gz:valid?h.gyro?.[2]:null,
      ax:valid?h.accel?.[0]:null,ay:valid?h.accel?.[1]:null,az:valid?h.accel?.[2]:null,
      px:valid?h.position?.[0]:null,py:valid?h.position?.[1]:null,pz:valid?h.position?.[2]:null,
      age:h.age_ms,camera:h.camera_age_ms,delivery,bracket:h.clock_bracket_us,limit:50,
      cpuTemp:maxTemp(temp,/cpu|cluster/),gpuTemp:maxTemp(temp,/gpu/),skinTemp:maxTemp(temp,/skin/),
      cpu0:getFreq('policy0'),cpu4:getFreq('policy4'),cpu7:getFreq('policy7'),gpu:getFreq('GPU')};
    renderSocBars(row, s.system);
    if(frozenAt!==null||now-lastPose<200)return;lastPose=now;
    poses.push(row);trim(poses,now);
  }

  function counterRate(a,b){if(!a||!b||a.frames<b.frames||a.last_monotonic_ns<=b.last_monotonic_ns)return null;return (a.frames-b.frames)*1e9/(a.last_monotonic_ns-b.last_monotonic_ns);}

  function video(o){
    const now=performance.now(),s=o.server,r=o.inbound,p=o.presentation;
    if(!s||!r)return;
    const prev=previousVideo?.server?.id===s.id?previousVideo:null;
    if(!prev)event('视频会话 · '+s.requested.width+' × '+s.requested.height+' / '+s.requested.bitrate/1e6+' Mbps 目标');
    const elapsed=prev?(s.capture?.captured_ns-prev.server.capture?.captured_ns)/1e9:0;
    const diff=k=>elapsed>0&&s.capture?.[k]>=prev.server.capture?.[k]?(s.capture[k]-prev.server.capture[k])/elapsed:null;
    const phase=k=>scaled(s.phase_latency?.stages?.[k]?.p50,1000);
    const row={t:now,vpu50:scaled(s.phase_latency?.stages?.encoder_input_to_output_us?.p50,1e6)*s.requested.fps*100,vpu95:scaled(s.phase_latency?.stages?.encoder_input_to_output_us?.p95,1e6)*s.requested.fps*100,queued:s.appsrc_queued_buffers,pending:s.phase_latency?.pending_pts,attempts:diff('attempts'),distinct:diff('sequence'),encoded:counterRate(s.counts?.encoded_vcl_au,prev?.server.counts?.encoded_vcl_au),decoded:r.decoded_fps,presented:p?.presented_fps,
      capture:scaled(s.timing?.capture_us?.p50,1000),pipe:phase(s.phase_latency?.native_encoder ? 'encoded_output_to_reader_us' : 'capture_complete_to_reader_us'),encode:phase('encoder_input_to_output_us'),total:phase('capture_complete_to_encoder_output_us'),
      queue:phase('reader_to_encoder_input_us'),rtt:o.browser_latency?.ice_rtt_ms,jitter:o.browser_latency?.jitter_buffer_mean_ms,decode:o.browser_latency?.decode_mean_ms,
      interval50:p?.callback_interval_p50_ms,interval95:p?.callback_interval_p95_ms,intervalMax:p?.callback_interval_max_ms};
    for(const [key,q] of [['vpu50','p50'],['vpu95','p95']])if(!finite(s.phase_latency?.stages?.encoder_input_to_output_us?.[q]))row[key]=null;
    $('vpu-budget').textContent=show(row.vpu95,1)+'%';
    previousVideo=o;lastVideoAt=now;
    if(frozenAt===null){videos.push(row);trim(videos,now);}
    $('overview-fps').textContent=show(row.decoded);$('overview-latency').textContent=show(row.total);
    const duplicate=finite(row.attempts)&&row.attempts>0&&finite(row.distinct)?Math.max(0,1-row.distinct/row.attempts):null;
    $('duplicate-ratio').textContent=finite(duplicate)?show(duplicate*100)+'%':'—';
    $('queue-drops').textContent=s.appsrc_dropped_confirmed??'—';
    const mbps = show(scaled(r.bitrate_bps,1e6),2)+' Mbps';
    $('actual-bitrate').textContent=mbps;
    if ($('hud-bitrate-tag')) $('hud-bitrate-tag').textContent = mbps;
    $('analysis-state').textContent='实际观测';$('analysis-state').className='pill live';
    const cooling=(window.mixDashboard.lastSystem?.cooling||[]).filter(x=>x.state>0&&/cpu|gpu/.test(x.name));
    let summary='采集 '+show(row.attempts)+' / 内容变化 '+show(row.distinct)+' / 编码 '+show(row.encoded)+' fps。';
    if(finite(duplicate)&&duplicate>.15)summary+='相同内容被跳过；不将重复读取当作新画面。';
    if(finite(row.encoded)&&finite(row.decoded)&&row.encoded-row.decoded>5)summary+='浏览器解码落后于编码，需同时检查网络和解码端。';
    if(cooling.length)summary+=' 当前存在温控降频请求；此快照不能单独证明帧率因果。';
    $('bottleneck-summary').textContent=summary;
    const bars=$('stage-bars');bars.replaceChildren();
    for(const [name,v,color] of [['GPU / CPU 采集',row.capture,palette[0]],['帧管道传输',row.pipe,palette[1]],['排队 / 入编码器',row.queue,palette[2]],['硬件编码与解析',row.encode,palette[3]]]){
      const div=document.createElement('div');div.className='stage-row';const label=document.createElement('span'),track=document.createElement('div'),fill=document.createElement('div'),value=document.createElement('strong');label.textContent=name;track.className='stage-track';fill.className='stage-fill';fill.style.width=(finite(v)?Math.min(100,v/40*100):0)+'%';fill.style.background=color;value.textContent=show(v,2)+' ms';track.append(fill);div.append(label,track,value);bars.append(div);
    }
  }

  for(const spec of specs){
    const c=$(spec.id);
    if (!c) continue;
    c.addEventListener('pointermove',e=>{
      spec.hover=e.offsetX;
      const w=c.clientWidth, left=46, right=w-14;
      sharedHoverRatio = right > left ? Math.max(0, Math.min(1, (e.offsetX - left) / (right - left))) : null;
      lastDraw=0;
    });
    c.addEventListener('pointerleave',()=>{
      spec.hover=null;
      sharedHoverRatio=null;
      lastDraw=0;
    });
    const unitEl = c.parentElement?.querySelector('.panel-head .unit');
    if (unitEl) {
      const headVal = document.createElement('strong');
      headVal.className = 'mono-readout';
      headVal.textContent = '— ' + spec.unit;
      unitEl.replaceWith(headVal);
      spec.headValueNode = headVal;
    }
    const legend=$(spec.id+'-legend');
    if(legend){
      legend.replaceChildren();
      for(let i=0;i<spec.keys.length;i++){
        const s=document.createElement('span'),dot=document.createElement('i'),val=document.createElement('b');
        dot.style.background=palette[i];
        val.textContent='';
        s.append(dot,document.createTextNode(spec.labels[i]),val);
        legend.append(s);
        spec.valueNodes.push(val);
      }
    }
  }

  function chart(spec,now,style){
    const canvas=$(spec.id); if(!canvas)return;
    const rect=canvas.getBoundingClientRect();if(rect.bottom<0||rect.top>innerHeight||rect.width<1)return;
    const w=canvas.clientWidth,h=canvas.clientHeight,dpr=Math.min(devicePixelRatio||1,2);
    if(canvas.width!==Math.round(w*dpr)||canvas.height!==Math.round(h*dpr)){canvas.width=Math.round(w*dpr);canvas.height=Math.round(h*dpr);}
    const c=canvas.getContext('2d');c.setTransform(dpr,0,0,dpr,0,0);c.clearRect(0,0,w,h);
    const from=now-span,rows=(spec.source==='pose'?poses:videos).filter(x=>x.t>=from&&x.t<=now);
    const left=46,right=w-14,top=20,bottom=h-24;let min=0,max=1,any=false;
    for(const row of rows)for(const key of spec.keys)if(finite(row[key])){any=true;min=Math.min(min,row[key]);max=Math.max(max,row[key]);}
    if(spec.id==='position-history'&&any){const values=rows.flatMap(r=>spec.keys.map(k=>r[k])).filter(finite);min=Math.min(...values);max=Math.max(...values);const pad=Math.max(.002,(max-min)*.15);min-=pad;max+=pad;}else{max*=1.12;if(min<0)min*=1.12;}
    const x=t=>left+(t-from)/span*(right-left),y=v=>bottom-(v-min)/(max-min)*(bottom-top);
    c.font='9.5px ui-monospace,monospace';c.lineWidth=1;c.strokeStyle=style.grid;c.fillStyle=style.muted;
    for(let i=0;i<4;i++){const v=min+(max-min)*i/3,yy=y(v);c.beginPath();c.moveTo(left,yy);c.lineTo(right,yy);c.stroke();c.fillText(spec.unit==='m'?v.toFixed(3):Math.abs(v)<10?v.toFixed(1):v.toFixed(0),4,yy+3);}
    for(let i=0;i<5;i++){const xx=left+(right-left)*i/4;c.fillText(i===4?'现在':'−'+Math.round(span/1000*(1-i/4))+'s',Math.min(xx,right-22),h-7);}
    if(spec.source==='pose')for(let i=0;i<rows.length;i++)if(!rows[i].valid){c.fillStyle='#fb71851a';c.fillRect(x(rows[i].t),top,Math.max(2,x(Math.min(now,rows[i].t+200))-x(rows[i].t)),bottom-top);}
    for(let i=0;i<spec.keys.length;i++){
      const key=spec.keys[i], col=palette[i];
      // Soft gradient area under primary series
      if(i===0 && rows.length>1){
        const grad=c.createLinearGradient(0,top,0,bottom);
        grad.addColorStop(0,col+'36');
        grad.addColorStop(1,col+'02');
        c.fillStyle=grad;
        c.beginPath();
        let started=false, firstX=left, lastX=left, prevT=null;
        for(const row of rows){
          if(!finite(row[key]))continue;
          const xx=x(row.t),yy=y(row[key]);
          if(!started||row.t-prevT>(spec.source==='pose'?450:2200)){
            if(started){c.lineTo(lastX,bottom);c.lineTo(firstX,bottom);}
            c.moveTo(xx,yy);firstX=xx;started=true;
          }else{
            c.lineTo(xx,yy);
          }
          lastX=xx;prevT=row.t;
        }
        if(started){c.lineTo(lastX,bottom);c.lineTo(firstX,bottom);c.closePath();c.fill();}
      }
      c.strokeStyle=col;c.lineWidth=i===0?1.9:1.5;c.lineJoin='round';c.lineCap='round';c.beginPath();let prev=null,lastPt=null;
      for(const row of rows){
        if(!finite(row[key])){prev=null;continue;}
        const xx=x(row.t),yy=y(row[key]);
        if(prev===null||row.t-prev>(spec.source==='pose'?450:2200))c.moveTo(xx,yy);
        else c.lineTo(xx,yy);
        prev=row.t;lastPt=[xx,yy];
      }
      c.stroke();
      if(lastPt&&i===0){
        c.beginPath();c.arc(lastPt[0],lastPt[1],2.8,0,Math.PI*2);c.fillStyle=col;c.fill();
      }
    }
    if(!any){c.fillStyle=style.muted;c.textAlign='center';c.fillText('等待真实样本',w/2,h/2);c.textAlign='start';}

    // Update legend live values or synchronized hover values
    const activeRatio = spec.hover !== null && right > left
      ? Math.max(0, Math.min(1, (spec.hover - left) / (right - left)))
      : (spec.id !== 'chart' ? sharedHoverRatio : null);
    let targetRow = rows.at(-1) || null;
    if (activeRatio !== null && rows.length) {
      const t = from + activeRatio * span;
      targetRow = rows.reduce((a,b)=>Math.abs(b.t-t)<Math.abs(a.t-t)?b:a);
      c.strokeStyle=style.muted;c.setLineDash([3,3]);c.beginPath();c.moveTo(x(targetRow.t),top);c.lineTo(x(targetRow.t),bottom);c.stroke();c.setLineDash([]);
      const text=spec.keys.map((k,i)=>spec.labels[i]+' '+show(targetRow[k],spec.unit==='m'?4:1)).join('   ');
      c.fillStyle=style.text;c.fillText(text,left,12,Math.max(0,right-left));
    }
    if (spec.headValueNode) {
      const primary = targetRow ? spec.keys.map(k => targetRow[k]).find(finite) : null;
      spec.headValueNode.textContent = (finite(primary) ? show(primary, spec.unit === 'm' ? 3 : 1) : '—') + ' ' + spec.unit;
    }
    if (spec.valueNodes.length) {
      const decimals = spec.unit === 'm' ? 3 : 1;
      for (let i = 0; i < spec.keys.length; i++) {
        const v = targetRow ? targetRow[spec.keys[i]] : null;
        spec.valueNodes[i].textContent = finite(v) ? show(v, decimals) : '';
      }
    }
  }

  function draw(){
    const live=performance.now();if(live-lastDraw<100)return;lastDraw=live;const now=frozenAt??live;
    const s=getComputedStyle(document.documentElement);
    const style={grid:s.getPropertyValue('--grid'),muted:s.getPropertyValue('--muted'),text:s.getPropertyValue('--text')};
    for(const spec of specs)chart(spec,now,style);
    if(lastHeadAt!==null&&(live-lastHeadAt>250||(headDeadline!==null&&live>headDeadline))){
      $('overview-pose').textContent='过期';$('overview-status').textContent='追踪数据已过期';$('overview-quality').textContent='等待新的有效样本';
      if ($('pose-age-bar')) { $('pose-age-bar').style.width = '100%'; $('pose-age-bar').style.background = 'var(--rose)'; }
      if(live-lastHeadAt>250){$('native-scan').textContent='—';$('native-fps').textContent='—';$('gpu-load').textContent='—';}
    }
    if(lastVideoAt!==null&&live-lastVideoAt>3000){
      $('overview-fps').textContent='—';$('overview-latency').textContent='—';
      $('analysis-state').textContent='视频已停止 / 过期';$('analysis-state').className='pill';$('vpu-budget').textContent='—';
    }
  }

  // 图表控制与分类过滤
  $('history-window').onchange=()=>{span=Number($('history-window').value)*1000;lastDraw=0;};
  $('charts-pause').onclick=()=>{
    frozenAt=frozenAt===null?performance.now():null;
    $('charts-pause').textContent=frozenAt===null?'暂停图表':'继续图表';
    $('charts-pause').setAttribute('aria-pressed',String(frozenAt!==null));
    $('chart-status').textContent=frozenAt===null?'实时记录 · 悬停任一图表跨图同步游标 · 缺失与过期数据留空':'图表已冻结 · 定位和视频继续运行；继续后保留停顿空隙';
    lastDraw=0;
  };
  $('charts-clear').onclick=()=>{poses=[];videos=[];previousVideo=null;events=[];event('已清空本页历史');lastDraw=0;};
  $('charts-export').onclick=()=>{
    const data={exported_at:new Date().toISOString(),scope:'browser observations; not physical display or end-to-end latency',time_unit:'browser performance.now milliseconds',pose_samples:poses,video_samples:videos,events};
    const url=URL.createObjectURL(new Blob([JSON.stringify(data,null,2)],{type:'application/json'}));
    const a=document.createElement('a');a.href=url;a.download='mix-timing-'+new Date().toISOString().replace(/[:.]/g,'-')+'.json';a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);
  };

  // 图表分类与列宽切换
  const chartGrid = $('chart-grid');
  document.querySelectorAll('[data-chart-cat]').forEach(btn => {
    btn.addEventListener('click', () => {
      document.querySelectorAll('[data-chart-cat]').forEach(b => b.classList.toggle('active', b === btn));
      if (chartGrid) chartGrid.dataset.filter = btn.dataset.chartCat;
      lastDraw = 0;
    });
  });
  document.querySelectorAll('[data-chart-cols]').forEach(btn => {
    btn.addEventListener('click', () => {
      document.querySelectorAll('[data-chart-cols]').forEach(b => b.classList.toggle('active', b === btn));
      if (chartGrid) chartGrid.dataset.cols = btn.dataset.chartCols;
      lastDraw = 0;
    });
  });

  // 工作台视图模式切换 (支持快捷键 1-5)
  function switchView(view) {
    document.body.dataset.view = view;
    document.querySelectorAll('[data-view-target]').forEach(btn => {
      const active = btn.dataset.viewTarget === view;
      btn.classList.toggle('active', active);
      if (btn.hasAttribute('aria-selected')) btn.setAttribute('aria-selected', String(active));
    });
    lastDraw = 0;
  }
  document.querySelectorAll('[data-view-target]').forEach(btn => {
    btn.addEventListener('click', e => {
      e.preventDefault();
      switchView(btn.dataset.viewTarget);
      window.scrollTo({top: 0, behavior: 'smooth'});
    });
  });
  window.addEventListener('keydown', e => {
    if (e.target && ['INPUT','SELECT','TEXTAREA'].includes(e.target.tagName)) return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    const map = {'1':'all','2':'visual','3':'spatial','4':'timing','5':'hardware'};
    if (map[e.key]) switchView(map[e.key]);
  });

  let theme='dark';try{theme=localStorage.getItem('mix-ui-theme')==='light'?'light':'dark';}catch{}
  function setTheme(){
    document.documentElement.dataset.theme=theme;
    const iframe=$('video-frame');
    iframe?.contentWindow?.postMessage({type:'mix-video-theme',theme},location.origin);
    lastDraw=0;
  }
  $('theme').onclick=()=>{theme=theme==='dark'?'light':'dark';try{localStorage.setItem('mix-ui-theme',theme);}catch{}setTheme();};
  setTheme();
  $('video-frame').addEventListener('load',setTheme);

  return {pose,video,draw,lastSystem:null,get observation(){return {pose_samples:poses.length,video_samples:videos.length,events:events.length,frozen:frozenAt!==null,window_seconds:span/1000};}};
})();
