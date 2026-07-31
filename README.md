# ZLMediaKit-RS

[![CI](https://github.com/super-jarvis/zlmediakit-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/ZLMediaKit/zlmediakit-rs/actions/workflows/ci.yml)
[![Docker Publish](https://github.com/super-jarvis/zlmediakit-rs/actions/workflows/docker-publish.yml/badge.svg)](https://github.com/ZLMediaKit/zlmediakit-rs/actions/workflows/docker-publish.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**ZLMediaKit-RS** 是 [ZLMediaKit](https://github.com/super-jarvis/zlmediakit-rs) 的 Rust 语言实现。高性能、多协议流媒体服务器，利用 Rust 的内存安全和高性能异步运行时，提供与 ZLMediaKit 兼容的 HTTP API。

---

## 功能特性

### 协议支持

| 协议 | 推流 | 拉流 | 说明 |
|------|:----:|:----:|------|
| **RTMP(S)** | ✅ | ✅ | RTMP/RTMPS 推拉流 |
| **RTSP** | ✅ | ✅ | TCP interleaved + UDP，RTSPS |
| **HTTP-FLV** | - | ✅ | HTTP 传输 FLV |
| **WebSocket-FLV** | - | ✅ | WebSocket 传输 FLV |
| **HLS (TS)** | - | ✅ | MPEG-TS 分片 + m3u8 |
| **HLS CMAF** | - | ✅ | fMP4 分片 + EXT-X-MAP，与 DASH 共享段 |
| **DASH** | - | ✅ | fMP4/CMAF 分片 + MPD 清单 |
| **WebRTC** | WHIP | WHEP | H.264 + Opus，可选 AAC→Opus 转码 |
| **SRT** | ✅ | - | libsrt 接收，自动发布到 MediaSource |
| **GB28181** | ✅ | - | RTP/PS 推流接收，PS 解复用提取 H.264/H.265 |
| **VOD 点播** | - | ✅ | 录制文件回放，HTTP Range 支持，35 种 MIME 类型 |

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
| 拉流代理 | RTMP/RTSP 远程拉流，自动发布到本地 |
| **推流中继** | 本地流自动推送至远程 RTMP 服务器 |
| FFmpeg 拉流源 | 通过 API 添加任意格式的 ffmpeg 拉流源 |

### 鉴权

| 方式 | 说明 |
|------|------|
| Token 鉴权 | SHA256 URL 签名（`?sign=`） |
| **外部 Hook** | HTTP 回调鉴权（on_publish/on_play/on_stream_not_found） |

### 其他

- **REST API**：流管理、录制控制、代理管理、推流管理、ffmpeg 源管理
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

浏览器访问 `http://localhost:8080/` 查看内置监控仪表盘。

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

### 流管理

| 端点 | 说明 |
|------|------|
| `getMediaList` | 列出所有活跃流 |
| `getMediaInfo` | 查询指定流信息 |
| `getServerConfig` | 获取服务器配置 |
| `getStatistic` | 获取服务器统计 |
| `closeStream` | 关闭指定流 |

### 录制控制

| 端点 | 说明 |
|------|------|
| `startRecord` | 开始录制（`?type=hls/flv/mp4/all`） |
| `stopRecord` | 停止录制 |
| `isRecording` | 查询录制状态 |

### 拉流代理

| 端点 | 说明 |
|------|------|
| `addStreamProxy` | 添加远程拉流代理 |
| `delStreamProxy` | 删除拉流代理 |
| `getStreamProxyList` | 列出拉流代理 |

### 推流中继

| 端点 | 说明 |
|------|------|
| `addStreamPusher` | 将本地流推送至远程 RTMP 服务器 |
| `delStreamPusher` | 停止推流 |
| `getStreamPusherList` | 列出推流任务 |

### FFmpeg 拉流源

| 端点 | 说明 |
|------|------|
| `addFFmpegSource` | 通过 ffmpeg 拉取任意格式的流 |
| `delFFmpegSource` | 停止 ffmpeg 拉流 |
| `getFFmpegSourceList` | 列出 ffmpeg 源 |

---

## 编译说明

### 环境要求

- Rust 工具链（edition 2021）
- 可选：`libsrt-gnutls-dev`（SRT 支持）
- 可选：`ffmpeg`（视频转码运行时）

### 常用命令

```bash
# 标准编译
cargo build --release

# 运行测试
cargo test -- --test-threads=1

# Clippy 检查
cargo clippy

# 格式化
cargo fmt --check
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
