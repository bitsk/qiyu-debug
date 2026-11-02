# MIX H.264 / H.265 WebRTC 头显观看

当前 GPU 入口使用原生 C / GStreamer：KMS DMA-BUF → Vulkan 旋转/按眼采样及 NV12 转换 → 导出 DMA-BUF → Venus `dmabuf-import` → 已压缩 access unit 管道 → Rust HTTP / 信令 / WebRTC。原始 NV12 不经过信令后端，也不以整帧管道传输；仍映射 NV12 做完整内容去重，因此不称为全链路无 CPU 访问。输出 pitch 按 128 字节、luma scanlines 按 32 对齐，通过 GstVideoMeta 传递；GPU fence 完成后提交，四个有独立所有权的输出槽只在下游释放 GstBuffer 后重用，前一内容槽留到比较结束。采集仍是并发扫描读取，`atomic_snapshot=false`。

页面支持自动 / H.264 / H.265，以及双眼 / 左眼 / 右眼。默认选择 H.264 硬件编码；手动选择自动时优先 HEVC；浏览器未宣告 H.265、设备 HEVC 启动失败，或协商/初次解码失败时回退 H.264，并显示原因。原生双眼 3664×1920；原生单眼 1832×1920，只转换和编码选中眼。两种编码保留 1–100 Mbps 目标码率；双眼宽度超过 2560 时仍限制请求 30fps，单眼最高请求 60fps。请求参数不表示性能验收。

原生编码计时在 C 进程采样：编码器 sink pad → 编码解析后的 appsink；编码完成 → Rust 读完码流另列。`capture_complete_to_reader_us` 在原生通路包含编码，不能继续称为原始 NV12 管道耗时。浏览器指标仍独立显示解码、抖动缓冲和 ICE RTT；没有时钟同步的端到端测量。未使用 GPU 入口时，CPU helper 仍保留原始 NV12 管道作为兼容实现。

r1 原生通路实测：3664×1920/30fps/40Mbps 同条件浏览器 H.264 解码从20.22提升到29.50fps，采集完成→编码输出P50从57.57降到13.53ms；编码后码流传输P50 0.95ms。原生左/右眼浏览器32.33/31.96fps，丢帧增量0；本地验证浏览器不宣告HEVC，正常能力回退和测试注入的协商拒绝回退均通过。HEVC双眼与左右单眼分别150帧硬编、150帧软件解码，无解码错误；独立原生编码速率29.63/32.90/33.70fps，不是浏览器HEVC帧率。原始证据见 `reports/device/debug-hevc-mono-r1/`。以上均为短窗口，不是持续60fps验收。

以下为先前部署和历史性能记录，历史 Python NV12 路径的描述不代表当前 GPU 入口：


独立候选 `/opt/mix-remote-video`。它从真实 KMS scanout 只读导出 DMA-BUF，按已有双眼映射旋转为横屏，再由所选 CPU/libyuv 或 GPU 入口转为 limited-range BT.601 NV12，交给实际 Venus V4L2 H.264 编码器及 WebRTC。默认请求 1280 宽、60fps；请求值和离线编译不表示实机达到该帧率。

r1 实测：Venus 对生成的运动 NV12 1280×672 输入完成300帧、约60fps硬编码及300帧软件解码；这不是双眼吞吐验收。真实CPU双眼转换分别约640宽53fps、960宽28fps、1280宽17fps，瓶颈是CPU转换。640×336实际WebRTC会话完成512个编码VCL AU、512个浏览器接收/解码帧，浏览器丢帧0；最后区间解码45.22fps、呈现40.31fps。输入/编码PTS匹配，RTP时间戳误差小于1个90kHz tick，未按裸码流约120fps的帧率字段发送。证据在 `reports/device/steamos-venus-r1/encoder-second.log`、`nv12-capture-*.log`、`browser-webrtc-640-progressive.json`。此后已显式选用原 GPU 入口并完成 8090 同源双眼观看：1280×672 浏览器稳定窗口为 35.44fps，重启后短窗口为 43.09fps（末秒 36.27fps），60fps 真实双眼目标尚未完成。GPU 专门化候选未确认改善，未选为默认；结果见 `reports/device/steamos-venus-r1/results.json`。

采集没有 DRM master/modeset 操作。并发 scanout 读取保持 `atomic_snapshot=false`：vblank 递增只证明新的扫描时刻，内容差异也不能证明获取了一帧完整的新 compositor 输出。统计必须分别保留实际采集 readback、编码 VCL access unit、浏览器接收/解码和浏览器视频呈现回调的计数。网页不使用 requestAnimationFrame 计算视频帧率，不从请求的60fps生成计数，不以旧图重复刷新充数。

`build.py --output <新的build目录> --backend <交叉构建的mix-debug-server>` 使用既有 Frame sysroot 编译采集程序并生成摘要包。WebRTC/DTLS/SRTP 依赖独立安装到同前缀的 `lib`；`mix-video-server` 与调试服务共享同一 Rust 构建产物，以独立进程提供8091服务；无需 Python GI。本服务包不覆盖该目录，也不更新全局库。安装前验证两个包的摘要和实际设备身份。

在硬件窗口拥有者控制的 SteamOS root 会话中，进入解压后的服务包目录：

```sh
python3 install.py --address <头显的局域网IPv4地址>
# 默认复用既有 /var/lib/mix-debug-console/access.token，不重写该密钥。
systemctl start a081-remote-video.service
```

安装不会启用或启动服务，不连接相机、IIO、手柄或追踪数据源。服务只有用户创建观看会话后才开始图像采集与编码；网页提供退出按钮，`systemctl stop a081-remote-video.service` 结束该服务。服务属于 `a081-remote-view.service`，VR正常停止观看时应先停止编码，再取得首次资源快照。服务启动时应刷新自身私有 GStreamer registry，以重新发现新内核分配的 Venus 节点。

观看入口及配置保存于 `/var/lib/mix-remote-video/` 的root私有文件，入口不含密钥；程序不把密钥写入URL文件或日志。浏览器可在密钥输入框授权，兼容fragment输入时会立即清除地址栏fragment，API通过Bearer授权。默认复用8090调试台的既有root私有密钥，不重写它；独立部署可显式选择 `--token-file /var/lib/mix-remote-video/access.token` 创建新密钥。服务仅绑定明确的私有IPv4地址，单会话占用由服务端管理。

网页默认 H.264/WebRTC，并提供640/960/1280/1600/1920/2560/3200/3664宽度、15/30/60请求帧率及1–100Mbps目标码率选择（13档预设和整数自定义）；宽度超过2560时最高请求30fps，原生双眼3664×1920使用Level 5.1。目标码率是编码器请求值；实际网络码率由浏览器累计字节的时间差计算。JPEG仅保留在8090维护调试台中作为备用入口；此独立页不自动请求JPEG。与原调试台姿态/设备区域的 8090 同源集成、分辨率/码率切换、全屏与正常退出已实测，原100ms姿态新鲜度门不变。

验收应记录同一实际会话的编码设备名/节点、NV12/H.264协商caps和PTS、采集/编码计数及浏览器 `getStats()` 累计值与差值。呈现使用 `requestVideoFrameCallback.presentedFrames` 和真实回调间隔；这是浏览器呈现统计，不是显示器物理扫描测量或头显到浏览器端到端延迟。静态内容、启动时间、断流和浏览器未提供的统计字段都需要单独说明。所有实机功能、帧率及回收结果仍以协调者保存的原始证据为准。

当前阶段计时在 `/api/session/<id>/stats` 的 `phase_latency` 中增加采集完成→后端读完整帧、读完→编码器输入、编码器输入→解析后的VCL输出，以及两个包含前述阶段的总时间。采集完成是helper完成GPU/CPU读回后、stdout传输前的真实 `captured_ns`；后续边界均取同设备 `CLOCK_MONOTONIC`。PTS仅用来精确匹配记录，不作为绝对时钟，也不按帧号补时。编码输出在 `h264parse` 的src probe入口计时，之后确认是真VCL AU，因此该阶段包含解析和调度，不是纯硬件编码耗时。

每个阶段只保留最近300次计时、全程计数、P50/P95/最大值。待匹配PTS最多1024条；重复键、未匹配输出、淘汰和时间顺序错误单独计数，不插值。采集→读完包括helper比较/复制、管道等待和后端读帧处理，不称为纯管道耗时。各阶段有包含关系，不能把其分位数相加。

浏览器分别显示本统计间隔的 `ΔjitterBufferDelay / ΔjitterBufferEmittedCount`、`ΔtotalDecodeTime / ΔframesDecoded`，以及实际选中ICE candidate pair的 `currentRoundTripTime`；字段缺失、计数重置或分母为0时显示未知。原始累计值和差值保留在 `mixVideoObservation.browser_latency`，定义遵循 [W3C WebRTC Statistics](https://w3c.github.io/webrtc-stats/)。这些值与设备阶段独立，不提供未经同步测量的单程或端到端延迟。阶段统计已实测：管道扩为 1MiB 后，采集完成→读完整帧的 P50 从 15.964ms 降为 5.025ms；这没有使真实视频达到 60fps。

采集程序默认仍为原CPU入口。显式 `--capture` / `MIX_VIDEO_CAPTURE` 只允许该入口、`/opt/mix-remote-video-gpu/nv12-capture-gpu` 或独立 `/opt/mix-remote-video-gpu-specialized/nv12-capture-gpu`；设备当前显式选用已验原 GPU 入口，专门化入口保留但未选中；程序本身不自动切换。管道缓存、采集节拍、去重、PTS、编码参数和现有追踪门限不因阶段计时而改变。

全分辨率已在真实浏览器解码验证：3664×1920/30fps/40Mbps请求，热态短窗口实际解码21.63fps、浏览器丢帧增量0。2560×1342/60fps/37Mbps请求实测33.02fps，1280×672/60fps/8Mbps请求实测37.65fps。以上是独立短窗口，非持续性能承诺。原生档采集P50约19.27ms，采集完成至读完整帧28.32ms、编码输入至VCL输出17.39ms；分位数不能直接相加。证据 `reports/device/debug-ui-fullres-r1/`。

r1 三相与屏幕流共用 `/run/mix-video-control` 的 1–4 路租约。8090 的“全部关闭”锁禁止新建并撤销旧代会话；恢复只解锁，用户另行开流。是否释放以全进程 Venus FD 扫描为准，不能只看租约为空。两服务需要 CAP_DAC_READ_SEARCH/CAP_SYS_PTRACE 才能完成其他 UID 的句柄审计。

设备 GStreamer 1.24.2 的 V4L2 编码器每个已激活输入池的后续帧漏掉一次 `gst_object_unref(opool)`，导致 NULL 后残留池的重复设备 FD。私有插件构建器 `build-v4l2.py --archive <gst-plugins-good-1.24.2.tar.xz> --output build/<output>` 校验官方源码归档 SHA-256，仅移动该 unref 到激活判断之外。构建产物 `obj/sys/v4l2/libgstvideo4linux2.so` 单独部署到 `/opt/mix-remote-video/lib/gstreamer-1.0/`；系统插件保持原样。安装前停止两个视频服务并保存原文件/不存在状态，清理各自插件 registry 后重启；回退移除私有覆盖并重启视频服务。原始归档、源码、补丁后的源码和构建日志保留在构建目录，实测证据见 `reports/device/phase-video-r1/`。常规服务包不覆盖整个私有库树。
