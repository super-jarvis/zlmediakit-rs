# ZLMediaKit-RS

[![CI](https://github.com/ZLMediaKit/zlmediakit-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/ZLMediaKit/zlmediakit-rs/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**ZLMediaKit-RS** 是使用 Rust 语言实现的高性能、多协议流媒体服务器。作为 [ZLMediaKit](https://github.com/ZLMediaKit/ZLMediaKit)（C++）的重新实现，它保持了相同的设计理念与 HTTP API 兼容性，同时利用 Rust 的内存安全特性和高性能异步运行时。

---

## 功能特性

### 支持协议

| 协议 | 状态 | 说明 |
|------|------|------|
| **RTMP** | ✅ 稳定 | 推流与拉流，支持 RTMPS（TLS） |
| **RTSP** | ✅ 稳定 | TCP/UDP interleaved 模式，支持 RTSPS（TLS） |
| **HTTP-FLV** | ✅ 稳定 | 通过 HTTP 播放 FLV 流 |
| **HLS** | 🚧 完善中 | MPEG-TS 分片 + m3u8 播放列表 |
| **WebSocket-FLV** | ✅ 实现 | 通过 WebSocket 传输 FLV |
| **WebRTC** | ✅ 基础可用 | WHEP（拉流）/ WHIP（推流），H.264 + Opus |
| **MP4 录制** | ✅ 实现 | 内存缓冲式录制，停止时写入完整 MP4 |
| **FLV 录制** | ✅ 实现 | 实时 FLV 录制 |
| **HLS 录制** | ✅ 实现 | HLS 分片录制（.ts + .m3u8） |
| **流代理** | ✅ 实现 | RTMP/RTSP 远程拉流代理，支持自动拉流配置 |
| **点播 VOD** | ✅ 实现 | 录制文件回放，支持 HTTP Range 请求 |

### 核心设计

所有协议共享统一的 **MediaSource** 抽象：推流端通过 `publish_and_cache()` 推送 `MediaFrame`，拉流端通过 `subscribe()` 接收相同帧，无需关心对方使用什么协议。

### 其他特性

- **HTTP 钩子（Hook）**：推流/拉流鉴权，支持自定义外部 HTTP 回调，与 ZLMediaKit 协议兼容
- **Token 鉴权**：基于 SHA256 的 URL 签名鉴权
- **REST API**：流管理、录制控制、代理管理等
- **Web 管理页面**：内置监控面板
- **配置文件**：TOML 格式，支持命令行参数覆盖

---

## 快速开始

### 编译

```bash
cargo build --release
```

编译产物位于 `./target/release/zlmediakit`。

### 运行

```bash
./target/release/zlmediakit
```

默认端口：RTMP `1935`、RTSP `8554`、HTTP `8080`、API `8081`、WebRTC `9000`。

指定配置文件：
```bash
./target/release/zlmediakit --config /path/to/config.toml
```

调试模式：
```bash
./target/release/zlmediakit --log-level debug
```

### 推流测试

```bash
# RTMP 推流
ffmpeg -re -i input.mp4 -c copy -f flv rtmp://localhost:1935/live/stream

# RTSP 推流
ffmpeg -re -i input.mp4 -c copy -f rtsp rtsp://localhost:8554/live/stream
```

### 拉流测试

```bash
# RTMP 拉流
ffplay rtmp://localhost:1935/live/stream

# RTSP 拉流
ffplay rtsp://localhost:8554/live/stream

# HTTP-FLV 拉流
ffplay http://localhost:8080/live/stream.flv

# HLS 拉流（浏览器或 ffplay）
ffplay http://localhost:8080/live/stream/hls.m3u8
```

---

## 架构

```
┌─────────────────────────────────────────────────────────┐
│                   zlmediakit 二进制                      │
│  ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌──────┐  │
│  │ RTMP   │ │ RTSP   │ │ HTTP   │ │ WebRTC │ │ 其他  │  │
│  │ 服务   │ │ 服务   │ │ 服务   │ │ 服务   │ │ 模块  │  │
│  └───┬────┘ └───┬────┘ └───┬────┘ └───┬────┘ └──┬───┘  │
│      │          │          │          │          │      │
│      └──────────┴──────────┴──────────┴──────────┘      │
│                         │                               │
│                  MediaSource 总线                        │
│              (GopCache + broadcast::channel)             │
│                         │                               │
└─────────────────────────────────────────────────────────┘
```

项目采用 Cargo Workspace 组织，包含 11 个 crate：

| Crate | 路径 | 职责 |
|-------|------|------|
| `core` | `crates/core` | 基础框架：MediaSource 总线、GopCache、EventBus、鉴权、钩子、配置、录制控制、流代理 |
| `codec` | `crates/codec` | 编解码器解析：H.264、H.265、AAC、G.711 |
| `rtmp` | `crates/rtmp` | RTMP 协议实现：握手、分块流、AMF0 编解码 |
| `rtsp` | `crates/rtsp` | RTSP 协议实现：TCP/UDP 传输 |
| `http` | `crates/http` | HTTP 服务：FLV 流、HLS 服务、REST API、VOD、WebSocket-FLV |
| `flv` | `crates/flv` | FLV 封装/解封装、录制器 |
| `hls` | `crates/hls` | HLS 封装（MPEG-TS 写入）、分片、录制器 |
| `mp4` | `crates/mp4` | MP4 封装器（ISO Base Media File Format） |
| `webrtc` | `crates/webrtc` | WebRTC 支持：WHEP 拉流、WHIP 推流 |
| `transcode` | `crates/transcode` | 音频转码（AAC ↔ Opus，可选特性） |
| `server` | `crates/server` | 二进制入口：配置加载、服务启动、监控协程 |

---

## 配置

配置文件位于 `conf/config.toml`，TOML 格式。主要配置项：

```toml
# 全局端口
rtmp_port = 1935
rtsp_port = 8554
http_port = 8080
api_port = 8081
webrtc_port = 9000

# 鉴权（false = 关闭，与 ZLMediaKit 签名算法兼容）
auth_enabled = false
secret = "your-secret-key"

[rtmp]
enabled = true
ssl = false
# ssl_cert = "/path/to/cert.pem"
# ssl_key  = "/path/to/key.pem"
# ssl_port = 443

[rtsp]
enabled = true
tcp_mode = true
ssl = false

[http]
enabled = true
dir_root = true
www_root = "./www"

[webrtc]
enabled = true
# ice_servers = ["stun:stun.l.google.com:19302"]
#
# WebRTC 支持：
# - WHEP（WebRTC-HTTP Egress Protocol）拉流：浏览器通过 WHEP 播放已发布的流
# - WHIP（WebRTC-HTTP Ingress Protocol）推流：浏览器通过 WHIP 推流到服务器
# - 编解码：H.264 + Opus（原生支持），AAC 需启用 `transcode` feature
# - `transcode` feature 依赖系统安装 `ffmpeg`/`fdk-aac`；启用后自动将 AAC 转码为 Opus

[record]
app = "record"
path = "./record"
hls = false
mp4 = false
flv = false

[proxy]
enabled = false
# pulls = [
#   { url = "rtmp://example.com/live/stream", vhost = "__defaultVhost__", app = "proxy", stream = "test" }
# ]

[hook]
# on_publish = "http://127.0.0.1:3000/hook/on_publish"
# on_play = "http://127.0.0.1:3000/hook/on_play"
timeout_sec = 5
retry = 1
```

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

| 端点 | 说明 |
|------|------|
| `getMediaList` | 列出所有活跃流 |
| `getMediaInfo` | 查询指定流信息 |
| `getServerConfig` | 获取服务器配置 |
| `getStatistic` | 获取服务器统计 |
| `closeStream` | 关闭指定流 |
| `startRecord` | 开始录制（`?type=hls/flv/mp4/all`） |
| `stopRecord` | 停止录制 |
| `isRecording` | 查询录制状态 |
| `addStreamProxy` | 添加远程拉流代理 |
| `delStreamProxy` | 删除拉流代理 |
| `getStreamProxyList` | 列出拉流代理 |

---

## 编译说明

### 环境要求

- Rust 工具链（edition 2021）
- 可选：libopus + libfdk-aac（启用 `transcode` 特性时）

### 编译命令

```bash
# 标准 Release 编译
cargo build --release

# 启用转码特性（需要系统库 libopus + libfdk-aac-dev）
cargo build --release --features transcode

# 运行单元测试
cargo test

# 运行集成测试
cargo test --tests
```

### 系统依赖（转码特性）

Ubuntu/Debian：
```bash
sudo apt install libopus-dev libfdk-aac-dev
```

macOS：
```bash
brew install opus fdk-aac
```

---

## 开发指南

### 项目结构

```
crates/
├── core/         # 核心抽象层 - 不依赖协议
├── codec/        # 编解码器解析 - 纯函数
├── rtmp/         # RTMP 协议
├── rtsp/         # RTSP 协议
├── http/         # HTTP 服务
├── flv/          # FLV 封装
├── hls/          # HLS 封装
├── mp4/          # MP4 封装
├── webrtc/       # WebRTC 支持
├── transcode/    # 音频转码
└── server/       # 可执行程序
```

### 添加新协议

1. 在 `crates/` 下创建新 crate
2. 实现推流/拉流逻辑，通过 `MediaSource` 发布和订阅帧
3. 在 `server/src/main.rs` 中注册服务

---

## 许可证

MIT License
