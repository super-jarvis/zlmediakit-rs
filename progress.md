# 进度日志

## 2026-08-02

- 用户要求创建并落实“完善前端管理台 + 配置 secret 鉴权”目标；已创建活动目标并将七阶段计划写入 `task_plan.md`。
- 使用 planning-with-files-zh 技能恢复三个规划文件，保留上一轮协议互转目标的完成记录。
- 完成首轮前后端能力对照：前端目前 6 个模块、调用约 17/48 个管理 API；确认 GB28181/RTP、转码、会话、录像归档、流详情和配置管理为主要缺口。
- 确认后端已有 `ServerConfig.secret`，但 `/index/api/*` 尚无统一校验且 `getServerConfig` 会回显 secret；当前进入鉴权契约和 HTTP 注入路径审计。
- Windows PATH 中直接执行 `node --check` 失败；已记录，后续改用 WSL 或浏览器测试链路。
- 已在 HTTP API 分派入口增加统一 secret 守卫，支持 `X-API-Secret`、`Authorization: Bearer` 和兼容查询参数；空 secret 可显式关闭 API 鉴权。
- `getServerConfig` 已对 secret 相关配置项脱敏并只返回 `secretConfigured` 状态，避免管理页取得服务端明文口令。
- 首轮 HTTP API 测试中鉴权新用例通过；发现原套件共用固定端口导致 1 个并行用例偶发占用，已改为动态端口。
- 已新增前端登录/退出、统一业务错误处理、WSS、流详情/截图、会话、录像归档、GB28181、RTP、转码、运行时配置、线程负载与重启页面。
- WSL 没有 Node，首次 `node --check` 无法执行；后续改用工作区捆绑 Node，不重复该失败路径。
- 使用捆绑 Node 对增强后的 `www/js/app.js` 执行语法检查，当前通过。
- 修复运行时配置快照只显示默认值的问题，服务启动后会同步实际配置文件及 CLI 覆盖值；管理 API 新增 WebRTC、SRT、GB28181 状态和端口摘要。
- 播放器新增浏览器原生 WHEP 协商、ICE 等待、远端轨道挂载和 DELETE 资源清理；服务器页新增基于实际端口的 RTMP/RTSP/WebRTC/SRT 地址参考。
- 首次联调误用不存在的 `target/debug/zlmediakit-server`，已确认实际产物为 `target/debug/zlmediakit`，不再重复错误入口。
- 联调启动前发现默认 `www_root="./www"` 会解析为 `www/www`，已归一化为仓库自带的 `www/` 目录。
- 真实浏览器验证错误 secret 拒绝、正确 secret 登录与 `sessionStorage` 恢复均正常；会话、录像、GB28181、RTP、转码、服务器页均加载成功，控制台无错误，实际 RTMP/RTSP/WebRTC 端口正确且无明文 secret。
- 完成最终 API 覆盖审计并补齐批量关流、在线探测、录制状态详情、RTP 详情、设备缓存快照、单转码详情和危险的 SIP 停止入口；`updateConfig` 仅为 `setServerConfig` 兼容别名，不重复 UI。
- 新增静态管理后台契约测试，验证登录、核心模块、WHEP、鉴权请求头及关键 API 入口；HTTP API E2E 最终 18/18 通过。
- 最终 WSL 门禁全部通过：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace --tests`、`cargo build --workspace --release`。
- 最终 `node --check www/js/app.js` 与 `git diff --check` 通过；Web 资源未包含配置文件的实际 secret，明文只存在于既有 core 默认配置实现中。

- 新增 `protocol_conversion_matrix` 真实媒体测试：ffmpeg 生成 H.264/AAC FLV，项目自身完成解复用及 FLV、MPEG-TS、MP4、fMP4 转封装，四种产物均通过 ffprobe 编码识别与 ffmpeg 全量解码。
- FLV demuxer 现保留完整 FLV payload 并标注 `PayloadFormat::Flv`，正确设置配置帧、解析有符号 CTS 得到 PTS，并对未知视频/音频编码返回 `CodecId::Unknown`，不再伪装为 H.264/AAC；FLV 测试 13+4+2 全通过。
- 真实播放器验证暴露并修复普通 MP4 的 VisualSampleEntry、tkhd/mvhd、dref/url、AAC esds、采样率定点数及 chunk offset 错误；MP4 改为合法的 ftyp+mdat+moov 布局，19 个库测试和 2 个 recorder E2E 通过。
- 修复 fMP4 空 sample table、avcC/hvcC 配置截断、compressor name 长度、mvex/trex 缺失、trun flags/data_offset 及音视频 mdat 偏移；初始化段与媒体段拼接后已可被 ffprobe/ffmpeg 读取和解码。
- FLV muxer 的 `write_tag` 改为显式返回 `Result`，不再在转换失败时原样写入无效 payload；补充 MP3 FLV 封装，Opus 等 classic FLV 不支持组合会明确返回错误。
- 修复 H.264 SPS 边界检查：解析宽高前要求至少 7 字节，避免畸形/精简测试配置触发越界 panic。
- 增加真实 H.265/AAC MP4 输入矩阵：项目解复用后分别转为 HLS MPEG-TS、普通 MP4、fMP4，三种输出均通过 ffprobe 的 HEVC/AAC 识别和 ffmpeg 全量解码；H.264 与 H.265 两条播放器级矩阵共 2/2 通过。
- 外部 ffmpeg MP4 揭示 demuxer 将规范的文件绝对 chunk offset 错当作 `mdat` 内偏移，并错误地把 `stsc` 每条记录当成单个 chunk；已改为以完整文件取样并按 `first_chunk` 范围展开，MP4 19+2 回归测试继续通过。
- 最终 WSL 门禁全部通过：`cargo fmt --all --check`、`cargo check --workspace --all-targets`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace --tests`、`cargo build --workspace --release`。
- WebRTC 可选 `aac-transcode` feature 最终复验通过：5 个单元/转码测试和 3 个 WHIP/WHEP 网络 E2E 全绿。
- 严格 FLV 错误返回使旧录制/VOD/WS-FLV 测试夹具缺少 `config_frame`/`PayloadFormat` 的问题可见；所有夹具已补成真实 FLV 语义，HTTP crate 全套 E2E 与最终 workspace 全量测试通过。
- README 已增加编解码与协议互转边界矩阵、转码限制、Linux/ffmpeg/可选 AAC→Opus 依赖和 CI 等价验证命令，并修正 badge 链接仓库所有者。

- 安装 WSL 验证依赖 `libopus-dev`/`libfdk-aac-dev` 后，WebRTC `aac-transcode` feature 全套通过：5 个单元/转码测试和 3 个 WHEP/WHIP 网络 E2E；默认 feature 的 3+3 测试也通过。

- Core/WebRTC 首轮编译发现 `video_config_annex_b` 代码块在编辑时重复插入两次；已删除重复定义并保留单一实现，属于局部编辑失误而非设计问题。

- Core 新增 decoder config→Annex-B 参数集转换，WHEP H.264 输出已用它和统一 sample 转换替换 FLV/AVCC 私有解析，因此可直接播放 RTMP/RTSP/WHIP/SRT/GB28181 来源的 H.264。
- WebRTC AAC→Opus feature 路径已在送入解码器前规范化 ASC 和 raw AAC，避免把 FLV/ADTS 头传给 AAC decoder。

- SRT/GB28181 crate 全套 18 个测试通过；新增 PES payload、PTS/DTS 与 90 kHz→ms 用例均通过，原有 SIP/PS/RTP 测试无回归。

- SRT MPEG-TS demux 已改为解析 PES `PTS_DTS_flags`、header length 与 33-bit PTS/DTS，并保存到各音视频 PES 累积器；去掉按每次 `srt_recv` 固定加 40 ms 的 TS 时间轴。
- 修复原 `extract_pes_payload` 把 PES packet length 高字节误当 optional header length（`6 + payload[4]`）的问题；现在使用规范位置 `9 + payload[8]`。
- 去掉每个 SRT recv buffer 末尾强制 flush PES 的行为，避免跨 recv 的 PES 被拆成残帧；改为下一个 payload-unit-start 或连接关闭时 flush。

- RTSP 全套验证通过：9 个单元测试、3 个 Digest 鉴权测试及 H.264/H.265 推流、拉流代理网络 E2E 全部无失败；新增 Annex-B、ADTS、Opus 与真实音频 SDP 用例均通过。

- RTSP 首轮测试编译仅失败于新增 SDP 单测构造 `AudioInfo` 时遗漏 `bits_per_sample` 字段；实现代码已通过该编译阶段，测试夹具字段现已补齐。

- RTSP 输出开始统一：视频先经 core 规范化为 FLV/长度前缀再进入现有 H.264/H.265 RTP packetizer；音频 packetizer 按 AAC/Opus/G.711/L16/MP3(MPA) 分流并使用对应 RTP payload type/clock rate。
- RTSP DESCRIBE 已按真实音频 codec 生成 AAC、Opus、PCMA、PCMU、MPA、L16 SDP，不再把任意音频轨固定声明成 AAC。

- 统一 FLV/RTMP 输出验证通过：core 41+9、FLV 11+3+2、RTMP 单元与全部网络 E2E（含 RTMPS/推拉流/HEVC/音视频）均无失败。

- Core 新增统一 `flv_payload` 输出转换：H.264/H.265 自动生成 FLV video packet 头并转长度前缀样本，AAC 自动移除 ADTS 并生成 FLV AAC packet，G.711 A/U 可生成 FLV 音频头；已增加 Annex-B→FLV 与 ADTS→FLV 回归用例。

- HLS 首轮编译定位到一个测试仍直接调用已重命名的 HEVC 配置解析 helper；已改为验证无 FLV 头的 hvcC record，并清理不再使用的 FLV 私有转换函数。

- HLS 分片与 live HTTP/WS-TS 媒体路径已切到统一 payload 转换层：视频统一规范化为 Annex-B，AAC 统一为 ADTS，并避免对已有 ADTS 输入重复加头。
- HLS 配置帧识别开始使用显式 `config_frame`/`PayloadFormat`，同时保留旧 FLV 内容识别兼容逻辑。

- 用户要求建立持续目标并落实所有协议与互转能力；已创建活动目标。
- 使用 planning-with-files-zh 技能恢复现有计划、发现和进度文件；Windows PATH 缺少 Python，`session-catchup.py` 未能执行，已改用完整读取文件与 Git 状态手动恢复。
- 建立 codebase-memory-mcp fast 索引：2955 nodes、11778 edges。
- 完成第一轮协议完整性审计，定位内部负载格式不统一、SRT 时间戳简化、RTSP 音频 SDP 固定 AAC、默认 WebRTC AAC→Opus 未启用、MP4/fMP4 原样写入 FLV payload 等关键断点。
- 将旧 CI 修复计划升级为八阶段协议落实计划；当前进入阶段 1：格式契约与失败测试。
- 检查工作区发现用户已把 CI/Release 工作流移动到 `.github/workflows_bak/`；记录为保留改动，协议实现不会撤销。
- 为 `MediaFrame` 增加 `PayloadFormat`，新增视频 FLV/AVCC/HVCC/Annex-B 与 AAC FLV/ADTS/Raw 的集中转换模块和首批单元测试。
- 首轮 core 测试 38/39 通过；Annex-B 三字节起始码边界已修复。针对性复验显示转换器会把三字节起始码规范化为四字节，已修正测试预期，等待再次复验。
- Annex-B 针对性测试复验通过；core 全套测试通过：39 个单元测试、9 个集成测试，无失败。
- `cargo fmt --all --check` 首次仅报告新增代码格式差异，已运行 `cargo fmt --all`。
- WSL `cargo check --workspace --all-targets` 通过，确认新增 `PayloadFormat` 字段和转换模块未破坏其他协议 crate 编译。
- 已给 RTMP/RTSP、RTMP 拉流、WHIP、SRT、GB28181、转码输出和 MP4 解封装入口标注真实负载格式；workspace 全目标检查再次通过。
- MP4/fMP4 muxer 改为写入前集中提取 decoder config、长度前缀视频样本和 raw AAC，新增两个防止 FLV 头进入 `mdat` 的回归测试。
- MP4 首轮测试 18/19 通过；唯一失败是旧测试仍期待 FLV 头，已更新为新的正确样本契约。
- MP4 库测试已 19/19 通过；recorder E2E 单独运行仍失败，排除并行目录竞争。
- 根因是 recorder 启动窗口可能错过实时广播且未回放 GOP；已加入最新 GOP 回放，并让 MP4/fMP4 对旧式 FLV 配置帧使用内容识别兼容。

## 2026-08-01

- 读取项目级全局指令 `RTK.md` 与 planning-with-files-zh 技能说明。
- 尝试读取代码知识图谱；确认本项目未索引，记录后切换为本地检索。
- 检查 Git 状态：`master...origin/master`，无已有修改。
- 创建任务计划、发现记录与进度日志。
- 首次批量读取因 Windows PATH 中没有 `cat` 而失败；已记录并改用 `rtk rg` 读取文本。
- 读取 README、Cargo 工作区、仓库文件清单、Actions 文件概览和最近提交历史。
- 初步确认项目是多协议 Rust 流媒体服务器，当前有 CI、Docker 发布、release-plz 三套工作流。
- 完整读取三套 workflow、根 Cargo 清单、server 清单、Dockerfile 和 README 构建相关内容。
- 发现 CI 未安装 Dockerfile 中显式存在的原生构建依赖，且 macOS/Windows job 仍无条件构建完整 workspace；下一步核对 GitHub 实际失败日志并在 WSL 复现。
- 尝试通过 Web 获取 Actions 状态失败，已记录并切换到 GitHub CLI/API 路径。
- 检查发现 Windows PATH 中没有 `gh`，不再重复该路径，改查 GitHub REST API。
- 经批准访问公开 GitHub REST API，确认当前最新提交上的 CI、Docker Publish、Release 三套 Actions 全部失败，并取得 run ID。
- 读取全部失败 job 的步骤状态和 GitHub annotations，确认至少四类独立问题：Linux 测试失败、SRT FFI 的 macOS/Windows 不兼容、Docker Debian 包名错误、release-plz token 输入名错误。
- 首次 WSL 枚举被沙箱拒绝（E_ACCESSDENIED）；已记录，下一步以批准模式重试。
- 成功枚举 WSL2 `Ubuntu-26.04`，并读取 SRT FFI/服务器实现，确认其当前为 Linux 风格实现。
- GitHub 原始日志 API 返回 403；已改为在 WSL 执行与 Ubuntu CI 相同的测试命令。
- 检查 WSL 工具链：Rust 1.96.0 可用，但 SRT 开发库当前缺失；准备先原样运行 CI 测试命令复现。
- 在 WSL 原样运行 `cargo test --workspace --tests`，复现 GitHub Ubuntu CI 的退出码 101：多个测试目标链接时找不到 `-lsrt-gnutls`。
- 阶段 1、2 完成，进入 workflow/Docker 修复阶段。
- 在 WSL 安装 `pkg-config`/`libsrt-gnutls-dev` 并应用首轮配置修复。
- 修复后 workspace 与测试目标全部编译成功；首轮完整测试因工具 180 秒超时被终止，将利用现有缓存以更长超时重跑。
- 第二轮测试确认绝大多数 workspace 测试通过，剩余转码集成测试缺少 `ffmpeg`；已将 FFmpeg 加入 CI 系统依赖。
- 在 WSL 安装 FFmpeg 并重跑转码集成测试：6/6 通过。
- `git diff --check` 通过；历史提交确认项目此前确实在 release 流程暂时停用了 macOS/Windows 目标。
- 首次完整 Docker build 在 10 分钟工具上限处被终止且未返回阶段日志；下一步检查是否已生成镜像以及 BuildKit 缓存，再选择更可观察的验证方式。
- 确认超时前未产出最终镜像，但 BuildKit 中间缓存已保留；准备拆分检查 Docker 运行时包和 Rust 1.80 MSRV。
- 尝试汇总 Cargo metadata 的 MSRV 时遇到 PowerShell/WSL 嵌套引号错误；改为直接使用 `rust:1.80-slim-bookworm` 临时容器验证。
- 一次性 Debian 包安装验证因本地 Docker 网络过慢超时；不重复该网络操作，依赖 Debian 官方包索引确认包名。
- 通过 `docker buildx history logs` 取回超时构建日志，确认卡点是 Docker Hub 基础镜像下载速度，不是代码编译或包名错误。
- 直接读取锁定依赖清单，确认 `time 0.3.54` 要求 Rust 1.88，而 Dockerfile 仍是 1.80；已把 builder 更新到官方存在的 Rust 1.96.1 bookworm 镜像。
- WSL 全量 `cargo test --workspace --tests` 成功，无失败测试。
- 修复阶段完成，进入最终编译与静态检查阶段。
- WSL 格式检查、Clippy warnings-as-errors、release build 全部通过。
- WSL 不带 Ruby，YAML 解析器改查已有 Python 环境。
- 使用 WSL 现有 PyYAML 成功解析 CI、Docker Publish、Release 三份 workflow。
- 最终 `git diff --check` 通过；产品改动限定在 3 个配置文件。
- 补充阅读 server 入口与媒体源核心，实现项目运行时架构总结。
- 所有计划阶段完成。
# 2026-08-02：启动 ZLMediaKit 开源核心能力对齐目标

- 创建长期活动目标，范围明确为 ZLMediaKit 开源版核心能力，不含闭源专业版。
- 使用 planning-with-files-zh 恢复并保留上一轮计划、发现和进度；确认工作区有前端、鉴权和文档等未提交成果，不做回滚。
- 完成官方能力与本地实现差距审计，确定第一阶段为通用 RTP sender 及 ZLM 兼容 API。
- 当前正在审计可复用的 RTSP RTP packetizer、HTTP API 注入点和 server 生命周期管理方式。
- 新增 core RTP packetizer、UDP sender manager、SSRC 生命周期与网络测试；首次编译只发现测试夹具多写了 `VideoInfo.bitrate`，已修正。
- HTTP/Server 首次集成检查发现 RTP sender clone 被机械插入到 RTMP 分支，已按分支上下文修正。
- `startSendRtp`/`stopSendRtp` 已接入 ZLM 参数名和 Secret 保护的管理 API；支持 UDP active、可配置 SSRC/PT/源端口、按 SSRC 或整流停止。
- `use_ps=1` 已实现为真实 MPEG-PS-over-RTP，默认 API 参数可直接工作；`use_ps=0` 继续提供 ES RTP。
- 真实 HTTP API → MediaSource → UDP receiver 端到端测试通过，验证 PT、SSRC、本地端口和停止计数。
- 第一阶段验收完成：core+HTTP 完整测试、针对性 RTP/API 测试、格式化、Clippy `-D warnings` 与 `git diff --check` 全部通过；进入 TCP/PS 阶段。
- RTP sender 已扩展 TCP active，使用 RFC 4571 两字节长度前缀；支持 IPv4/IPv6 目标解析与可选源端口绑定。真实 TCP listener 测试验证帧长度、RTP SSRC 和停止生命周期。
- 新增 core `PsMuxer`：生成 MPEG-2 pack header、system header、PSM、带 PTS/DTS 的 PES 和 MPEG-2 CRC；支持 H.264/H.265/AAC/G.711A/U/MP2/MP3，AAC 自动补 ADTS。
- 修复 codec PS demuxer 的标准 PES 可选头解析；61 个 codec 单元测试和 core mux → codec demux 往返测试通过。
- 新增默认 `use_ps=1` 的真实 HTTP API → MediaSource → UDP E2E，验证 RTP SSRC 及负载 `00 00 01 BA`；既有 ES、UDP、TCP 测试继续通过。
- 对齐 ZLM master 的 RTP API：新增 `type=0/1/2`（`use_ps` 兼容）、`only_audio`、`startSendRtpPassive` 和 `listRtpSender`；sender 列表包含 SSRC、目的端、端口、包数和总字节。
- TCP active/passive 共用可恢复传输层：主动模式初次连接失败直接返回，成功后断线重连；被动模式监听返回本地端口，客户端断开后可再次接入。真实 RFC 4571 重连测试通过。
- `type=2` 已接入现有连续 `TsLiveMuxer`，真实 HTTP API → UDP 测试确认 RTP payload 以 `0x47` 开始且保持 188-byte TS packet 边界。
- 本阶段完整门禁通过：core 51+9、codec 61+1、HTTP API E2E 22；core/codec/http/server 全目标 Clippy `-D warnings` 通过。
- 新增 `ssrc_multi_send` 与 UDP 源端口复用；同一 SSRC 两个目标共享本地端口并同时收到网络包，`stopSendRtp` 按 SSRC 返回停止 2 路。
- 通用 RTP 阶段最终验收通过：core 52+9、HTTP API E2E 22，格式检查和 core/http/server 全目标 Clippy `-D warnings` 通过；阶段 2 完成，转入 GB28181 接收完整化。
- 完成 GB28181 接收首轮调用链审计：确认 UDP-only、TS payload 被忽略、sequence 未用于重排/去重、PSM 音频 codec 未解析以及 AAC AU header 未处理；下一实现批次先扩展接收传输抽象与 RTP reorder buffer。
- TCP 扩展后的 RTP 4 项核心测试、UDP API E2E、`cargo fmt --check`、目标 crate Clippy `-D warnings` 与 `git diff --check` 全部通过。
- GB28181 RTP 接收已增加 64 包/50ms 有界重排：支持 u16 sequence 回绕、乱序恢复、重复丢弃、缺口跳过，并在 `RtpServerInfo` 暴露 lost/duplicate/reordered 计数。
- `openRtpServer` 的 `tcp_mode=0/1/2` 已贯通 manager；被动模式监听 RFC4571，主动模式预留本地端口并由新增 `connectRtpServer` 设置目标、断线重连。两种模式均通过真实 TCP 网络测试。
- RTP 接收默认改为自动封装识别：可从静态 PT、PS start code、TS sync byte 和 H.265 NAL/FU/AP 识别 G.711/PS/TS/H.264/H.265；TS 复用 SRT 解复用器并发布 PES 媒体帧。
- 补齐 H.265 RFC7798 FU/AP/单 NAL 重组；真实 UDP 测试验证自动识别 H.265 FU 并发布关键帧，TS-over-RTP 测试验证自动识别和 H.264 PES 解复用。
- codec PS demuxer 现解析并保存 PSM stream map；GB receiver 可区分 H.264/H.265、AAC、G.711A/U、MP2、MP3。AAC 支持 ADTS 和 RFC3640 AU Header 去除。
- 本轮回归通过：codec 62+1、SRT 26、HTTP API E2E 22；codec/srt/http 全目标 Clippy `-D warnings` 与格式检查通过。阶段 3 仅剩 RTP 转推和 SIP 双向语音对讲。
- 完成 GB28181 SIP 双向语音对讲：新增 `startTalk`/`stopTalk`/`getTalkList`，以已发布 G.711A/U 音源协商 PCMA/PCMU，通过独立 SSRC 和 RTP 端口向设备发送音频，并在 BYE、设备注销和服务器停止时清理 sender。
- 修复 SIP 响应状态码解析和 ACK CSeq 格式：平台发起的点播/对讲 INVITE 现在能正确处理 200、保存远端 dialog tag，并发送标准 `CSeq: <n> ACK`；真实伪设备测试完成 REGISTER→INVITE→200→ACK→G.711 RTP→BYE 全闭环。
- GB28181 管理页新增对讲音源配置、通道对讲按钮和活动对讲表；前端脚本通过 Node `--check`。
- 新增 GB RTP 接收→MediaSource→通用 RTP sender→新 UDP 目的端闭环测试，证明接收流可通过 `startSendRtp` 受控转推。
- 阶段 3 验收通过：SRT 29 项、HTTP API E2E 22 项、`cargo fmt --check`、srt/http 全目标 Clippy `-D warnings` 全部通过；转入阶段 4 原生 HLS/HTTP-FLV/HTTP-TS 拉流客户端。
- 新增原生 HTTP(S) 拉流客户端并接入 `run_proxy_supervisor`：支持 HTTP/1.1 Content-Length、connection-close、chunked、重定向、IPv6 URL和基于 WebPKI 公共根证书的 TLS 校验。
- HTTP-FLV 复用 FLV demux 并补充轨道信息；HTTP-TS 和 HLS-TS 复用从 SRT 抽出的增量 `TsStreamDemuxer`，可跨任意网络 read 边界保存 188-byte TS packet 与 PES 状态。
- HLS 支持相对 URL、master variant、直播清单轮询、VOD ENDLIST、segment 去重和 `EXT-X-MAP`；新增 CMAF/fMP4 demuxer，覆盖 H.264/H.265/AAC init config 与常见 `tfhd/tfdt/trun` sample 字段。
- 原生拉流 5 项网络测试通过：HTTP-FLV、HTTP-TS、HLS-TS、HLS-fMP4、URL 解析；其中 HTTP-FLV 与 HLS-fMP4 继续转发到真实 UDP RTP receiver，验证协议互转出口。MP4 21+2、SRT 29、HTTP 全套 E2E 通过，目标 crate/server 严格 Clippy 通过。
- 阶段 4 完成，进入阶段 5：RTSP 主动推流、SRT 双向模式与 MP4 多协议点播/seek。
- 最终目标 crate/server Clippy `-D warnings` 通过；全局及范围化 `git diff --check` 因工作区既有 CRLF 文件把每个新增行判为 trailing whitespace 而不可作为有效门禁，本轮新增 Rust 文件已由 rustfmt 规范化，未擅自批量转换用户现有文件行尾。
- 新增 RTSP RECORD 主动推流客户端并接入 `addStreamPusher` supervisor：基于源轨道生成 SDP，按视频/音频顺序 SETUP，发送缓存 GOP 与实时 TCP interleaved RTP，停止时 TEARDOWN。
- 真实 RTSP server-to-server E2E 已验证 ANNOUNCE→SETUP(video/audio)→RECORD→H.264/AAC RTP→远端 MediaSource；RTSP/server 严格 Clippy `-D warnings` 通过，前端目标地址提示同步支持 RTSP。
- RTSP RECORD 客户端新增 Digest challenge/retry，凭据从请求 URI、日志和订阅者标识中剥离；鉴权远端 H.264/AAC E2E 与严格 Clippy 通过。
- 新增 RTSP 客户端统一传输层，`rtsps://` 拉流和推流使用 Rustls/WebPKI 公共 CA 校验；自签测试 CA 的真实 TLS 握手/双向读写、RTSP 全套网络测试与严格 Clippy 均通过。同时把 workspace Rustls provider 固定为 ring，消除既有 TLS 配置首次构建时的双 provider panic。
- 修正 libsrt 1.5 FFI：`TRANSTYPE/LATENCY/RCVSYN/STREAMID` 等 socket option、状态和异步错误码全部对齐公开头文件；live 模式改用正确的 5 参数 `srt_sendmsg` 与 3 参数 `srt_recvmsg`，process-wide startup 只执行一次。
- 新增 SRT Caller 拉流、Caller MPEG-TS 推流和 Rendezvous 客户端，支持 `mode`、`localip/localport`、`latency`、`streamid`、`passphrase`，并接入统一拉流代理/推流中继 supervisor。
- 真实 libsrt 网络验收通过：Caller→Listener H.264/AAC TS 推流、Listener→Caller 拉流、双端 Rendezvous bind/connect/message 传输；SRT 全套 32+1+1 测试、fmt 与 SRT/server 严格 Clippy 通过。
- SRT 网络测试同时修复 TS 解复用旧缺陷：PAT program loop、PMT program_info_length 偏移，以及依据 PMT stream_type 固定 H.264/H.265，避免把 H.264 `0x41` P 帧误判为 H.265。
- 新增共享 `Mp4VodLibrary`/`VodAsset`：RTMP、RTSP、HTTP-FLV 与 WS-FLV 使用同一安全路径解析、轨道元数据、关键帧 seek、时间戳重置和实时 pacing；`record` 之外应用、绝对路径、父目录、反斜杠及编码穿越均拒绝。
- RTMP VOD 支持 `play.start`/`duration`，有限文件播放结束发送 `NetStream.Play.Stop` 与 StreamEOF；真实握手/AMF/视频网络测试验证 250ms 请求选择 200ms IDR、首帧归零并省略前一 GOP。
- RTSP VOD 支持 DESCRIBE/SDP、TCP/UDP RTP 和 `Range: npt=`，响应回报实际关键帧起点；真实 TCP interleaved 测试验证 `npt=0.250-` 返回 `npt=0.200-` 并发送对应 IDR。
- HTTP/WS-FLV VOD 支持秒单位 `start`/`duration`，按时间节奏输出并在 WS 文件末尾发送正常 close；同一真实 MP4 测试覆盖 HTTP FLV tag 与 WebSocket binary frame。
- VOD 网络回归发现并修复 MP4 `stts` 旧偏移：sample duration 现由后继时间戳计算、末帧复用上一 cadence，MP4 全套 23+2 测试和 RTMP/RTSP/HTTP/WS VOD 网络测试通过。阶段 5 完成，进入阶段 6 集群与生命周期。
- 阶段 5 最终门禁通过：RTMP 全套（含 RTMPS、推拉流、H.264/H.265/音频和 VOD）、RTSP 11 单元 + Digest + 六类网络 E2E、HTTP 21 单元 + 33 API/媒体网络测试全部通过；mp4/rtmp/rtsp/http/server 全目标 Clippy `-D warnings` 与 `cargo fmt --check` 通过。
- 阶段 6 完成多上游边缘回源：兼容 `origin_url` 并新增有序 `origin_urls`、逐候选超时、基础 URL/占位模板、同一路流 single-flight；首上游失败会自动切换，两个并发播放器仅产生一组回源尝试。
- 新增 `SubscriberGuard` 并接入 RTMP、RTSP TCP/UDP、HTTP-FLV/TS/fMP4、WS-FLV/TS/fMP4；最后播放器离开触发宽限期，仅停止按需回源，期间新播放器会取消定时关闭。发布/代理任务结束会关闭并移除媒体源，播放器能收到 channel close。
- 修复代理快速停止/重建竞态：旧任务只可移除自己的 active entry，不会按相同 stream key 误删新任务；代理自然结束时同步清理对应媒体源。
- 集群中继修复 vhost/app 过滤与远端 URL 重复 app 问题，新增 URL 模板、每源每节点去重、`StreamUnPublish` 取消和 1–30 秒指数退避重连。
- 阶段 6 门禁通过：core 54+9、server 3、RTMP/RTSP/HTTP 全套协议网络测试通过；core/rtmp/rtsp/http/server 全目标 Clippy `-D warnings` 与格式检查通过。转入阶段 7 WebRTC 与多轨。
- WebRTC 阶段首批完成：WHEP 接入 `get_or_pull`、RAII 播放者计数与 PeerConnection 失败/断开清理；WHIP 发布结束会发 `StreamUnPublish`、关闭并移除 MediaSource，DELETE 与断线路径均生效。
- WebRTC 编解码扩展：WHIP 动态接收 H.264/VP8/VP9/Opus 多轨，WHEP 可按源轨输出 H.264/H.265/VP8/VP9/AV1/Opus；AAC→Opus 可选 feature 继续通过。WHIP H.265/AV1 因 webrtc-rs 0.12 缺少可靠分片重组暂不宣称支持。
- WHIP 保存每轨 RID、SSRC、远端 track id 与 stream id；WHEP `?rid=` 可只播放指定 simulcast 层，未知 RID 返回 404。单元测试与真实 WHEP HTTP 媒体闭环覆盖该路径。
- WebRTC 可靠性/拥塞反馈补齐：协商 NACK/PLI/TWCC、H.264/VP8/VP9/H.265/AV1 RTX apt，WHEP SDP 声明 REMB 并由 RTPSender 实际读取码率估值；真实 RTCP 测试验证服务器收到 750000 bit/s。
- 新增共享 ICE UDP mux：`ice_port`、`ice_bind_ip`、`ice_lite`、`external_ips`；默认一个 UDP 端口服务所有 PeerConnection，显式绑定 IP 会约束 host candidate，避免广告不可达接口。真实 HTTP WHEP 单端口测试通过。
- WHIP/WHEP 新增标准 resource `PATCH application/trickle-ice-sdpfrag`：按 media section 解析 MID、m-line index、ufrag、多 candidate 与 end-of-candidates；POST/OPTIONS 暴露 Accept-Patch，真实 HTTP WHEP 会话验证 PATCH=204 后媒体继续可达。
- 阶段 7 当前门禁：core 54+9、server 3、WebRTC 8 单元 + 3 网络 E2E 通过；启用 `aac-transcode` 时 10 单元 + 3 网络 E2E 通过；相关 crates 严格 Clippy 与全工作区格式检查通过。阶段 7 仍继续处理完整 ICE restart、远端 WebRTC 客户端与底层库尚未实现的 ICE-TCP 边界。
- 运维配置首批落实：`StreamAuth` 增加共享运行时 secret，`setServerConfig?api.secret=` 使用旧 secret 鉴权成功后会立即轮换 API、播放签名和 RTSP Digest 的后续校验；真实 HTTP 测试验证旧 secret 立即 401、新 secret 立即 200，并恢复测试实例原值。
- `setServerConfig`/`updateConfig` 不再接受任意虚构键：未知键进入 `rejected`；当前尚未真正动态接线的已知项进入 `restartRequired`，避免只改展示快照却宣称热生效。管理前端已显示拒绝和需重启项。
- Core 新增运行时 secret/未知配置测试，当前 core 55+10、HTTP 21 单元 + 22 API + 全部媒体/VOD/录制网络测试通过；core/http 严格 Clippy 通过，管理前端 `node --check` 通过。
- Hook 动态配置完成：运行时快照新增全部 `hook.*` 键，`setServerConfig`/`updateConfig` 会校验 URL、正整数超时/周期和 retry；合法更新不进入 `restartRequired`，非法值进入 `rejected` 且不污染 HookClient 或全局快照。
- Hook HTTP 客户端补齐 HTTPS 公共 CA 校验、正确 443 默认端口、IPv6 bracket authority 和 scheme/userinfo 拒绝；单元测试验证同一个 HookClient 从空配置热切换到真实拒绝回调，再清空立即恢复放行。
- 流量上报任务改为运行期配置驱动：watch 修订通知可从未配置状态动态启用，并在周期变化时立即重置等待。
- HTTP/RTMP/RTSP TLS 接受器改为 `ArcSwap` 的可热加载配置；新增 `reloadCertificate` 管理 API。真实三次握手测试验证证书 A→证书 B 热切换、损坏私钥 reload 失败后仍继续呈现 B。
- 本轮验证通过：core 59+10、HTTP API E2E 22/22、RTMPS 网络 E2E、HTTP/RTMP/RTSP/server 全目标 check、相关 crates 严格 Clippy `-D warnings`。README 和示例配置已同步 Hook/TLS 热更新语义。
- 管理后台服务器配置卡新增“重载 TLS 证书”入口，操作前确认，成功显示更新监听器数量，失败明确说明仍保留最后有效证书；使用 bundled Node `--check` 和前端资产/API E2E marker 复验通过。javascript-pro 技能促使该异步操作沿用统一 `apiGet` 错误通道，并使用空值合并处理缺失计数。
- 完成后以 fast 模式重建代码知识图谱，当前包含 3786 个节点和 16453 条边，后续 WebRTC/IPv6 工作可继续使用最新调用关系。
- 新增原生 WHEP 拉流客户端并接入 `addStreamProxy`：`whep://`/`wheps://` 建立 recv-only PeerConnection，经 HTTP(S) POST/Location/DELETE 管理远端 resource，将 H.264/VP8/VP9/Opus 轨道回灌统一 MediaSource；配置的 STUN/TURN 会传入客户端。
- 抽取 WHIP/WHEP 共用远端轨接收管线；真实动态端口测试完成远端 WHEP server → ICE/DTLS/SRTP → 本地 MediaSource → H.264 GOP，并验证停止时删除 resource。
- 完成 WHIP/WHEP 完整 ICE restart：PATCH 带新 ufrag/pwd 时串行更新远端 offer、清除旧候选、调用底层 `restart_ice`、重新生成 answer 并返回 `application/trickle-ice-sdpfrag`；真实 WHEP HTTP E2E 验证双方凭据变化且重启后媒体继续增长。
- 新增原生 WHIP 推流客户端并接入 `addStreamPusher`：`whip://`/`whips://` 将本地 H.264/H.265/VP8/VP9/AV1/Opus 兼容轨通过远端 WHIP resource 发布；真实本地 MediaSource → 远端 WHIP server → 远端 MediaSource 网络闭环及 DELETE 清理通过。
- IPv6 监听配置落地：顶层新增 `listen_ip`（默认 `0.0.0.0`，可设 `::`），HTTP/API、RTMP、RTSP、WebRTC 信令、SRT、GB28181 SIP/RTP 均使用解析后的 `IpAddr`/`SocketAddr`；运行时配置快照同步暴露 `general.listenIP` 并明确修改后需重启。
- GB28181 RTP 管理器与 SIP server 支持指定绑定地址，UDP/TCP passive/active 保持地址族；真实 `::1` UDP RTP 输入成功发布 H.264 MediaFrame。
- SRT FFI 从固定 `sockaddr_in` 升级为 IPv4/IPv6 `sockaddr_storage`，URL 支持 bracketed IPv6 Caller/Rendezvous，本地默认地址随远端地址族选择；完整 SRT 35 个单元测试及拉/推 E2E 通过，其中真实 IPv6 Caller→Listener 已传输消息。
- RTMP 拉推客户端改用共享 IPv6 URL 解析器；原生 WHEP E2E 改为 `[::1]` HTTP 信令并继续完成 ICE/DTLS/SRTP→本地 MediaSource 回灌。相关 server/SRT/WebRTC 严格 Clippy、全工作区格式和 diff 检查通过。
- 新增 `.github/workflows/cross-platform.yml`，保留用户已移走的旧 CI：Linux 做完整质量门禁，macOS/Windows 做 workspace 全目标类型检查；所有官方 Node action 已升级为 `checkout@v6`/`cache@v5`，消除 Node.js 20 action 警告来源。
- core 新增 `EmbeddedMediaKit` Rust SDK：publisher 可设置轨道并发布统一 MediaFrame，subscription 先返回缓存 GOP 再接实时广播，另有事件订阅、SourceSnapshot 枚举、共享 manager 和显式 close/unpublish。闭环单测与 core 严格 Clippy、workspace `cargo check --all-targets --locked` 通过。
- 本批次最终门禁完成：workspace `cargo test --all-targets` 全绿（含真实 ffmpeg、RTMP/RTSP/SRT/WebRTC 网络测试），`cargo build --release --locked` 成功，workspace 严格 Clippy、格式检查及 Windows Git `diff --check` 均通过。首次组合执行仅因 120 秒外层超时被终止，拆分复跑无代码失败。
- 进入系统化验证阶段并完成首轮图谱审计：确认 retry/cancel/stop 的现有测试覆盖点，同时确认仓库尚无压力、长稳、fuzz 或 benchmark 基础设施；下一批先补 supervisor 故障恢复与媒体图并发压力，再建立解析器 fuzz-smoke 和可重复性能基线。
- 完成代理/推流控制竞态修复：DashMap entry 原子注册保证 16 个并发同-key add 仅一项成功；supervisor receiver 已关闭时 add 返回 false 并按任务 marker 回滚 active 条目。四项定向测试和 core 全目标严格 Clippy 通过。
- 新增 32 流 × 4 订阅者 × 200 帧媒体图压力测试，验证 25,600 次广播按序交付并在 unpublish 后零 source 泄漏；新增带每 128 帧消费者确认的 release benchmark，WSL 实测 50,000 源帧/200,000 次投递用时 33ms（约 1.49M 源帧/s），CI 最低门槛设为 20k fps 以只阻断数量级退化。
- 本轮 WebRTC 完整门禁通过：11 项单元测试、5 项真实网络 E2E（含完整 ICE restart、原生 WHEP 拉流和 WHIP 推流）、webrtc/server 全目标严格 Clippy、全工作区 rustfmt 与 `git diff --check` 均通过。
- 修复 MP4/fMP4 解析器的输入计数资源放大：所有普通 MP4 sample table 在分配/循环前校验 box/file 上限，`trun` 按 flags 表宽或默认样本大小限制 sample count，并对 sample offset 使用 checked arithmetic；MP4 24 项测试和严格 Clippy 通过。
- 新增 AMF、FLV、PS、MP4、WebSocket 五类解析器 fuzz-smoke；全工作区筛选测试全部通过，单个有效变异测试均在 0.02 秒内完成。FLV 同时新增 oversized header 与 20,000 unknown tags 防 panic/防递归回归。
- 阶段 8 的 Hook/API/TLS 热更新、IPv6、跨平台 workflow 定义和 EmbeddedMediaKit SDK 已落实，阶段状态转为完成；阶段 9 转入进行中，后续继续补真实 GitHub runner 结果、长稳/故障注入和兼容性文档。
- 全工作区门禁暴露并修复 SRT Rendezvous 启动期测试不稳定：连接使用 libsrt 专用 `srt_rendezvous`，双 loopback IP 同端口模拟真实两端，持续 1316-byte TS 批次直到 ACK 且总时限 5 秒；连续 10 次定向测试与完整 workspace 并行测试均通过。
- 本批次系统门禁完成：`cargo fmt --check`、workspace 全目标 Clippy `-D warnings`、workspace 全目标测试、workspace release build、管理前端 Node 语法检查和 Windows Git `diff --check` 全部通过；性能基线复测约 1.50M 源帧/s，远高于 CI 20k fps 下限。
- 2026-08-03 继续长期目标并通过规划技能恢复上下文；GitHub 公共 API 复核确认远端仍是 `ea37fcba`，旧 CI/Docker 成功、旧 Release 的 release-plz step 失败，而本地 cross-platform workflow 尚未跟踪、从未被真实 runner 执行。
- 图谱审计确认跨平台 workflow 的 Windows check 仍缺少强证据：SRT FFI 地址转换直接使用 Windows target 不提供的 Unix `libc::sockaddr_*` 类型；下一步改为 workspace 已有的 `socket2::SockAddr` 跨平台表示。
- SRT FFI 地址层已改为 `socket2::SockAddr` + opaque C pointer，移除 Unix `libc::sockaddr_*` 依赖；Linux 下 SRT 35 单元测试、Caller 拉/推 E2E、全目标 check 和严格 Clippy 全部通过。
- 2026-08-05 继续长期目标；补 RTSP 客户端 Basic 上游鉴权：`client_transport` 新增共享 `basic_authorization`/`digest_authorization`/`authorization_header`，拉流在 DESCRIBE 401 后按 WWW-Authenticate scheme 重试，推流在 ANNOUNCE 401 后同规则重试，两者都解析 `user:pass@host` 凭据。新增 `rtsp_basic_auth` E2E：mock 上游验证拉流 Basic 认证收帧、推流 Basic 认证转发 RTP、错误口令返回 401 并报错；rtsp crate 全测试与严格 Clippy 通过。
- 本轮 WSL 门禁注意：`cargo fmt --all` 在 WSL 会把 Windows CRLF 工作树整体转成 LF 造成 98 文件假 diff；已 `git checkout -- .` 恢复全部，仅重新套用目标改动，并用 Windows 原生 Git 验证 diff 仅含 4 个目标文件 + 1 个新测试文件。
- 补齐原生 HLS 拉流的 AES-128 与 EXT-X-BYTERANGE：`playlist_entries` 现解析 `EXT-X-KEY`（METHOD=NONE/AES-128、URI、IV）、`EXT-X-BYTERANGE:length[@offset]` 和 `EXT-X-MEDIA-SEQUENCE`；`open_response` 支持可选 Range 请求头。
- `pull_hls` 按 tag IV 或媒体序列号回退 IV 用 AES-128-CBC+PKCS7 解密 segment，key 按 URI 缓存；BYTERANGE 支持显式 offset 与省略 offset 的链式续读，并按 (URL, range) 去重避免同资源多段误跳。新增 4 个测试：AES-128 拉流解密、BYTERANGE 拉流、加密+range 清单解析、AES roundtrip；http crate 26 测试全过，workspace 全量、fmt、严格 Clippy、`git diff --check` 均通过。
- 补 RTSP 服务端 Basic 401 挑战（第②缺口）：core `StreamAuth` 新增 `check_basic`（RFC 7617，base64(user:pass) 解码后按用户名查用户表、未知用户名回退共享 secret 的常量时间比较，core 新增 workspace `base64` 依赖）；session `is_authenticated` 现可识别 `Authorization: Basic` 头（无需 realm 即可校验），`auth_reject` 的 `WWW-Authenticate` 改为同时宣告 Basic 与 Digest 两种挑战（Basic 在前）。
- 新增 `rtsp_server_basic_auth` E2E（3 用例）：无凭据 DESCRIBE 得 401+Basic 挑战、正确口令放行（404/200 而非 401）、错误口令 401、未知用户名回退 secret 放行、无用户表时 secret 为唯一凭据；`rtsp_digest_auth` 中两处 `starts_with("Digest")` 断言改为 `contains("digest")` 以匹配双挑战格式。rtsp/core 全测试、workspace 全量、fmt、严格 Clippy、`git diff --check` 均通过。
- 第②缺口收尾：门禁全绿（rtsp 含 3 Basic 服务端 + 3 Digest + 3 客户端 Basic 等 22 网络用例，core 10 Digest 单测，workspace 全量测试通过）；Windows Git diff 仅含 core/auth、rtsp/session、rtsp_digest_auth、core/rtsp Cargo.toml、README、progress/findings 与新测试文件。
- 第③缺口 SRT macOS/Windows 原生构建验证：用户明确不在 Windows 安装 Rust 工具链（占内存），改为 WSL 内交叉 `cargo check`。WSL 已有工具链默认走清华镜像，且已装工具链的 `multirust-channel-manifest.toml` 内嵌镜像绝对 URL，`RUSTUP_DIST_SERVER` 环境变量无效；改为从 USTC 镜像手动下载对应版本 `rust-std-1.96.0-x86_64-pc-windows-{msvc,gnu}` 与 `aarch64-apple-darwin` 并解压进工具链 `lib/rustlib/`。
- Windows MSVC 目标交叉检查被传递依赖 `ring` 的 C 构建卡住（需 MSVC `lib.exe`/cl，Linux 无此工具链）；改用 Windows GNU 目标（`x86_64-pc-windows-gnu`）在 WSL 装 `gcc-mingw-w64-x86-64` 让 ring 的 C 代码用 mingw gcc 编译。
- Windows GNU 交叉检查结果：`cargo check -p zlmediakit-srt --target x86_64-pc-windows-gnu`、`--all-targets` 以及 `cargo check --workspace --target x86_64-pc-windows-gnu` 全部通过——SRT FFI 无 `libc::sockaddr_*` 等 Unix-only 类型（上一轮已改 `socket2::SockAddr`+opaque 指针），整依赖图（含 ring、rustls、tokio、webrtc）在 Windows 目标上类型检查干净。
- macOS 目标（`aarch64-apple-darwin`）交叉检查被 ring 的 C 构建卡住（需 Apple clang/SDK，Linux 无法提供）；该目标由 CI `cross-platform.yml` 的 `platform-check` 在 `macos-latest` 原生 runner 覆盖，本地 WSL 仅验证到类型检查边界。
- 第③缺口结论：无需改动任何源码（SRT 跨平台 FFI 上一轮已就绪），本地新增 Windows-GNU 交叉检查复现 CI 的 Windows `cargo check --workspace --all-targets` 门禁；已清理临时 std 包与 `/tmp` 下载文件、删除误生成的 `nul` 文件。三个候选缺口（HLS AES-128/BYTERANGE、RTSP 服务端 Basic、SRT 跨平台构建验证）全部收尾。

---
