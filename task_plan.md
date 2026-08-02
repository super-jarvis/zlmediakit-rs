# 任务计划：补齐协议实现与协议互转

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
