# ZLMediaKit-RS

[![CI](https://github.com/super-jarvis/zlmediakit-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/super-jarvis/zlmediakit-rs/actions/workflows/ci.yml)
[![Docker Publish](https://github.com/super-jarvis/zlmediakit-rs/actions/workflows/docker-publish.yml/badge.svg)](https://github.com/super-jarvis/zlmediakit-rs/actions/workflows/docker-publish.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**ZLMediaKit-RS** 是 [ZLMediaKit](https://github.com/ZLMediaKit/ZLMediaKit) 的 Rust 语言实现。高性能、多协议流媒体服务器，利用 Rust 的内存安全和高性能异步运行时，提供与 ZLMediaKit 兼容的 HTTP API。

---

## 功能特性

### 协议支持

| 协议 | 推流 | 拉流 | 说明 |
|------|:----:|:----:|------|
| **RTMP(S)** | ✅ | ✅ | RTMP/RTMPS 推拉流 |
| **RTSP** | ✅ | ✅ | TCP interleaved + UDP；RTSPS 公共 CA 校验；RECORD 推流支持 Digest |
| **HTTP-FLV** | - | ✅ | HTTP 传输 FLV |
| **WebSocket-FLV** | - | ✅ | WebSocket 传输 FLV |
| **HLS (TS)** | - | ✅ | MPEG-TS 分片 + m3u8 |
| **HLS CMAF** | - | ✅ | fMP4 分片 + EXT-X-MAP，与 DASH 共享段 |
| **DASH** | - | ✅ | fMP4/CMAF 分片 + MPD 清单 |
| **WebRTC** | WHIP | WHEP | H.264 + Opus，可选 AAC→Opus 转码 |
| **SRT** | ✅ | ✅ | Listener/Caller/Rendezvous，MPEG-TS 输入输出，可选延迟、streamid 与 AES passphrase |
| **GB28181** | ✅ | ✅（对讲） | UDP/TCP active/passive RTP 接收；自动识别 PS/TS/ES，支持乱序恢复及 H.264/H.265/AAC/G.711/MP2/MP3；SIP 点播与 G.711A/U 语音对讲 |
| **RTP ES/PS/TS 转推** | ✅ | - | `startSendRtp`/`startSendRtpPassive`/`stopSendRtp`/`listRtpSender`；UDP、TCP active/passive 与断线重连；`type=0/1/2`，默认 GB28181 PS-RTP |
| **原生 HTTP 拉流** | ✅ | ✅ | HTTP/HTTPS-FLV、HTTP/HTTPS-TS、HLS MPEG-TS/CMAF；支持跳转、chunked、主/媒体清单和系统 CA 校验，接入统一拉流代理与协议互转 |
| **VOD 点播** | - | ✅ | 录制文件回放，HTTP Range 支持，35 种 MIME 类型 |

### 编解码与协议互转边界

所有入口协议都发布统一的 `MediaFrame`，并显式标注 FLV、AVCC/HVCC、Annex-B、AAC ASC/Raw/ADTS、Opus 等负载格式；输出协议按目标容器需要做无损转封装。因此，“协议互转”指下表中的兼容编码组合，不代表任意编码器都能无条件全互转。

| 入口/出口 | 可直接承载的主要编码 | 说明 |
|------|------|------|
| RTMP/RTMPS | H.264、H.265；AAC、MP3、G.711A/U | 使用 FLV/RTMP 负载；不支持的 classic FLV 编码会明确返回错误 |
| RTSP/RTSPS | H.264、H.265；AAC、Opus、PCMA、PCMU、MP3/MPA、L16 | SDP、RTP payload type 与 clock rate 按真实音频轨生成 |
| HTTP-FLV / WebSocket-FLV | H.264、H.265；AAC、MP3、G.711A/U | Opus、L16 不能直接写入 classic FLV |
| HLS MPEG-TS | H.264、H.265；AAC | 视频统一转 Annex-B，AAC 统一转 ADTS |
| HLS CMAF / DASH / MP4 / fMP4 | H.264、H.265；AAC | 视频写长度前缀样本，AAC 写 raw access unit；CMAF/DASH 共享 fMP4 段 |
| WebRTC WHIP/WHEP | H.264、Opus | WHEP 可通过 `aac-transcode` feature 把 AAC 转为 Opus；不声明 H.265 WebRTC 支持 |
| SRT MPEG-TS 输入/输出 | H.264、H.265；AAC/ADTS | Listener/Caller/Rendezvous；从 PAT/PMT、PES 读取轨道及 PTS/DTS，输出按 7 个 TS packet 分组发送 |
| GB28181 输入 | PS：H.264/H.265/AAC/G.711/MP2/MP3；TS：H.264/H.265/AAC；ES：H.264/H.265/AAC/G.711 | RTP 跨包重排/去重；自动识别 PS/TS/ES，PSM 决定轨道编码，AAC 支持 ADTS/RFC3640 |

转码能力目前限定为显式配置的 H.264 ↔ H.265，以及可选的 AAC → Opus。其他组合如果目标协议不能承载，会拒绝或不发布该轨道，不会原样写入伪合法容器。仓库中的 `protocol_conversion_matrix` 测试会由 ffmpeg 生成真实 H.264/AAC FLV，再验证 FLV、MPEG-TS、MP4、fMP4 输出可被 ffprobe 识别并由 ffmpeg 完整解码。

### 录制

| 格式 | 说明 |
|------|------|
| FLV | 实时录制 |
| HLS (TS) | 分片录制 |
| MP4 | 完整文件（流结束后写入） |
| **fMP4** | 分片录制（init.mp4 + seg-N.m4s），适合 DASH/HLS CMAF |

### 视频转码

通过 ffmpeg 子进程实现实时视频转码：

| 能力 | 说明 |
|------|------|
| H.264 ↔ H.265 | 编解码互转 |
| 分辨率缩放 | `width`/`height` 任意缩放 |
| 码率控制 | `bitrate` 参数控制 |

### 集群与代理

| 功能 | 说明 |
|------|------|
| 拉流代理 | RTMP(S)、RTSP(S)、SRT Caller/Rendezvous、HTTP(S)-FLV、HTTP(S)-TS、HLS TS/CMAF 远程拉流，自动发布到本地 |
| **推流中继** | 本地流自动推送至远程 RTMP(S)、RTSP(S) 或 SRT Caller/Rendezvous 端点 |
| FFmpeg 拉流源 | 通过 API 添加任意格式的 ffmpeg 拉流源 |

### 鉴权

| 方式 | 说明 |
|------|------|
| Token 鉴权 | SHA256 URL 签名（`?sign=`） |
| **外部 Hook** | HTTP 回调鉴权（on_publish/on_play/on_stream_not_found） |

### 其他

- **REST API**：流管理、录制控制、代理管理、推流管理、RTP ES/PS/TS 转推、ffmpeg 源管理
- **Web 管理面板**：内置监控仪表盘
- **静态文件服务**：www_root 提供 HTML/CSS/JS 等静态文件
- **TOML 配置**：丰富的配置项，支持命令行覆盖
- **Docker 支持**：多阶段构建镜像 + docker-compose 一键部署

---

## 快速开始

### Docker（推荐）

```bash
# 拉取镜像
docker pull ghcr.io/super-jarvis/zlmediakit-rs:latest

# 或本地构建
docker build -t zlmediakit-rs .

# 一键部署
docker compose up -d

# 查看日志
docker compose logs -f
```

### 编译运行

```bash
# 编译
cargo build --release

# 运行（默认端口：RTMP 1935, RTSP 8554, HTTP 8080, API 8081, WebRTC 9000）
./target/release/zlmediakit

# 指定配置
./target/release/zlmediakit --config /path/to/config.toml

# 调试模式
RUST_LOG=debug ./target/release/zlmediakit
```

### 推流

```bash
# RTMP
ffmpeg -re -i input.mp4 -c copy -f flv rtmp://localhost:1935/live/stream

# RTSP
ffmpeg -re -i input.mp4 -c copy -f rtsp rtsp://localhost:8554/live/stream

# SRT（需启用 srt 配置段）
ffmpeg -re -i input.mp4 -c copy -f mpegts "srt://localhost:9000?streamid=live/stream"
```

### 播放

```bash
# RTMP
ffplay rtmp://localhost:1935/live/stream

# RTSP
ffplay rtsp://localhost:8554/live/stream

# HTTP-FLV
ffplay http://localhost:8080/live/stream.flv

# HLS (TS)
ffplay http://localhost:8080/live/stream/hls.m3u8

# HLS CMAF (fMP4)
ffplay http://localhost:8080/live/stream.cmav.m3u8

# DASH
ffplay http://localhost:8080/live/stream.mpd
```

### 管理面板

浏览器访问 `http://localhost:8080/` 打开管理后台，使用配置文件顶层的
`secret` 登录。管理 API 默认接受 `X-API-Secret` 请求头，也兼容
`Authorization: Bearer <secret>` 和 ZLMediaKit 风格的 `?secret=` 查询参数；
配置为空时可显式关闭管理鉴权。`getServerConfig` 只返回 secret 是否已配置，
不会回显明文。

后台已接入流与播放者详情、截图、连接会话、拉流代理、推流转发、FFmpeg
源、HLS/FLV/MP4 录制与归档、GB28181 设备/目录/点播、RTP 服务、实时转码、
运行时配置、线程负载和协议地址。浏览器播放器支持 HLS、HTTP/WS-FLV 和
WebRTC WHEP；SRT 提供推流地址与运行状态。登录 secret 仅保存在当前标签页的
`sessionStorage` 中，退出登录会立即清除。

---

## 架构

```
┌──────────────────────────────────────────────────────────────┐
│                    zlmediakit 二进制                          │
│  ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐ ┌─────┐ ┌────┐ ┌────┐  │
│  │ RTMP │ │ RTSP │ │ HTTP │ │WebRTC│ │ SRT │ │GB  │ │DASH│  │
│  │ 服务 │ │ 服务 │ │ 服务 │ │ 服务 │ │服务 │ │2818│ │    │  │
│  └──┬───┘ └──┬───┘ └──┬───┘ └──┬───┘ └──┬──┘ └──┬─┘ └──┬─┘  │
│     │        │        │        │        │       │       │     │
│     └────────┴────────┴────────┴────────┴───────┴───────┘     │
│                              │                                │
│                     MediaSource 总线                           │
│                (GopCache + broadcast::channel)                 │
│                              │                                │
└──────────────────────────────────────────────────────────────┘
```

### Crate 结构（10 crates）

| Crate | 路径 | 职责 |
|-------|------|------|
| `core` | `crates/core` | 基础框架：MediaSource、GopCache、EventBus、鉴权、Hook、配置、录制控制、流代理、推流控制 |
| `codec` | `crates/codec` | 编解码器解析：H.264/H.265/AAC/G.711/PS |
| `rtmp` | `crates/rtmp` | RTMP 协议：握手、分块流、AMF0、推拉流客户端 |
| `rtsp` | `crates/rtsp` | RTSP 协议：TCP/UDP 传输、RTP 封包/解包 |
| `http` | `crates/http` | HTTP 服务：FLV/HLS/DASH/VOD/API/WebSocket/静态文件 |
| `flv` | `crates/flv` | FLV 封装/解封装、录制器 |
| `hls` | `crates/hls` | HLS 封装（TS + CMAF fMP4）、分片、录制器 |
| `mp4` | `crates/mp4` | MP4 封装、fMP4 分片、DASH MPD、录制器 |
| `srt` | `crates/srt` | SRT 接收（libsrt FFI）、GB28181 RTP/PS 接收 |
| `transcode` | `crates/transcode` | 视频转码（ffmpeg 子进程）：H.264↔H.265、缩放、码率控制 |
| `webrtc` | `crates/webrtc` | WebRTC WHEP/WHIP、音频转码 |
| `server` | `crates/server` | 二进制入口：配置加载、服务编排、监控协程 |

---

## 配置

配置文件 `conf/config.toml`（TOML 格式），完整注释见文件。主要配置项：

| 配置段 | 说明 |
|--------|------|
| **全局** | `auth_enabled`、`secret`、各协议端口 |
| `[general]` | 流量阈值、无播放者延时 |
| `[rtmp]` / `[rtsp]` / `[http]` | 协议开关、端口、SSL |
| `[webrtc]` | WebRTC WHEP 端口、ICE 服务器 |
| `[hook]` | 外部鉴权回调 URL（可选） |
| `[srt]` | SRT 接收端口、延迟、加密（可选） |
| `[cluster]` | 集群推流中继目标列表（可选） |
| `[proxy]` | 启动时自动拉流列表（可选） |
| `[record]` | 录制开关（hls/mp4/flv）、存储路径 |

### 命令行参数

| 参数 | 说明 | 默认值 |
|------|------|--------|
| `--config` | 配置文件路径 | `conf/config.toml` |
| `--rtmp-port` | RTMP 端口 | 1935 |
| `--rtsp-port` | RTSP 端口 | 8554 |
| `--http-port` | HTTP 端口 | 8080 |
| `-a, --api-port` | API 端口 | 8081 |
| `--webrtc-port` | WebRTC 端口 | 9000 |
| `--log-level` | 日志级别 | `info` |

---

## REST API

所有 API 端点位于 `/index/api/`：

```bash
# 推荐：请求头传递 secret，避免写入 URL 和常规访问日志
curl -H 'X-API-Secret: <配置文件中的 secret>' \
  http://localhost:8080/index/api/getMediaList

# 兼容 ZLMediaKit 调用方式
curl 'http://localhost:8080/index/api/getMediaList?secret=<secret>'
```

### 流管理

| 端点 | 说明 |
|------|------|
| `getMediaList` | 列出所有活跃流（支持 `vhost`/`app`/`stream` 过滤） |
| `getMediaInfo` | 查询指定流信息 |
| `getMediaPlayerList` | 列出指定流的播放者 |
| `isMediaOnline` | 查询流是否在线（`?stream=`） |
| `getServerConfig` | 获取服务器运行时配置 |
| `setServerConfig` | 修改运行时配置（如 `?general.flowThreshold=2048`） |
| `getStatistic` | 获取服务器统计 |
| `closeStream` | 关闭指定流 |
| `close_streams` | 按 `vhost`/`app`/`stream` 批量关闭流 |
| `getMp4RecordFile` | 列出已录制的 MP4 文件（`?stream=&period=YYYYMMDD`） |

### 会话管理

| 端点 | 说明 |
|------|------|
| `getAllSession` | 列出所有会话（支持 `peer_ip`/`local_port` 过滤） |
| `kick_session` | 按 id 或 vhost/app/stream 踢掉单个会话 |
| `kick_sessions` | 按 `peer_ip`/`local_port`/`typeid` 批量踢掉会话 |

### 系统信息

| 端点 | 说明 |
|------|------|
| `getApiList` | 列出所有可用 API 路径 |
| `version` | 版本信息 |
| `getThreadsLoad` | 线程负载 |
| `getWorkThreadsLoad` | 工作线程负载 |

### 录制控制

| 端点 | 说明 |
|------|------|
| `startRecord` | 开始录制（`?type=hls/flv/mp4/all`） |
| `stopRecord` | 停止录制 |
| `isRecording` | 查询录制状态 |
| `getRecordStatus` | 查询录制状态（`?stream=`） |

### 拉流代理

| 端点 | 说明 |
|------|------|
| `addStreamProxy` | 添加远程拉流代理；原生支持 RTMP(S)、RTSP(S)、SRT Caller/Rendezvous、HTTP(S)-FLV、HTTP(S)-TS 和 HLS TS/CMAF |
| `delStreamProxy` | 删除拉流代理 |
| `getStreamProxyList` | 列出拉流代理 |

### 推流中继

| 端点 | 说明 |
|------|------|
| `addStreamPusher` | 将本地流推送至远程 RTMP(S)、RTSP(S) 或 SRT 端点；RTSP 使用 ANNOUNCE/SETUP/RECORD、Digest 与 TCP interleaved RTP，SRT 使用 MPEG-TS Caller/Rendezvous |
| `delStreamPusher` | 停止推流 |
| `getStreamPusherList` | 列出推流任务 |

### FFmpeg 拉流源

| 端点 | 说明 |
|------|------|
| `addFFmpegSource` | 通过 ffmpeg 拉取任意格式的流 |
| `delFFmpegSource` | 停止 ffmpeg 拉流 |
| `getFFmpegSourceList` | 列出 ffmpeg 源 |

### RTP / GB28181

| 端点 | 说明 |
|------|------|
| `openRtpServer` | 开启 RTP 收流端口；`tcp_mode=0/1/2` 表示 UDP/TCP passive/TCP active，默认自动识别 PS/TS/ES |
| `connectRtpServer` | 为 `tcp_mode=2` 的收流端口设置主动连接目标（`stream_id`、`dst_url`、`dst_port`） |
| `closeRtpServer` | 关闭 RTP 收流端口 |
| `listRtpServer` | 列出 RTP 收流端口 |
| `getRtpInfo` | 查询 RTP 收流信息 |
| `startRtp` | 邀请 GB28181 设备推流（`?device_id=&channel_id=`） |
| `stopRtp` | 停止 GB28181 推流（同时向设备发送 SIP BYE） |
| `startTalk` | 发起 GB28181 语音对讲；指定设备/通道和本机已发布的 G.711A/U 音源（`device_id`、`channel_id`、`vhost`、`app`、`stream`） |
| `stopTalk` | 停止指定通道的语音对讲并发送 SIP BYE（`?channel_id=`） |
| `getTalkList` | 查询活动对讲及音源、编码、SSRC、本地 RTP 端口 |
| `getDeviceList` | 列出已注册的 GB28181 设备（在线状态、地址、通道、静态信息），支持 `?device_id=` 过滤 |
| `getDeviceInfo` | 查询单个设备快照（`?device_id=`） |
| `queryCatalog` | 向设备发起 Catalog 查询并返回通道列表（`?device_id=`） |
| `queryDeviceInfo` | 向设备发起 DeviceInfo 查询（`?device_id=`） |
| `getSipInfo` | 查询 SIP 服务器信息（端口、realm、设备数、活跃流数） |
| `stopSip` | 停止 SIP 服务器（清空设备、关闭流、退出接收循环） |

---

## 编译说明

### 环境要求

- Rust 工具链（edition 2021）
- Linux 构建：`pkg-config`、`libsrt-gnutls-dev`
- 测试与视频转码：`ffmpeg`/`ffprobe`
- 可选 WebRTC AAC → Opus：`libopus-dev`、`libfdk-aac-dev`，并启用 `zlmediakit-webrtc/aac-transcode`

### 常用命令

```bash
# 标准编译
cargo build --release

# 运行工作区测试（包含真实网络 E2E 与 ffmpeg/ffprobe 转换矩阵）
cargo test --workspace --tests

# 验证可选 AAC → Opus 路径
cargo test -p zlmediakit-webrtc --features aac-transcode

# Clippy 检查（CI 标准）
cargo clippy --workspace --all-targets -- -D warnings

# 格式化
cargo fmt --all --check
```

---

## Docker

### 镜像地址

```
ghcr.io/super-jarvis/zlmediakit-rs:latest
```

### 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `ZLM_RTMP_PORT` | `1935` | RTMP 端口 |
| `ZLM_RTSP_PORT` | `8554` | RTSP 端口 |
| `ZLM_HTTP_PORT` | `8080` | HTTP 端口 |
| `ZLM_API_PORT` | `8081` | API 端口 |
| `ZLM_WEBRTC_PORT` | `9000` | WebRTC 端口 |
| `RUST_LOG` | - | 日志级别 |

### 部署

```bash
# docker-compose（推荐）
docker compose up -d

# docker run
docker run --network host \
  -v $(pwd)/conf:/etc/zlmediakit:ro \
  -v $(pwd)/record:/var/lib/zlmediakit/record \
  ghcr.io/<用户>/zlmediakit-rs:latest
```

---

## 项目结构

```
zlmediakit-rs/
├── conf/
│   └── config.toml              # 配置文件
├── crates/
│   ├── core/                    # 核心框架
│   ├── codec/                   # 编解码器
│   ├── rtmp/                    # RTMP 协议
│   ├── rtsp/                    # RTSP 协议
│   ├── http/                    # HTTP 服务
│   ├── flv/                     # FLV 封装
│   ├── hls/                     # HLS 封装
│   ├── mp4/                     # MP4/fMP4/DASH
│   ├── srt/                     # SRT + GB28181
│   ├── transcode/               # 视频转码
│   ├── webrtc/                  # WebRTC
│   └── server/                  # 入口程序
├── Dockerfile                   # Docker 镜像构建
├── docker-compose.yml           # Docker Compose 部署
├── .github/workflows/           # CI/CD（编译 + Docker 发布）
└── test/                        # 外部测试脚本
```

---

## 许可证

MIT License
