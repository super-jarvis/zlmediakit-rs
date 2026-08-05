# 项目发现

## 2026-08-02 前端完整性与鉴权审计

- 当前前端是无构建依赖的静态管理页，包含控制台、拉流代理、录制、推流转发、FFmpeg 源和服务器信息 6 个模块。
- 后端 `getApiList` 列出 48 个管理 API，前端实际调用约 17 个；GB28181/RTP、转码、会话、录像文件、流详情/截图、配置修改和线程监控尚未接入。
- 前端 `apiGet` 只检查 HTTP 状态，不检查 JSON `code`，后端以 HTTP 200 返回业务错误时会出现假成功提示。
- 播放器只接入 HLS、HTTP-FLV 与 WS-FLV；HTTPS 页面仍会生成 `ws://` 地址，存在 mixed-content 失败。
- `ServerConfig` 已有顶层 `secret` 字段及默认值，但当前 `HttpSession::handle_api` 直接分派管理 API，没有统一 secret 校验。
- `getServerConfig` 当前把 `api.secret` 放入响应，前端又会原样显示整份 JSON；实现鉴权时必须脱敏。
- 管理 API 当前均通过 GET query 参数执行，包括修改配置、关闭流、踢会话和重启；本轮先保持兼容，同时由 UI 增加确认和统一鉴权。
- 会话 API 返回连接 ID、对端/本地地址、协议、创建时间和绑定流，可直接支持筛选与单条/批量踢出。
- `getMediaPlayerList` 能返回单流订阅者及会话信息；`getMp4RecordFile` 按 app/stream/period 返回归档；`getThreadsLoad`/`getWorkThreadsLoad` 返回线程负载列表。
- 转码 API 支持 H.264/H.265、缩放、码率、自定义目标 app/stream，并返回可用于停止任务的 key。
- API secret 可复用 `HttpSession` 已持有的 `Arc<StreamAuth>`，无需扩展 `HttpServerConfig` 和大量构造调用；当配置 secret 为空时可作为显式关闭管理鉴权的兼容模式。
- GB28181 设备列表已包含在线状态、IP/端口、最后心跳、厂商/型号和缓存通道；通道对象包含 ID、名称、ON/OFF、厂商、型号及经纬度，足以直接构建设备/目录/点播页面。
- VOD 播放签名算法是 `sha256(secret|vhost|record|app/stream/file|play)`；前端可用 Web Crypto 只生成摘要 URL，不需要把明文 secret 放入录像链接。
- `runtime_config_snapshot` 原先只在首次访问时用 `ServerConfig::default()` 初始化，未同步配置文件和 CLI 覆盖后的有效值；这会让管理页显示错误端口。现增加启动后同步，且把 WebRTC、SRT、GB28181 的启用状态和端口纳入脱敏配置响应。
- WebRTC 服务器已为 POST/OPTIONS/DELETE 响应提供 CORS，可由不同端口的管理页直接完成 WHEP SDP 协商；SRT 不能由浏览器原生播放，管理页提供准确推流 URL 和启用/端口状态。
- 默认配置 `http.www_root="./www"` 原先会被再次拼接到已指向仓库 `www/` 的 bundled root，最终得到不存在的 `www/www`；启动路径解析现把空值、`.`、`www`、`./www` 都归一为 bundled `www/`。


## 2026-08-02 协议完整性审计

- 原 FLV demuxer 会丢弃 FLV 音视频头、忽略配置帧与 CTS，并把未知编码强行认作 H.264/AAC；这会破坏后续无损转封装。现已保留完整 payload 和语义元数据。
- 原 MP4/fMP4 测试只检查 box/header 字节存在，不足以证明播放器可用。真实 ffprobe 首先发现普通 MP4 header 无效，随后发现 fMP4 的空表和 fragment offset 无效；两类容器现均通过真实解码。
- classic FLV 可以承载 H.264/H.265、AAC、MP3、G.711A/U；Opus/L16 等组合不能直接写入。FLV 输出现在对不支持组合返回错误，避免把原始字节静默包装成看似合法的 FLV tag。
- MP4 `stco`/`co64` 是文件绝对偏移，`stsc` 条目描述从 `first_chunk` 开始的一段 chunk 范围；原 demuxer 对两者的理解都只适配了宽松的自产测试数据，无法可靠读取 ffmpeg 文件。真实 HEVC 输入测试已覆盖修正后的标准语义。

- WSL 当前既无 `opus` 也无 `fdk-aac` pkg-config 库，但 Ubuntu 26.04 软件源可提供 `libfdk-aac-dev`；因此 AAC→Opus 实现可验证需要安装原生开发依赖，不能仅靠打开 Cargo feature。

- SRT 时间戳问题比初审更严重：除固定 40 ms 外，PES payload 起点也使用了错误字段，且每次网络接收末尾都会无条件发布未完成 PES。三者会共同造成截断、漂移和播放异常。

- RTSP 音频不仅 SDP 固定 AAC，原 packetizer 也通过 FLV SoundFormat 猜测并把非 AAC 统一发到 PT98；已改为以 `CodecId` 为准，并从 source track 把实际 RTP clock rate 传入发送任务。

- `FlvMuxer::write_tag` 被 HTTP-FLV、WS-FLV、FLV recorder 和 MP4-VOD remux 共用，是统一修复 FLV 输出的最佳边界；RTMP `handle_play` 另有两条直接把 `frame.data` 写入消息的 cached/live 路径，也必须接入同一转换。

- HLS 旧测试直接绑定私有 `extract_hevc_config` 名称，统一转换重构后造成测试编译失败；这是测试耦合而非运行时回归，现已改为针对 hvcC record 的解析测试。

- HLS segment 与 live TS 原先分别调用 FLV 专用转换函数；现已改为共享 core 规范化入口，AVC/HEVC/AAC 配置提取也可接收去掉 FLV 头的 decoder config。

- 项目通过 `MediaSource` 广播 `MediaFrame` 实现协议解耦，但 `MediaFrame.data` 没有记录负载格式；当前不同入口实际发布了不同表示。
- RTMP 输入保留 FLV Video/Audio payload；RTSP H.264/H.265/AAC 和 WHIP H.264 会主动构造成 FLV/AVCC 风格 payload；WHIP Opus 保留原始 Opus；SRT/GB28181 发布 Annex-B 视频或 ADTS/原始音频。
- RTMP、HTTP-FLV、RTSP packetizer、HLS-TS muxer 多处默认 `MediaFrame.data` 是 FLV payload。因此 SRT/GB28181 到这些出口以及 WHIP Opus 到非 WHEP 出口没有完整实现。
- RTSP 输出 SDP 只按 H.264/H.265 视频和 AAC 音频生成；任意存在的音频轨都会被声明为 MPEG4-GENERIC/AAC，和 Opus、MP3、G.711 数据不一致。
- 默认 server 依赖 WebRTC crate 时没有启用 `transcode`/`aac-transcode` feature，因此默认 RTMP AAC → WHEP 没有音频。
- MP4 与 fMP4 muxer 当前把 `frame.data` 原样写入 `mdat`。当来源是 RTMP/RTSP/WHIP H.264 时，其中仍含 FLV 视频 5 字节头；AAC 仍含 FLV 音频 2 字节头，生成样本不符合 MP4 负载要求。
- SRT TS 解复用没有读取 PES PTS/DTS 或 PCR；连接层按每次 `srt_recv` 固定增加 40ms，时间戳与真实帧/网络包边界不一致。
- 当前 E2E 已覆盖 RTMP→WS-FLV、RTMP→RTMP、RTMP 推拉代理、RTSP 输入/拉流、WHIP→WHEP、MP4 VOD→FLV，以及直接构造 MediaFrame→HLS/WS-TS/WS-fMP4。
- 尚无完整入口×出口矩阵；SRT 没有网络 E2E，GB28181 主要是单元测试，CMAF/DASH 没有真实播放器/ffprobe 验证，多数录制测试只验证 box/header 而非媒体可解码性。
- `CodecId` 虽列出 H.264/H.265/AAC/G.711/Opus/MP3/L16/VP8/VP9/AV1/JPEG/MP2V/MP2A，但枚举存在不代表协议收发链路完成。

## 初始状态

- Git 分支：`master`，跟踪 `origin/master`。
- 工作区在任务开始时干净。
- Rust Cargo 工作区包含多个媒体协议/容器/服务相关 crate（初步可见 core、codec、http、rtsp、rtmp、hls、flv、mp4、webrtc、srt、transcode、server）。
- codebase-memory-mcp 当前未索引本项目。

## 待补充

- 项目意图与核心架构
- CI 失败根因
- 修复方案与验证证据

## 项目意图（初步）

- 项目名为 ZLMediaKit-RS，是 ZLMediaKit 的 Rust 实现。
- 目标是提供完整流媒体服务器能力，覆盖 RTMP/RTMPS、RTSP/RTSPS、HTTP-FLV、WebSocket-FLV、HLS TS/CMAF、DASH、WebRTC WHIP/WHEP、SRT 等协议。
- Cargo workspace 分层明显：`core` 提供媒体源、会话、事件总线、认证、录制等抽象；`codec` 处理 H.264/H.265/AAC/G.711/PS；协议 crate 分别负责 RTMP、RTSP、HTTP、HLS、FLV、MP4、WebRTC、SRT；`transcode` 管理转码；`server` 是可执行入口。
- 仓库包含大量协议和端到端测试，以及 Dockerfile、docker-compose、Web 播放页面和示例配置。

## 运行时架构

- `zlmediakit-server` 读取 TOML/CLI 配置后建立共享的 `EventBus`、`MediaSourceManager`、认证和 Hook 客户端，再按配置并发启动 RTMP、RTSP、HTTP/API、WebRTC、SRT、GB28181 服务。
- 各输入协议把解析后的音视频统一发布为 `MediaFrame` 到 `MediaSource`；播放协议和录制器通过广播订阅消费同一媒体源。这是整个项目的核心“协议解耦/互转”枢纽。
- server 同时管理录制（HLS/FLV/MP4）、拉流代理、RTMP 推流转发、FFmpeg 源、实时转码、边缘回源和集群推送等 supervisor。
- SRT 使用同步 C FFI，因此独立放在 blocking 线程；其当前 socket ABI 与链接库实现是 Linux 专用，这也是 CI 应暂时只承诺 Linux 的原因。

## CI 现状（初步）

- 存在三个工作流：`ci.yml`、`docker-publish.yml`、`release.yml`。
- 最近提交集中修改了 CI：最新提交添加 Docker Buildx 并暂时禁用二进制发布；前序提交暂时禁用了 macOS/Windows 构建并更新 Actions 依赖。
- `ci.yml` 在 Ubuntu stable Rust 上执行，需继续读取完整步骤并复现。

## CI 配置细节

- `ci.yml` 的 Ubuntu `test` job 依次运行 `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace --tests`、`cargo build --release`。
- 同一工作流还无条件运行 macOS 和 Windows 的 `cargo build --release`。
- CI 没有安装任何原生系统依赖；但项目包含 SRT 和可选 FFmpeg/转码能力。Docker 构建阶段明确安装 `cmake`、`build-essential`、`pkg-config`、`libssl-dev`、`libsrt-gnutls-dev`、`ffmpeg`，说明纯净 runner 可能缺少构建依赖。
- Docker 发布工作流使用 Buildx、GHCR 登录、metadata-action 和 build-push-action；release 工作流只保留 release-plz job，多平台二进制发布已整段注释。
- Dockerfile 固定 builder 为 `rust:1.80-slim-bookworm`，而 Cargo 清单未声明 `rust-version`；需结合实际依赖锁文件和失败日志判断是否存在 MSRV 问题。

## GitHub 状态查询

- Web 抓取无法直接读取目标仓库 Actions 页面，搜索索引返回的是上游 C++ ZLMediaKit 而非本 Rust 仓库；不能据此判断当前失败。
- 下一步改用仓库远端信息和 GitHub CLI/API 获取实际 run/job 日志。
- 仓库远端为公开仓库 `super-jarvis/zlmediakit-rs`（本地通过 `gh.idea8.top` 镜像访问）。
- 最新提交 `b13b147` 同时触发的三个 workflow 全部失败：CI run `30702176711`、Docker Publish run `30702176716`、Release run `30702176729`。
- 三个失败均发生于 2026-08-01 13:39 UTC；下一步逐个读取 jobs/steps，判断是否一个共享原因还是多个独立问题。

## Actions 实际失败点

- Ubuntu CI：format 与 Clippy 成功，`cargo test --workspace --tests` 退出 101；需读取日志确定具体失败测试。
- macOS CI：`crates/srt/src/ffi.rs` 使用 Linux 形态的 `libc::sockaddr_in`，macOS 缺少 `sin_len` 字段且端口类型不匹配。
- Windows CI：`libc` crate 在 Windows 不提供 `sockaddr_in`、`socklen_t`、`in_addr`、`AF_INET`；当前 SRT FFI/服务器代码没有平台抽象或 cfg 限制。
- Docker Publish：runtime 阶段安装 `libsrt-gnutls1.5` 时 apt 以 100 退出；Debian bookworm 包名疑似写反，需要在 WSL/Debian 源中核实。
- Release：`release-plz/action@v0.5` 收到无效输入 `GITHUB_TOKEN`；该 action 当前合法输入包含 `token`，workflow 使用了错误键名。
- 所有 workflow 还有 Node.js 20 action 被 runner 强制运行于 Node.js 24 的弃用警告，但目前这些不是失败原因。

## 系统化验证审计（2026-08-02）

- 代理和推流 supervisor 原本用 `contains_key` 后 `insert`，两个并发请求可同时通过检查并重复启动；发送 command 失败时还会留下虚假的 active 条目。改用 `DashMap::entry` 原子占位，并以任务 marker 精确回滚，已用 16 线程竞争和 receiver-drop 回归覆盖。
- 媒体图的 broadcast channel 是有界实时通道；性能基线若只测 producer 会把调度延迟误判为系统吞吐。新增基线用每 128 帧 ACK 保证 4 个订阅者均完整消费，再统计 50,000 源帧和 200,000 次实际投递。
- FLV header 的 `data_offset` 原先可越过当前 buffer 直接 `advance` panic；未知 tag 通过递归跳过，长恶意链可导致栈溢出。现已改为 header 边界校验和迭代跳过，20,000 个未知 tag 回归可正常解析后继视频 tag。
- MP4 sample table 的 input-declared entry count 原先直接用于 `Vec::with_capacity` 和嵌套循环，单字节变异即可造成明显资源放大。`stts/stsz/stsc/stco/co64/stss` 现按 box 长度校验，样本数量与文件数据上限交叉约束；同一 fuzz-smoke 从超过 60 秒降至约 0.02 秒。
- fMP4 `trun.sample_count` 具有同类风险，尤其 flags 不携带逐样本字段时循环次数不受 trun 长度限制。现按 flags 计算表宽，或按默认 sample size 与可用媒体数据约束，并对样本偏移做 checked arithmetic。
- 当前解析器 fuzz-smoke 覆盖 AMF、FLV、MPEG-PS、MP4 和 WebSocket 的随机、截断和结构化变异输入；这是快速确定性回归，不替代后续 cargo-fuzz 持续语料库。
- SRT Rendezvous 的旧测试在双方刚连接后只发送一个很短消息并固定等待 500ms；在 live/TSBPD 启动期该消息可能被丢弃，随后 sender 先关闭又令 receiver 报无效 socket。新测试改用 `srt_rendezvous` 专用 API、双 loopback IP/同端口拓扑，并持续发送生产等价的 1316-byte TS 批次直到接收 ACK；10 次连续复跑及 workspace 并行套件稳定通过。
- 系统门禁应拆分长链命令保留诊断：本轮组合命令在全目标测试已通过后无错误文本退出，单独 release 构建、性能基线和前端语法检查均成功，确认不是代码失败。

## 2026-08-03 外部 CI 状态复核

- GitHub 公共 API 显示远端 `master` 仍停在 `ea37fcba`（2026-08-01）：旧 CI 成功、Docker Publish 成功、Release 在 `release-plz/action@v0.5` 步骤失败；当前本地 `master` 比远端 ahead 3，且新的 `.github/workflows/cross-platform.yml` 尚未跟踪，所以远端没有任何 Linux/macOS/Windows 新矩阵结果。
- 本地旧 `ci.yml`/`release.yml` 已作为历史文件位于 `.github/workflows_bak/`，其中 action 已升级到 `checkout@v6`/`cache@v5` 且 release-plz 参数已改为 `token`；它们不会被 GitHub Actions 加载，因此远端旧 Release 失败不会在本地现状自动重现。
- 新 cross-platform workflow 当前只在推送 `master/main` 或面向二者的 PR 时触发。若要在不直接改远端主分支的情况下取得真实 runner 证据，应允许任意分支 push，再从 `codex/` 验证分支触发。
- 当前 SRT 地址转换仍直接返回 `libc::sockaddr_storage` 并引用 `libc::sockaddr_in/in6/socklen_t/AF_INET`。这已处理 BSD 的 `sin_len`，但 `libc` 在 Windows Rust target 不暴露同一套 Winsock 类型，因此新 workflow 的 Windows `cargo check --all-targets` 很可能仍会在类型检查阶段失败；“check 不链接 libsrt”不足以绕过源码类型不兼容。
- workspace 已声明 `socket2 = 0.6`，而 `zlmediakit-srt` 仍单独只依赖 `libc`。优先方案是复用 `socket2::SockAddr` 的跨平台 ABI 指针/长度，避免自行维护 Unix 与 Winsock 的 sockaddr 布局。
- `socket2 0.6.5` 的 `SockAddr` 自带跨平台 native storage、`as_ptr()` 与 `len()`，并实现 `From<SocketAddr>`；SRT FFI 的 bind/connect/rendezvous 只需 opaque address pointer 和 C `int` 长度，因此可以完全移除 SRT crate 对 Unix `libc::sockaddr_*` 的源码依赖。
- 可持续 libFuzzer 不需要暴露新的内部 API：`FlvDemuxer::{feed,parse_header,parse_tag}`、`PsDemuxer::{push,next_pes,pending}` 与 `Mp4Demuxer::from_bytes` 都是公开入口。独立 `fuzz/` package 可覆盖三类高风险媒体解析器，既复用真实产品接口，又不把 test-only parser 复制进 harness。

## 本地环境与 SRT 平台问题

- 可用 WSL2 发行版为 `Ubuntu-26.04`，当前正在运行；另有卸载中的旧 `Ubuntu` 和运行中的 `docker-desktop`。
- `zlmediakit-srt` 直接链接 `srt-gnutls`，并在公共模块里无条件编译 Linux 风格 FFI。
- `socket_addr_to_sockaddr` 直接构造 `libc::sockaddr_in`；这精确解释了 macOS 的 `sin_len`/类型问题和 Windows 缺失 libc socket 类型问题。
- 仓库近期明确表达“暂时禁用 macOS 和 Windows 构建”的意图，因此优先考虑让 CI 与当前受支持平台（Linux）一致，而不是在本次 Actions 修复中扩张为完整跨平台 FFI 重构。
- 匿名 GitHub API 可以读取 job 状态和 annotations，但原始 job 日志要求仓库管理员权限；Ubuntu 失败将通过 WSL 运行相同命令复现。
- WSL `Ubuntu-26.04` 使用 Rust/Cargo 1.96.0；当前未安装可被 `pkg-config` 发现的 SRT 开发包，这与 CI 未安装系统依赖的配置相符。

## Ubuntu CI 本地复现

- 在未安装 SRT 开发库的 WSL 中运行原 CI 命令 `cargo test --workspace --tests`，稳定复现退出码 101。
- 具体根因是链接器 `rust-lld: error: unable to find library -lsrt-gnutls`；多个 HTTP 端到端测试目标因为依赖 `zlmediakit-srt` 而一起链接失败。
- 这与 GitHub job 的“Clippy 通过、tests 失败”完全吻合：Clippy 只检查代码，测试二进制需要实际链接动态库。
- Ubuntu CI 应在 Rust 步骤前安装 `libsrt-gnutls-dev`（以及为 native build 保留 `pkg-config`）；无需把问题误判为测试断言失败。
- 安装开发库后，整个 workspace 及所有测试目标已成功编译；首次验证在 2 分 55 秒完成编译、刚开始执行 E2E 测试时触发 180 秒工具超时，之后的 Broken pipe 是进程被终止造成的验证中断，不是测试断言失败。
- 放宽超时后，SRT 链接问题消失，已执行到转码集成测试；其中 4 个用例因 WSL 没有 `ffmpeg` 可执行文件失败。测试代码把 FFmpeg 作为运行时测试前置条件，CI 应显式安装而不能依赖 runner 镜像偶然预装。
- 在转码测试之前运行的 codec/core/flv/hls/http/mp4/rtmp/rtsp/srt 等测试均通过。
- WSL 安装 FFmpeg 8.0.1 后，`cargo test -p zlmediakit-transcode --test integration_test` 的 6 个用例全部通过，包括 H.264/H.265 双向转码与缩放。

## Docker 验证状态

- Docker Desktop 29.6.1 可从 WSL 使用。
- 完整 `docker build` 首次运行超过 10 分钟工具上限，被终止前没有生成最终 `zlmediakit-rs:ci-fix` 镜像。
- BuildKit 已保留大量中间缓存；因此后续不原样重复，而是分别验证 Debian 运行时依赖安装、Cargo 依赖 MSRV，并利用缓存进行最终构建。
- WSL 未安装 `jq`，Cargo metadata 的 MSRV 汇总将使用只读 Python JSON 解析。
- 一次性 Debian 容器验证受本地 Docker/apt 网络速度影响在 180 秒超时；没有出现原 Actions 中的“包不存在/apt 100”快速失败。Debian 官方 bookworm 索引已经直接确认正确包名为 `libsrt1.5-gnutls`。
- BuildKit 历史日志解释了完整构建超时：10 分钟主要耗在从 Docker Hub 下载 `rust:1.80-slim-bookworm`（29 MB 层仅下 12.58 MB，251 MB 层仅下 10.49 MB）；builder/runtime 的 apt 步骤随后因客户端超时被取消，尚未进入 Cargo 编译。此处是本地网络瓶颈，不是 Dockerfile 新错误。
- Runtime apt 已开始成功解析 Debian bookworm 仓库，修正后的包名没有像 GitHub 原 run 那样立即报 apt 100；官方包索引仍是完整证据。
- `Cargo.lock` 中实际编译的是 `time 0.3.54`，其缓存清单声明 `rust-version = "1.88.0"`；Docker builder 固定的 Rust 1.80 无法满足锁定依赖，包名修复后还会暴露第二个构建失败。
- Docker Hub 官方索引确认 `rust:1.96.1-slim-bookworm` 标签存在；将 builder 升级到 1.96.1，与本地成功验证的 Rust 1.96 工具链同代并满足锁定依赖 MSRV。

## 最终测试（进行中）

- 安装 `libsrt-gnutls-dev` 与 FFmpeg 后，WSL `cargo test --workspace --tests` 全量通过，覆盖 codec/core/flv/hls/http/mp4/rtmp/rtsp/srt/transcode/webrtc 及全部 E2E 测试。
- 下一步补齐格式、Clippy（warnings-as-errors）和 release build，随后检查最终 diff/YAML。
- WSL 上 `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo build --release` 全部成功；release 产物已生成。
- 三份 workflow 均通过 PyYAML 语法解析，`git diff --check` 通过。
- 产品改动为 3 个文件：`ci.yml`、`release.yml`、`Dockerfile`；另有本技能要求生成的 3 个未跟踪工作记录文件。
## 2026-08-05 RTSP 客户端鉴权缺口

- RTSP 拉流代理原先在 DESCRIBE 401 时直接失败；推流客户端只有 Digest。两者都缺少 Basic 上游认证，这是 ZLMediaKit 常见代理/推流部署（设备或 CDN 常配 Basic）无法联通的断链。
- `client_transport.rs` 现集中提供 Basic 与 Digest 的 `WWW-Authenticate` 挑战解析并自动选择 scheme；后续其他协议客户端需要 HTTP Basic 时可复用同一入口。
- 认证重试使用标准一次挑战语义：首个请求不带 Authorization，401 后按 challenge scheme 重试一次；凭据来自 URL `user:pass@host`。错误口令会保留上游 401 语义返回错误，不静默降级。
- WSL 运行 `cargo fmt --all` 会把 Windows 工作树 CRLF 文件批量转成 LF，使 98 个未修改文件整体进入 diff；恢复方式是 `git checkout -- .` 后只重放目标改动，diff 校验使用 Windows 原生 Git（`git diff --check` 通过）。
- 本轮原生 HLS 拉流缺口：`EXT-X-KEY:METHOD=AES-128` 与 `EXT-X-BYTERANGE` 此前被忽略（segment 直接喂 demuxer），加密或分段清单会解码失败；补充 `aes`/`cbc`/`hex` 依赖后按 RFC 8216 实现。
- 解密语义：IV 优先取 tag 显式 IV（`0x` 前缀 hex），否则用媒体序列号大端 128-bit 回退；key 按解析后的 URI 缓存，避免同清单重复拉取。`decrypt_padded_mut::<Pkcs7>` 要求输入为 16 字节整数倍，与 HLS segment 的 PKCS7 填充契约一致。
- BYTERANGE 细节：省略 offset 的段按“同资源前一 range 结束处”续读（`range_ends` 链），显式 offset 直接使用；去重键从 URL 升级为 `(URL, range)`，否则同一资源多段（各带不同 range）会被误判为重复而跳过。
- 测试服务器按请求路径路由（`serve_routes`），避免依赖客户端请求顺序；AES 段在 segment 连接之前先取 key，若用固定顺序 serve 会因连接次序不同把 segment 当 key 返回。
- RTSP 服务端此前只会校验 Digest `Authorization` 并只签发 Digest 挑战，Basic 凭据会被当匿名拒绝、Basic-only 客户端无法接入；现 core `StreamAuth::check_basic` 按用户名查用户表、未知用户名回退共享 secret，与 Digest 的 `digest_password` 语义一致。
- 服务端 `WWW-Authenticate` 改为同时宣告 Basic 与 Digest（Basic 在前）：`authorization_header` 客户端按前缀识别，因此本项目客户端会优先走 Basic；既有 Digest-only 客户端仍可解析其中的 Digest 部分。
- Basic 校验不依赖 realm（RFC 7617 凭据直接内嵌于 header），故 `is_authenticated` 中 Basic 分支在 realm 尚未协商时也可校验；Digest 分支仍要求 realm 已就绪。
- 两个既有 Digest E2E 断言原为 `starts_with("Digest")`，与双挑战格式冲突，改为 `contains("digest")`；未改变 Digest 协商、nonce 提取与响应校验逻辑。
- SRT 跨平台构建：WSL 内交叉 `cargo check` 复现 CI Windows 门禁。已装工具链的 `multirust-channel-manifest.toml` 内嵌清华镜像绝对 URL，`RUSTUP_DIST_SERVER` 被忽略；需从其他镜像（USTC）手动下载同版本 std 并解压进 `lib/rustlib/`。
- Windows MSVC 目标无法在 Linux 交叉检查：传递依赖 `ring` 的 C 构建需要 MSVC `lib.exe`/cl；改用 Windows GNU 目标 + `gcc-mingw-w64` 让 ring 用 gcc 编译。SRT 及整个 workspace（含 webrtc、rustls、tokio）在 `x86_64-pc-windows-gnu` 上 `cargo check --all-targets` 通过，证明 SRT FFI 无 Unix-only 类型。
- macOS 目标同理被 ring 的 C 构建（需 Apple clang/SDK）卡在交叉检查边界；真实覆盖由 CI `platform-check` 在 `macos-latest` 原生 runner 完成，本地无法替代。
- 第③缺口无需改源码：SRT FFI 上一轮已从 `libc::sockaddr_*` 改为 `socket2::SockAddr`+opaque 指针，Windows-GNU 整依赖图类型检查即为其强证据。

# 2026-08-02 ZLMediaKit 开源能力对齐发现

- 当前项目的主流 H.264/H.265/AAC 直播链路已经闭环，但通用 RTP sender 是最靠前的架构断点；现有 `startRtp` 是 GB28181 SIP INVITE，不等价于 ZLM `startSendRtp`。
- RTSP session 内已有 H.264/H.265/AAC/Opus/G.711/L16/MP3 RTP packetizer 与 UDP/TCP interleaved 发送逻辑，可提取为通用 RTP packetizer，避免重新实现编码负载。
- 当前 `MediaFrame` 已带 `track_id`、codec、DTS/PTS、payload format 和 config-frame 标识，足以驱动独立 RTP sender。
- HLS `HlsDemuxer::parse_m3u8` 没有生产调用者，因此不构成原生 HLS 拉流客户端。
- Stream proxy 原生只区分 RTMP 和 RTSP；Stream pusher 固定调用 RTMP push client。
- SRT 初审时只有 listener/server 输入链路；现已补齐 Caller 拉流、MPEG-TS 输出与 Rendezvous，并通过真实 libsrt 网络闭环验收。
- 集群配置只有 RTMP push peers 与单个 RTMP/RTSP `origin_url`，没有多源站轮询和 HLS/HTTP-TS 溯源。
- WebRTC 已有 NACK/PLI/TWCC 和 DataChannel，后续应补缺失高级特性，不能将整个 WebRTC 模块判定为未实现。
- ZLMediaKit README 中的任意转码、JT1078、IPTV、S3、RTC 集群、AI、MCU 属于闭源专业版，本目标不以这些功能作为开源对齐验收项。
- ZLM `startSendRtp` 的默认 `use_ps=1` 是 PS-RTP，而 `use_ps=0` 才是 ES；现已实现 MPEG-2 pack/system/PSM/PES、90 kHz PTS/DTS、MPEG-2 CRC 和 RTP 分片，默认参数不再降级为 ES。
- PS 输出已覆盖 H.264/H.265、AAC（从 ASC/track 补 ADTS）、G.711A/U、MP2/MP3；Opus/L16 没有直接映射到 GB28181 PS，保持 ES 输出。
- 审计发现既有 PS demuxer 的 PES 可选头少解析了 MPEG-2 flags 字节，只能解析项目自造的非标准测试数据；已修正为标准 `flags1 + flags2 + header_data_length`，并保留长度为 0 的 GB28181 视频 PES 支持。
- `is_udp=0` 现已按 RFC 4571 使用两字节网络序长度前缀发送 RTP/TCP，ES 与 PS 共用传输层；下一断点缩小为 TCP passive、TS 输出、重连和端口复用。
- ZLM master 当前优先使用 `type=0/1/2` 表示 ES/PS/TS，旧 `use_ps` 只作为兼容覆盖；同时公开 `only_audio`、`startSendRtpPassive` 与 `listRtpSender`。本项目已按该契约补齐这些入口。
- TCP passive 由独立监听器接入，RFC 4571 连接断开后继续接受新客户端；TCP active 首次连接仍同步报错，成功后断线会按 1 秒退避重连。
- TS-over-RTP 复用现有 `TsLiveMuxer`，通过 core muxer factory 注入避免 core/hls 循环依赖；每个 RTP 负载最多包含 7 个完整 188-byte TS packet，不切断 TS packet 边界。
- `ssrc_multi_send=1` 已允许同一流/SSRC 按目标建立多路 sender；UDP 使用 `SO_REUSEADDR`/Unix `SO_REUSEPORT` 复用显式源端口，真实双目标测试确认两端同时收包且按 SSRC 一次停止全部。
- 通用 RTP 输出阶段已经闭环，下一阶段进入 GB28181 TCP 接收、PS/TS/ES 自动识别、音频与乱序恢复。
- GB28181 接收审计确认 `RtpServerManager::open` 只创建 UDP socket，API 虽有 `tcp_mode` 语义但未下传到 manager；TCP passive/active 接收尚未实现。
- `RtpStreamReceiver::handle_rtp` 已解析 RTP sequence/PT，却未使用 sequence 做乱序、重复和丢包处理；payload 类型完全依赖开户参数，没有从 PT/PS pack/TS sync byte 自动识别。
- RTP/TS 分支当前直接忽略 payload；已有 SRT TS demux 逻辑不在 GB28181 receiver 中复用，这是下一条最明显的断链。
- PS demuxer 会跳过 PSM 而不保存 stream type，导致 PS 内所有 `0xC0` 音频都按 receiver 默认的 G.711A 发布；AAC/G.711U/MP2 等无法可靠识别。直接 AAC RTP 也尚未剥离 RFC 3640 AU header。
- 上述 GB28181 接收断链已落实修复：UDP/TCP passive/TCP active 共用同一 ingest/reorder 层；`connectRtpServer` 对齐 ZLM 主动 TCP 两阶段控制语义；list/info 可观察 transport 与丢包统计。
- 自动识别当前以首个有效 RTP payload 锁定封装：PT 0/8 映射 PCMU/PCMA，PS 识别 `00 00 01 BA..BF`，TS 识别 188-byte sync，H.265 识别 VPS/SPS/PPS/AP/FU，其余视频回落 H.264。调用方仍可用 `type` 显式覆盖。
- SRT 的 TS demux 已抽成 crate 内共享入口供 GB RTP 复用；它目前覆盖 PMT 中 H.264/H.265 与 AAC，其他 TS 音频 stream_type 仍属于后续兼容矩阵工作。
- PS PSM stream map 现由增量 `PsDemuxer` 保存，接收侧已映射 `0x1B/0x24/0x0F/0x11/0x90/0x91/0x04/0x03`；AAC 非 ADTS RTP 会按 RFC3640 AU header 拆出原始 access unit。
- GB28181 阶段未完成项已缩小为“收到的 RTP 流按控制面再转推”与 SIP 语音对讲事务/音频发送；不能仅凭通用 `startSendRtp` 视为这两项已经完成。
- SIP 响应解析存在一个影响所有平台主动 INVITE 的旧缺陷：`SIP/2.0 200 OK` 曾错误尝试把第三个 token `OK` 解析为状态码，因此点播和新对讲都不会 ACK/激活；现已改为第二个 token，并由对讲网络闭环覆盖。
- 双向语音对讲当前明确以设备普遍支持的 G.711A/G.711U 为边界：从本机 MediaSource 选择音频轨，SDP 使用 PT 8/0 和 `sendonly`，200 SDP 决定设备接收地址，随后走通用 ES-RTP sender；不隐式执行任意音频转码。
- 接收 RTP 的受控转推无需建立第二套 GB 专用 sender：首包发布统一 MediaSource 后，现有 `startSendRtp` 即可选择 ES/PS/TS 和 UDP/TCP 目的端。新增真实 UDP 测试已验证接收 H.264 RTP 经缓存 GOP 转为新 SSRC/PT 后到达第二目的端。
- GB28181 阶段现已闭环；下一架构断点是代理层仍只有 RTMP/RTSP 原生客户端，HLS TS/fMP4、HTTP-FLV 和 HTTP-TS 都只能依赖外部 ffmpeg。
- 原生 HTTP 拉流已补上上述断点：代理 supervisor 按 URL scheme 调用 HTTP client，HTTP-FLV 保持 FLV payload，HTTP/HLS-TS 输出 Annex-B/ADTS，HLS CMAF 输出 AVCC/HVCC/AAC raw，使下游复用现有无损转封装路径。
- 既有 `HlsDemuxer` 只按 TS packet payload 生成粗粒度帧，不足以处理跨 packet PES；新 HTTP/HLS 输入改为复用 SRT 中已经验证的 PAT/PMT/PES 解封装状态，并新增网络 read 边界缓存。
- 新增 `Fmp4Demuxer` 支持 CMAF init + fragment 的常用 ISO BMFF 字段，覆盖 H.264/H.265/AAC、默认/逐 sample duration/size/flags、CTS offset 和多 traf 数据偏移；测试同时覆盖项目 muxer 的视频与 AAC 往返。
- 当前 HLS 原生客户端以未加密、URI segment 的开源核心路径为边界；尚未实现 AES-128/SAMPLE-AES、LL-HLS parts、EXT-X-BYTERANGE、cookie/session header 注入和自定义 CA，这些属于后续兼容性扩展，不隐式宣称支持。
- 推流 supervisor 原先无条件调用 RTMP push client；现按 RTMP(S)、RTSP(S)、SRT 分流。RTSP RECORD client 复用 core RTP packetizer，覆盖 H.264/H.265 与 AAC SDP/RTP、Digest challenge/retry 和 WebPKI 校验的 RTSPS；仍未实现 UDP RECORD 与 Basic 上游认证。
- SRT 原 FFI 常量来自错误的枚举序号，且把 libsrt 1.5 的三参数 `srt_recvmsg` 错声明为五参数；live 模式还错误调用 stream API。现已按本机公开 `srt.h` 对齐 ABI，并用真实 Caller/Listener/Rendezvous 网络测试固定行为。
- SRT Listener 的非阻塞 `RCVSYN=false` 会被 accepted socket 继承，旧处理器把首个“暂无数据”当断线关闭；accepted socket 现在切回 200ms 有界同步接收，并对 async/timeout 重试，既能响应停止又不会抢先断开。
- TS 解复用器原 PAT program loop 少跳过 5 字节、PMT program_info_length 多跳过 1 字节，导致 PMT/AAC PID 不生效；现按标准字段偏移解析，并保存 PMT stream_type 作为视频 codec，避免 H.264 P 帧被启发式误判成 H.265。
- 原 MP4 muxer 计算 `stts` 时把“当前时间戳减前一个时间戳”的结果写到了当前 sample；但 `stts` 表示当前 sample 到下一 sample 的持续时间，因此非固定帧率时间轴会整体漂移。现按后继时间戳计算当前 sample，末 sample 复用上一 cadence，并由非零 seek 网络测试固定。
- MP4 点播不应为每个协议各自解析文件。共享 `Mp4VodLibrary` 现在负责 `record` 应用映射、percent decode 后的路径校验、轨道元数据和关键帧切片；协议层只处理各自控制语义及输出封装。
- 点播地址统一为相对录制路径：RTMP `record/<path>.mp4`、RTSP `/record/<path>.mp4`、HTTP/WS `/record/<path>.mp4.flv`。RTSP seek 使用秒制 NPT，HTTP/WS 参数使用秒，RTMP AMF `play.start` 使用毫秒。
- 原边缘回源把配置中的上游 `origin_vhost` 错当成本地发布 vhost，非默认 vhost 永远等不到请求的媒体源；现已分离上游 URL 参数与本地目标 vhost，并以同流 single-flight 串行尝试多个候选。
- `MediaSource` 原有订阅 API 未被主要播放协议调用，因此 `NoReader` 事件在真实播放器上不会发生；现以 RAII 守卫覆盖所有持久 HTTP/WS、RTMP、RTSP 播放出口，且只有自动回源条目标记为 `on_demand`，避免误停手工常驻代理。
- 代理 supervisor 原来按 stream key 无条件删除 active entry，旧连接延迟退出会误删刚重建的新代理；现使用任务专属 `AtomicBool` Arc 身份执行条件删除。
- 原集群中继没有处理 `StreamUnPublish`、没有重连、没有按 peer vhost/app 过滤，并把已含 app 的 `peer.url` 再拼一次 app；现已修正 URL 语义并实现可取消的指数退避重连。
- 原 WHEP 仅从本地已存在 MediaSource 播放，未参与边缘回源、未登记真实 reader，也没有 PeerConnection 状态清理；WHIP 同样不会在断线/DELETE 后移除发布源。两类会话现均形成完整生命周期。
- webrtc-rs 0.12 的发送侧可可靠启用 RTX、NACK/PLI、TWCC 与读取 REMB，但其 `SettingEngine` 中 ICE-TCP mux 仍是注释 TODO；本项目已实现共享 UDP mux，不能把 TCP candidate 伪装成已支持。
- webrtc-rs 0.12 的 WHIP H.265 depacketizer不能可靠重组分片访问单元，因此接收侧只开放 H.264/VP8/VP9/Opus；WHEP 发送侧可输出 H.265/AV1，但最终可播仍受浏览器/客户端编解码支持影响。
- `TrackRemote::rid/ssrc/id/stream_id` 足以保留 WHIP simulcast 层身份；core 通过独立 `TrackDescriptor` 保存这些协议元数据，不污染通用容器 `TrackInfo`。WHEP 当前支持按 RID 显式选层，不宣称生成新的 `a=simulcast` 多编码输出。
- WHEP/WHIP 的普通 trickle candidate 可直接映射到 `RTCPeerConnection::add_ice_candidate`；仅包含新 `ice-ufrag` 的 fragment 并不足以执行完整 ICE restart，后者需要重建远端描述/凭证状态，不能与已完成的 PATCH candidate 支持混为一谈。
- webrtc-rs 0.12 实际提供 `RTCPeerConnection::restart_ice`、`RTCOfferOptions::ice_restart` 和 `gathering_complete_promise`。服务端可在 PATCH 同时收到新 ufrag/pwd 时替换当前 remote offer 的 ICE 凭据、清除旧 generation candidates、添加新 candidates、重启本地 gathering 并返回新的标准 SDP fragment；真实 E2E 证明媒体在该重协商后继续传输。
- WHIP 服务端与 WHEP 原生拉流客户端本质上共用远端 RTP 接收管线；把 `on_track`、轨道描述和 H.264/VP8/VP9/Opus 解包抽为共享入口后，可避免两条入口的 MediaFrame 语义漂移。
- 原生 WebRTC 远端客户端使用显式 `whep(s)://` 与 `whip(s)://` scheme，避免普通 `http(s)://` 媒体代理误判；信令内部仍映射到 HTTP(S)，支持公共 CA、IPv6 bracket authority、重定向和相对 Location，并复用 `[webrtc].ice_servers`。
- 原 `setServerConfig` 会把任意键写入全局 HashMap，并仅改变 `getServerConfig` 的展示；所有协议持有的 `StreamAuth` 和监听器完全不变。现先对安全敏感的 `api.secret` 做真实共享热轮换，其余配置在接线前明确报告需重启，未知键拒绝。
- API secret 热轮换必须是服务实例内共享状态，不能让并行测试和多个 HttpServer 读取进程全局 secret；因此运行值放在每个 `StreamAuth` 的共享 RwLock 中，全局 runtime config 只承担管理快照与持久化候选值。
- 原 `HookClient` 把启动时 `HookConfig` 直接存为不可变值，28 处调用都读取同一旧快照；此外 URL 解析会剥掉 `https://` 后仍向 80 端口发送明文。现在每次调用复制共享配置快照，HTTPS 使用 rustls/WebPKI 校验，动态更新不会跨异步等待持锁。
- `on_flow_report` 原来只有启动时已配置 URL 才创建任务，且周期被永久捕获；仅更新 HookClient 仍无法从关闭状态启用。使用 watch 修订通道后，调度任务常驻但空配置不发请求，URL/周期变化会立即唤醒并重新计时。
- 三个 TLS 服务原来在构造器中固定克隆 `TlsAcceptor`，替换磁盘证书不会影响新连接。`ArcSwap<rustls::ServerConfig>` 适合作为握手边界：每个 accept 取得当前 Arc，交换只影响后续握手，现有会话自然保留旧配置。
- TLS reload 必须先完整读取证书链、私钥并让 rustls 校验证书/密钥匹配，再执行原子 swap；若先替换路径或部分状态，损坏证书会让监听器失去最后一份可用配置。当前实现对失败保持 last-known-good。
- 监听地址不能用 `"{ip}:{port}"` 拼接，否则 `::` 会生成非法的 `:::port`；全局 `listen_ip` 统一解析为 `IpAddr` 后通过 `SocketAddr::new` 交给各协议，协议自己的 UDP/TCP 主动端也必须保留地址族。
- libsrt 的 bind/connect 接口接收通用 `sockaddr`，原先固定 `sockaddr_in` 既阻断 IPv6，也让调用长度不正确。改用 `sockaddr_storage` 后真实 `::1` Caller→Listener 可传输；结构体零初始化再赋值还避开了 BSD/macOS libc 额外长度字段的源码兼容问题。
- RTSP 拉推流已有 bracket-aware IPv6 authority；RTMP 拉推流原先各自用 `rfind(':')` 拆地址，会把 IPv6 尾段误当端口。两者现在复用同一个解析器，并保留 `rtmp://[::1]:port/app/stream` 的正确 Host/地址语义。
- server crate 目前只有二进制入口，把嵌入能力建立在其中会迫使宿主复制整个启动编排。生产协议本来就围绕 core 的 `MediaSourceManager` 互通，因此稳定 SDK 应放在 core：对 manager 做生命周期安全的 publisher/subscription/snapshot 门面，同时保留 manager getter 供宿主按需装配 listener。
- 跨平台 CI 需要区分“源码/类型兼容”与“本机系统库集成”：macOS/Windows 的 `cargo check --all-targets` 能捕获 libc 结构、条件编译和 API 差异，但不会证明 libsrt 可链接运行；后者当前由 Linux 真实测试覆盖，macOS/Windows native SRT build/run 仍需后续 runner 验证。
- 系统化验证审计发现，当前仓库没有 `#[ignore]` 长稳测试、压力测试、fuzz harness 或 benchmark；现有网络 E2E 主要证明单链路正确性，不能作为异常输入不 panic、并发媒体图无泄漏或性能无明显退化的证据。
- 生命周期已有的强证据集中在 RTP stop、GB SIP stop、WHEP/WHIP DELETE、边缘无人观看取消和 cluster retry 可取消；`run_proxy_supervisor`/`run_pusher_supervisor` 的失败后状态清理与快速同 key 重建仍应补并发测试，避免只验证成功链路。
- 模糊测试应优先覆盖无状态或增量二进制解析边界（AMF、FLV、PS/TS、MP4 box、WebSocket frame、SDP/ICE fragment），并设置输入长度上限；网络协议完整状态机留给故障注入测试，避免 fuzz target 被 I/O 与超时主导。
- FLV demuxer 的 `parse_header` 原先直接信任输入中的 32-bit `header_size` 并调用 `BytesMut::advance`，声明长度大于已收数据会 panic；`parse_tag` 又用递归跳过 PreviousTagSize、空 payload 和未知 tag，足够长的合法边界输入可造成栈溢出。两处都必须在 fuzz 基线前改成长度检查与迭代消费。

---
