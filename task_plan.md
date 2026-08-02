# 任务计划：完善前端管理后台与 API Secret 鉴权

## 当前目标（2026-08-02）

参考 ZLMediaKit 的管理控制台能力，完整接入本项目已有 HTTP API，覆盖流、会话、录像、拉流代理、推流转发、FFmpeg、GB28181/RTP、转码、服务器配置与运行监控；使用配置文件中的 `secret` 对管理 API 进行统一鉴权，并补齐前端可靠性、安全性、自动化测试和 WSL 全量验证。

## 当前验收边界

- 除明确列为内部/危险的接口外，`getApiList` 所列管理能力均有可发现、可操作的前端入口。
- `/index/api/*` 默认校验配置文件中的 `secret`；错误、缺失口令返回明确的 HTTP/JSON 错误，前端提供登录、退出、会话保存与失效处理。
- `getServerConfig` 不向前端回显明文 secret；修改配置、重启、关闭流、踢会话等危险操作有确认与错误反馈。
- 页面覆盖桌面与移动端，HTTPS 下正确使用 WSS；WebRTC/SRT 等浏览器不能原生直接承载的能力提供正确入口或状态说明。
- 为鉴权、API 客户端和关键页面交互增加自动化测试；最终在 WSL 通过 fmt、Clippy、workspace tests 与 release build。

## 当前阶段

1. [completed] 审计 API、配置加载、HTTP 请求解析和前端结构，固化鉴权契约与页面信息架构
2. [completed] 实现后端 API secret 守卫、敏感配置脱敏、鉴权测试
3. [completed] 实现前端登录态、统一 API 错误处理、HTTPS/WSS 和基础组件
4. [completed] 完善流详情、会话、截图、录像文件、服务器配置与监控
5. [completed] 实现 GB28181/RTP、转码以及剩余协议管理入口
6. [completed] 完成交互、安全、响应式、WebRTC/SRT 能力说明与前端自动化测试
7. [completed] WSL 全量质量门禁、文档与最终能力矩阵验收

## 当前实施决策

- secret 由后端从 `ServerConfig.secret` 注入 HTTP 服务并统一校验；前端不硬编码默认 secret。
- 前端优先使用请求头传递 secret，兼容 ZLMediaKit 风格的 `secret` 查询参数，避免在正常页面 URL 中暴露口令。
- 登录态默认使用 `sessionStorage`，不把 secret 持久保存到磁盘；用户可主动退出清除。
- 不引入大型前端框架，继续沿用当前无构建依赖的静态 HTML/CSS/JavaScript 架构，除非实现测试时证明需要最小开发依赖。
- 保留已完成的协议实现及用户现有工作区内容，不撤销无关改动。

## 当前错误记录

| 错误 | 尝试次数 | 解决方案 |
|---|---:|---|
| Windows PATH 中 `node` 不可用，无法直接执行 `node --check` | 1 | 后续通过 WSL Node 或浏览器级测试验证前端 |
| HTTP API E2E 并行运行偶发 `Address already in use` | 1 | 将测试服务器由共享固定端口改为每用例动态端口 |
| WSL 未安装 Node，无法运行 `node --check` | 1 | 改用 Codex 工作区捆绑 Node 运行时和浏览器测试 |
| 首次联调按 crate 名猜测二进制为 `zlmediakit-server`，启动后立即退出 | 1 | 通过产物清单确认实际二进制名为 `target/debug/zlmediakit`，后续使用正确入口 |
| 使用 WSL `--cd` 直接执行 cargo 时登录环境未加载，提示 `cargo: command not found` | 1 | 后续固定通过 `bash -lc 'cargo ...'` 加载 Rust 工具链 |
| 最终 `cargo fmt --all --check` 报告新增测试一处换行差异 | 1 | 执行 `cargo fmt --all` 后复验通过 |

---

## 已完成历史目标：补齐协议实现与协议互转

## 目标

完成仓库已声明的 RTMP/RTMPS、RTSP/RTSPS、HTTP/WS-FLV、HLS-TS、HLS-CMAF、DASH、WebRTC WHIP/WHEP、SRT、GB28181、MP4/fMP4 关键收发与互转能力。统一并显式标注内部媒体负载格式，消除入口与出口对 FLV、AVCC/HVCC、Annex-B、ADTS、Raw 音频格式的隐式假设；建立真实网络入口到真实网络出口的自动化转换矩阵，并用 ffmpeg/ffprobe 验证输出可解码。

## 验收边界

- “完成”指仓库 README 已声明的协议方向和已声明编解码器组合能够工作并有自动化验证，不宣称实现各标准的全部可选扩展。
- 视频基线：H.264、H.265；音频基线：AAC、Opus，RTMP/RTSP 已声明的 G.711/MP3/L16 路径必须明确支持或返回不支持，不能静默伪装为 AAC/H.264。
- 所有输出容器必须通过 ffprobe；关键实时链路必须用真实客户端完成推流、拉流或播放验证。
- 保留用户现有工作区改动，不擅自恢复已移动到 `.github/workflows_bak/` 的工作流。

## 阶段

1. [completed] 固化协议/编解码器/方向/测试现状矩阵，建立失败用例和格式契约
2. [completed] 为 `MediaFrame` 引入明确的负载格式，集中实现 FLV、AVCC/HVCC、Annex-B、ADTS、Raw 音频转换工具
3. [completed] 修复入口规范化：RTMP、RTSP、WHIP、SRT、GB28181、MP4/VOD
4. [completed] 修复输出封装：RTMP、RTSP、HTTP/WS-FLV、HLS-TS、MP4/fMP4、CMAF、DASH、WHEP
5. [completed] 补齐协议能力：SRT 时间戳/TS 解复用、RTSP 音频 SDP/负载、WebRTC 默认音频策略及错误处理
6. [completed] 建立“入口 × 出口 × 编解码器”端到端测试矩阵，并加入 ffmpeg/ffprobe 有效性断言
7. [completed] 在 WSL 完成 fmt、Clippy、workspace tests、release build 和真实媒体互操作测试
8. [completed] 更新 README 支持矩阵、限制说明、配置和 CI 测试入口

## 实施决策

- 不继续依赖 `MediaFrame.data` 的隐式 FLV 语义；先增加可查询的负载格式，再渐进迁移各协议，避免一次性破坏所有调用方。
- 转封装优先无损转换，不把转码混入协议互转；仅在目标协议无法承载源编解码器时使用显式、可配置的转码。
- 不支持的组合必须返回明确错误或不在 SDP/元数据中宣告，禁止把未知视频默认当 H.264、把未知音频默认当 AAC。
- 每修复一条转换路径，先增加失败测试，再修改实现，并使用真实容器/播放器验证。
- 代码发现优先使用已建立的 `codebase-memory-mcp` 图谱；字符串、配置和测试数据检索使用 `rg`。
- shell 命令统一通过 `rtk` 执行；编译与媒体工具验证使用 WSL Ubuntu-26.04。

## 遇到的错误

| 错误 | 尝试次数 | 解决方案 |
|---|---:|---|
| codebase-memory-mcp 初次返回项目未索引 | 1 | 已使用 fast 模式建立 `zlmediakit-rs` 图谱 |
| Windows `rtk cat`/`rtk Get-Content` 不能直接执行 PowerShell 内建命令 | 2 | 使用 `rtk proxy powershell.exe -NoProfile -Command ...` |
| 规划技能 `session-catchup.py` 无法运行，Windows PATH 没有 `python` | 1 | 已通过完整读取三个规划文件和 `git diff --stat/status` 手动恢复；后续需要脚本时使用 WSL 或工作区依赖 Python |
| WSL 在沙箱内首次访问返回 E_ACCESSDENIED | 1 | 按环境规则使用已批准的 WSL 调用 |
| 新增 Annex-B 往返测试失败：三字节起始码的 `0x01` 被包含在前一 NAL | 1 | 将扫描结果改为同时记录 marker 起点与 NAL 起点，NAL 终点使用下一 marker 起点 |
| 修复边界后测试仍按字节比较三字节与四字节 Annex-B 起始码 | 1 | 测试改为断言合法的四字节规范化输出；NAL 内容保持不变 |
| 首次 `cargo fmt --all --check` 报告新增模块格式差异 | 1 | 使用 `cargo fmt --all` 应用机械格式化，随后以 workspace check 验证 |
| MP4 测试仍断言 `SampleEntry.data` 保留 FLV 视频头 | 1 | 更新旧测试契约，只断言剥离后的 MP4 样本内容和顺序 |
| MP4 recorder 单独运行仍未生成文件 | 1 | 定位为 recorder 启动只回放配置、不回放 GOP，且旧配置帧可能只由字节识别；补充 GOP 回放和兼容性配置识别 |

## 已知非本目标改动

- `.github/workflows/ci.yml`、`.github/workflows/release.yml` 当前被用户移动/删除，`.github/workflows_bak/` 为未跟踪目录。
- `.github/workflows/docker-publish.yml` 含此前的 `actions/checkout@v6` 更新。
- 协议实现过程中不修改或撤销上述改动，除非后续阶段明确更新 CI 测试入口。
# 当前目标：对齐 ZLMediaKit 开源版核心能力（2026-08-02）

## 目标边界

在保留现有协议互转、管理前端和 API Secret 鉴权成果的基础上，按依赖顺序补齐 ZLMediaKit 开源版核心能力。闭源专业版的 JT1078、GPU/任意转码、S3、AI、RTC 集群代理和 MCU 不纳入本目标。

## 验收原则

- 每项能力必须形成真实网络入口到真实网络出口的闭环，并有单元测试或端到端测试。
- 优先保持 ZLMediaKit REST API、参数和返回语义兼容；无法兼容时必须文档化差异。
- 不以类型、枚举或未接线的解析器作为“已实现”依据。
- 每个阶段完成后执行针对性测试；里程碑完成后在 WSL 执行 fmt、check、clippy、workspace tests 和 release build。
- 保留用户现有未提交改动，不回滚 `.github/workflows_bak/` 或前端/鉴权成果。

## 实施阶段

1. [completed] 通用 RTP 基线：建立 RTP sender 管理器，实现 ZLM 兼容 `startSendRtp`/`stopSendRtp` API、UDP 主动发送、SSRC/PT/时间戳/序列号和端到端测试。
2. [completed] 通用 RTP 完整传输：TCP active/passive、PS/TS/ES 输出、端口复用、同 SSRC 多目标、重连与统计。
3. [completed] GB28181 完整化：TCP RTP 接收、PS/TS/ES 自动识别、音频轨、乱序/去重、RTP 转推和双向语音对讲。
4. [completed] 原生拉流客户端：HLS TS/fMP4、HTTP-FLV、HTTP-TS，并接入代理、按需启停和协议互转。
5. [completed] 双向协议与点播：RTSP 主动推流、SRT Caller/输出/Rendezvous、MP4 经 RTSP/RTMP/HTTP/WS-FLV 点播和 seek。
6. [completed] 集群与生命周期：多源站轮询、HLS/HTTP-TS 溯源、无人观看关闭、按需转协议、先播后推、断连续推。
7. [in_progress] WebRTC 与多轨：simulcast、RTX/REMB、单端口/迁移/TCP、ice-full 客户端、多音视频轨和扩展编码。
8. [completed] 运维与平台：补齐 Hook/API 响应语义、真实配置/TLS 热更新、IPv6、跨平台 CI 和可嵌入 SDK 接口。
9. [in_progress] 商用验证：兼容性矩阵、故障恢复、长稳、压力、模糊测试、性能基线和完整文档。

## 当前阶段设计决策

- 第一批先做 H.264/H.265/AAC `MediaFrame` 到 RTP 的无转码发送；封装层复用现有 payload 规范化工具。
- `startSendRtp` 与当前用于 SIP INVITE 的 `startRtp` 分开，避免 API 名称相似但语义混淆。
- sender 生命周期由独立管理器持有，API 只负责创建、停止和查询，不能把长任务绑在 HTTP session 上。
- 先实现 UDP active 形成最小可验证闭环，再在同一抽象上增加 TCP active/passive 与 PS/TS/ES。
- UDP/TCP active、TCP passive、ES/PS/TS、断线重连、sender 统计、端口复用与同 SSRC 多目标已完成；当前转入 GB28181 接收完整化。
- GB28181 接收侧已完成 UDP/TCP passive/TCP active（`connectRtpServer`）、RFC4571、64 包/50ms 乱序窗口、重复/丢包统计、PS/TS/ES 自动识别、H.264/H.265 分片重组、PSM 音频映射和 RFC3640 AAC。
- 收到的 GB RTP 会发布为统一 `MediaSource`，可由 `startSendRtp` 再次受控转推；真实 UDP 测试已验证接收 SSRC/PT 与新目的 SSRC/PT 隔离。
- SIP 对讲现支持 `INVITE(s=Talk/sendonly)`、200 SDP 协商、ACK、G.711A/U ES-RTP 发送、BYE 和设备离线清理；管理 API 与前端均可启动、停止和查看活动对讲。阶段 3 已完成，当前转入原生 HTTP/HLS 拉流。
- 原生 HTTP 拉流已实现 HTTP/1.1 固定长度、连接关闭和 chunked body、最多 5 次跳转、IPv4/IPv6 URL、HTTPS 公共 CA 校验；HTTP-FLV、HTTP-TS、HLS 主/媒体清单、TS segment 与 `EXT-X-MAP` CMAF/fMP4 均发布统一 MediaSource。
- CMAF 解封装器解析 init 段的 H.264/H.265/AAC 轨道与配置，并解析 `moof/traf/tfhd/tfdt/trun/mdat` 的 sample duration/size/flags/CTS；HTTP-FLV 与 HLS-fMP4→UDP RTP 网络闭环均已验证。阶段 4 完成，当前转入双向协议与点播。
- RTSP 主动推流已接入既有 `addStreamPusher`：支持 H.264/H.265 与可选 AAC，执行 ANNOUNCE/SETUP/RECORD、TCP interleaved RTP、缓存 GOP/实时帧发送和 TEARDOWN；支持 Digest 上游鉴权，`rtsps://` 拉推流使用 WebPKI 公共 CA 校验，真实双服务器与 TLS 测试通过。
- SRT 已补齐正确的 libsrt 1.5 ABI、Listener/Caller/Rendezvous、MPEG-TS 拉推流、streamid、延迟与 passphrase 参数，并接入 `addStreamProxy`/`addStreamPusher`；Caller 推流、Caller 拉流与 Rendezvous 均有真实 UDP 网络测试。阶段 5 继续完成 MP4 多协议 VOD 与 seek。
- MP4 VOD 已抽成 RTMP/RTSP/HTTP/WS-FLV 共用资源库：统一限制在 `record` 根目录，拒绝编码后的目录穿越；seek 回退到前置视频关键帧并重置时间戳，所有流式出口按媒体时间实时节奏发送。RTMP 发送 Play.Stop/StreamEOF，RTSP 支持 NPT Range，HTTP/WS-FLV 支持 `start`/`duration`。真实四入口网络测试通过，阶段 5 完成并进入集群与生命周期。
- 边缘回源支持兼容单地址的有序 `origin_urls`、URL 占位模板、逐上游超时和同流 single-flight；RTMP/RTSP/HTTP/WS 的持久播放器统一登记订阅，最后读者离开后仅关闭按需代理，宽限期内恢复播放会取消关闭。
- 集群推流修正为按本地 vhost/app 过滤并把 `push_to.url` 视为远端发布基址，支持目标模板；每个源/节点仅保留一个中继任务，断线按 1–30 秒指数退避重连，`StreamUnPublish` 立即取消。阶段 6 完成并进入 WebRTC 与多轨。
- WebRTC 已补齐 WHIP/WHEP 会话自动清理与真实播放器计数；WHEP 可触发边缘回源。WHIP 依据协商结果动态发布 H.264/VP8/VP9/Opus 多轨，保留 RID/SSRC/远端 track/stream 标识；WHEP 可输出 H.264/H.265/VP8/VP9/AV1/Opus，并通过 `?rid=` 选择 simulcast 层。
- WebRTC sender/receiver 均协商 NACK/PLI/TWCC，WHEP 开启 RTX 并实际接收、记录 REMB 码率反馈；配置支持进程级单 UDP 端口 ICE mux、ICE-lite、1:1 NAT 公网 IP 和指定绑定地址。
- WHIP/WHEP resource 的 `PATCH application/trickle-ice-sdpfrag` 同时支持普通 candidate 和完整 ICE restart：新远端 ufrag/pwd 会更新 offer、触发本地 ICE 重新采集并返回 200 SDP fragment；真实媒体会话验证重启后继续收包。
- 原生远端客户端已接入统一代理/推流监督器：`whep://`/`wheps://` 把远端 WebRTC 轨道回灌本地 MediaSource，`whip://`/`whips://` 把本地兼容轨推送到远端；HTTP(S) 使用 WebPKI、支持 IPv6 URL 和相对 Location。webrtc-rs 0.12 未实现 ICE-TCP mux，该边界仍保留在阶段 7。
- 运维阶段已提前修正配置 API 的虚假热更新语义：只接受启动快照中的已知键；`api.secret` 会更新当前 `StreamAuth`，现存 HTTP/RTMP/RTSP 会话共享新 secret；其他尚未动态接线的键明确列入 `restartRequired`，未知键列入 `rejected`。前端会显示两类结果。
- `HookClient` 已改为进程内共享的原子运行时配置；全部 `hook.*` URL、超时、重试和流量上报周期可通过配置 API 即时更新。每次回调先复制快照，不跨 `await` 持锁；流量上报调度器通过配置修订通知立即重置计时，支持从关闭状态动态启用。
- Hook 传输现在区分 HTTP/HTTPS，HTTPS 使用 WebPKI 公共根证书校验；URL 解析支持带括号 IPv6，拒绝未知 scheme 和 URL userinfo。
- HTTP/RTMP/RTSP TLS 监听器改用共享可热替换 rustls 配置；`reloadCertificate` 重新读取启动时 PEM 路径，新握手即时使用新证书，解析/密钥匹配失败时保留最后一份有效配置。监听端口与 SSL 开关变化仍需重启。
- IPv6 第一批已贯通：新增全局 `listen_ip` 并让 HTTP/API、RTMP、RTSP、WebRTC 信令、SRT、GB28181 SIP/RTP 都通过 `SocketAddr` 绑定；SRT FFI 使用 `sockaddr_storage` 支持 IPv4/IPv6，RTMP 拉推流共享 bracket-aware URL 解析。真实 `::1` GB28181 RTP、SRT Caller/Listener 与原生 WHEP 信令/媒体回灌均通过。阶段 8 继续补跨平台 CI 与嵌入式 SDK 边界。
- 新增独立的 Cross-platform CI：Linux 执行完整 fmt/Clippy/test/release 并真实链接 libsrt，macOS/Windows 执行 workspace `--all-targets` 类型检查；actions 使用 `checkout@v6`、`cache@v5`，不恢复用户移入 `workflows_bak` 的旧文件。
- `EmbeddedMediaKit` 作为稳定 Rust SDK 门面复用生产 `MediaSourceManager`，提供 publisher、缓存 GOP + live subscription、事件订阅、流快照与显式关闭；宿主可把同一 manager 传给协议 listener，不需要复制媒体图实现。阶段 8 的 SDK 基线已完成，后续仍需验证各平台 CI 实际运行结果。

## 本轮错误记录

| 错误 | 尝试次数 | 处理 |
|---|---:|---|
| PowerShell 内嵌引号/复杂正则/`&&` 组合命令解析失败 | 5 | 后续多项只读检查使用工具层 `Promise.all` 和多个简单 `rg` 模式，不在 PowerShell 中拼接复杂命令 |
| `rtk bat`/`rtk Get-Content` 不可用 | 2 | PowerShell 内建命令必须使用 `rtk powershell -NoProfile -Command Get-Content`，普通文本优先 `rtk rg`/WSL `sed` |
| Windows `rg crates/*/Cargo.toml` / `www/*.html` 将通配符当非法路径并使并行读取失败 | 2 | 改用 `rtk rg -g <glob> ... .` 或显式文件名，不依赖 PowerShell 展开 Unix 风格 glob |
| 一次跨三个规划文件的补丁包含了不属于目标文件的上下文，整体校验失败 | 1 | 拆成按文件、按真实尾部上下文的小补丁后成功写入 |
| RTP 单元测试构造 `VideoInfo` 时误用了不存在的 `bitrate` 字段 | 1 | 按实际结构删除该字段后重新编译 |
| 机械插入 `http_rtp_sender` clone 时命中了更早的 RTMP `sm` 变量 | 1 | 用协议分支上下文将 clone 移到 HTTP server 分支 |
| 标准化 PES parser 后旧 `parse_pes_video_basic` 仍构造少一个 flags 字节的非标准 PES | 1 | 同步修正测试夹具和 `PES_packet_length`，再以 core mux → codec demux 往返测试验收 |
| Clippy 报 `send_frame` 9 个参数超过限制 | 1 | 将轨道选择、packetizer 和容器 muxer 合并为 `RtpSendState`，全目标 Clippy 复验通过 |
| 全量 SRT 测试中的 PS 分片夹具重复使用 sequence=1 且 PES 少写 flags2 | 2 | 改为递增 sequence 并构造标准 `flags1 + flags2 + header_data_length` PES，分片无 marker 测试恢复通过 |
| Clippy 报 GB RTP `handle_rtp` 8 个参数超过限制 | 1 | 将 PS/TS/自动识别/FU 状态统一收进 `RtpIngestState`，严格 Clippy 复验通过 |
| SIP 响应首行把 `OK` 当作状态码解析，导致平台 INVITE 永远不进入 ACK/激活分支 | 1 | 改为解析 `SIP/2.0` 后的第二个 token，并由真实对讲 SIP/RTP 往返测试覆盖 |
| RTP 转推测试在首个入站包前启动 sender，媒体源尚未创建 | 1 | 先发送入站 RTP 并等待接收端发布 MediaSource，再验证缓存 GOP 经 sender 到达新 UDP 目的端 |
| HTTP-FLV 测试在拉流任务结束后才订阅广播，错过实时帧 | 1 | 按真实晚加入播放者语义读取 GOP cache，再增加拉流→RTP 网络出口验证 |
| 严格 Clippy 报 HLS playlist 四元组返回类型过于复杂 | 1 | 提取 `PlaylistEntries` 结构体后复验通过 |
| SRT URL 解析时 `local_port` 无法推断整数类型 | 1 | 显式标注为 `Option<u16>`，单元测试与 Clippy 通过 |
| 首条 SRT 网络 E2E 长时间等待且无媒体 | 3 | 增加有界超时与 socket 状态诊断，依次修复错误 FFI 常量、message API 签名、accepted socket 非阻塞继承及 PAT/PMT 偏移 |
| RTMP VOD seek 测试发现输入 200ms 关键帧落在 233ms | 1 | 定位 MP4 `stts` 将前一段时长错误写到后一 sample；改为由后继时间戳决定当前 sample duration，末帧复用上一 cadence |
| SRT IPv6 FFI 返回 `sockaddr_storage` 后旧拉流 E2E 仍按 `sockaddr_in` 接收 | 1 | 同步测试夹具类型；随后把 sockaddr 结构改为零初始化再赋公共字段，兼容 BSD/macOS 的 `sin_len`/`sin6_len` |
| HTTP VOD 全套并行测试偶发第二次请求 404 | 1 | 定位同一测试二进制中的多个用例共享并删除固定 `/tmp` 目录；新增 MP4 VOD 用例改用独立 `tempdir` 后全套稳定通过 |
| ICE UDP mux 首次绑定 `127.0.0.1` 时 SDP 仍广告 WSL 其他接口，协商成功但媒体为 0 | 1 | 指定绑定 IP 时同步设置 ICE IP filter，并在 loopback 场景显式允许 loopback candidate；真实 WHEP 媒体测试通过 |
| simulcast E2E 的双轨描述首次误加到直接 WHEP 用例而不是 HTTP 用例 | 1 | 按 stream 上下文移动到目标用例，验证未知 RID=404、指定 RID=201 且媒体可达 |
| 新增 API 断言后 `cargo fmt --all --check` 报一处换行差异 | 1 | 执行 `cargo fmt --all` 后复验，并继续以严格 Clippy 和定向测试验收 |
| `cargo test` 命令传入两个独立测试名，Cargo 报 unexpected argument | 1 | 改为运行完整 `api_e2e` 测试目标，同时与 Node 语法检查并行 |
| WSL 依赖源码检索命中了 Windows 商店内置 `rg`，Linux 侧返回 Permission denied | 1 | 改用 WSL `find`/`grep` 精确定位 Cargo registry；工作区检索继续使用 Windows `rtk rg` |
| 原生 WHEP 首次格式门禁发现一处 rustfmt 换行和未使用 `Buf` 导入 | 1 | 删除无用导入并执行 `cargo fmt --all`，编译检查通过 |
| WHEP E2E 新测试首次编译重复导入 `WebRtcServer` | 1 | 复用测试文件既有导入，修正后真实网络测试通过 |
| 全门禁组合命令在 workspace tests 刚开始时达到 120 秒外层超时，随后出现 `Broken pipe` | 1 | 利用已生成缓存把 workspace tests 与 release build 拆开；前者 35 秒全绿，后者 87 秒成功 |
| 直接 `rtk wsl cargo` 未加载登录环境，且 WSL 没有 `rustup` 命令 | 2 | Cargo 继续统一经 `rtk wsl bash -lc`；Windows/macOS 源码兼容由新增 GitHub runner 矩阵验证 |
| WSL Git 将 Windows 工作树 CRLF 全部报告为 trailing whitespace | 1 | 不机械改写用户文件换行；改用仓库实际 Windows Git 配置执行 `git diff --check`，结果通过 |
| 并发注册测试首次引用了不存在的 `PusherEntry::belongs_to`，并在 `JoinHandle::filter` 中移动所有权 | 1 | 推流条目改用 `Arc::ptr_eq` 比较 marker；线程结果先 `map(join)` 再过滤成功值 |
| core 严格 Clippy 报测试模块后仍有生产函数 | 1 | 将 `split_key` 移到 `#[cfg(test)]` 模块之前，保持生产项在测试模块前 |
| 首版性能示例让 publisher 无节制跑满，broadcast subscriber 报 `channel lagged by 37` | 1 | 每 128 帧增加消费者 ACK 屏障，性能基线改为无丢帧端到端吞吐而非单纯生产速度 |
| WebSocket fuzz-smoke 首次使用未导入的 `Bytes` | 1 | 随机输入直接使用 `Vec<u8>`，保留解码器公开输入契约 |
| MP4 变异测试超过 60 秒，恶意 `stts/stsz/stsc/stco/co64/stss` 计数可放大循环与分配 | 2 | 所有 sample table 在分配/遍历前按 box 长度和文件长度校验，`stts` 与 `stsz` 样本数必须一致；变异回归降至 0.02 秒 |
| fMP4 `trun.sample_count` 可在缺少表字段时驱动超长循环，样本偏移相加也可能溢出 | 1 | 按 flags 计算每样本表宽并校验计数；仅默认 sample size 时按媒体数据上限约束；偏移使用 `checked_add` |
| fMP4 严格 Clippy 报非零分支中的手工安全除法 | 1 | 使用 `checked_div` 显式表达零除保护后复验通过 |
| 全工作区门禁偶发 SRT Rendezvous `Invalid socket ID` 或 5 秒收不到单个启动消息 | 5 | 改用 libsrt 专用 `srt_rendezvous`；测试采用双 loopback IP/同端口拓扑，并按真实 live 推流每 100ms 发送 7×188 TS 批次直到 ACK，保留 5 秒硬超时；连续 10 次与 workspace 并行测试均通过 |
| 组合门禁在全部测试通过后于 release 编译末段无诊断退出 1 | 1 | 拆分复跑 `cargo build --release --workspace --locked`，76 秒成功；未发现代码编译错误 |
| WSL 中没有 Node，`node --check` 返回 command not found | 2 | 使用 Codex bundled Node 的绝对路径执行，管理前端语法检查通过 |
| 首次 session-catchup 命令嵌套 `$(pwd)` 引号不完整 | 1 | 改用显式 `/mnt/d/work_space/ai_coding/zlmediakit-rs` 参数后成功 |
| GitHub API 首次命令同时存在 shell 引号不匹配且 WSL 未安装 `jq` | 1 | 改为小分页 API 直接输出 JSON，并逐个查询 run/jobs 端点 |
| GitHub 公共 job 日志下载端点返回 403 | 1 | 使用无需认证的 run/jobs 元数据确认失败步骤；详细日志需要 GitHub 凭据时不绕过权限 |

---
