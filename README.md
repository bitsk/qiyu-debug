# qiyu-debug

同仓的调试前端、Rust 后端与视频 helpers。

实际功能源码首次按仓库所有权整理，来源和导入前后 SHA-256 见 `provenance/source-import.json`。原有历史构建与设备结果不自动等同于拆分后的设备验收。

构建入口和依赖见下方；工程 Wiki/Blog 与跨仓库规划集中在 [qiyu-docs](https://github.com/bitsk/qiyu-docs)。

## 目录与构建

- frontend/: 现有 HTML/CSS/JavaScript。
- backend/: Rust HTTP/WebSocket、遥测、视频控制和 WebRTC；Cargo.lock 固定依赖。
- video/ 与 helpers/: 原生采集/编码、GPU 转换、相机与 OpenVR 观测工具。
- api/: 从现有实现整理的接口索引，生产者 wire 合同由对应仓库维护。

`python3 tools/build.py --sysroot /path/to/arm64/gstreamer/sysroot --output /new/build`

生成的 debug/ 包把前端与服务放在一起，适配后端既有资源加载规则。离线验证使用 `mix-debug-server --offline --listen 127.0.0.1 --token-file /private/generated/token`；密钥按现有方式在本地生成，不进入 Git。摄像头来自生产者 tap；当前硬件编码、采集/解码/呈现统计边界保留。
