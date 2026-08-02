# 项目发现

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
