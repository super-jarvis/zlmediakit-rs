# 进度日志

## 2026-08-02

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
