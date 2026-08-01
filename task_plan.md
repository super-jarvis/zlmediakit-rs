# 任务计划：熟悉项目并修复 GitHub Actions

## 目标

理解项目的用途、架构与构建方式，定位并修复当前 GitHub Actions 问题，并使用 WSL 完成本地编译/测试验证。

## 阶段

1. [complete] 梳理项目文档、工作区结构、依赖与 CI 配置
2. [complete] 复现 GitHub Actions 对应的失败
3. [complete] 实施最小且可靠的修复
4. [complete] 在 WSL 中编译并运行相关测试/检查
5. [complete] 总结项目意图、根因、改动与验证结果

## 约束与决策

- 保留用户已有改动；开始时工作区为干净状态。
- 代码发现优先使用 codebase-memory-mcp；本项目未建立索引，因此回退到本地检索。
- shell 命令统一通过 `rtk` 执行。
- CI 修复以本地可复现、改动最小为原则。

## 遇到的错误

| 错误 | 尝试次数 | 解决方案 |
|---|---:|---|
| codebase-memory-mcp 返回 project not found/not indexed | 1 | 按 AGENTS.md 规则回退到本地检索 |
| Windows 环境中 `rtk cat` 无法解析 `cat` | 1 | 后续读取文本改用 `rtk rg -n "^" <file>`，不重复该命令 |
| Web 工具无法直接抓取目标仓库 Actions 页面，搜索也未索引该 Rust 仓库 | 1 | 改用本地 Git remote 与 GitHub CLI/API 获取运行日志 |
| Windows PATH 中未安装 GitHub CLI (`gh`) | 1 | 使用 GitHub REST API（公开仓库无需认证）或 WSL 网络工具查询 |
| 沙箱内运行 `wsl.exe -l -v` 返回 `Wsl/EnumerateDistros/Service/E_ACCESSDENIED` | 1 | 按环境规则以提升权限重试 WSL 只读检查 |
| GitHub job 原始日志 API 对匿名请求返回 403（需要仓库管理员权限） | 1 | 使用公开 annotations + WSL 本地复现，不再请求原始日志 |
| WSL `cargo test --workspace --tests` 链接失败：`unable to find library -lsrt-gnutls` | 1 | 已确认是 CI Linux 根因；workflow 安装 `libsrt-gnutls-dev`，本地安装后继续验证 |
| 修复后完整测试首次验证超过 180 秒工具超时，进程被终止并产生 Broken pipe | 1 | 编译已成功完成并生成缓存；提高超时后重跑，不把终止后的 Broken pipe 当作代码失败 |
| SRT 链接修复后 4 个转码集成测试因找不到 `ffmpeg` 失败 | 1 | CI 显式安装测试所需 `ffmpeg`；WSL 安装后先跑转码测试再跑完整套件 |
| 首次完整 Docker build 超过 600 秒工具超时且 RTK 未返回阶段日志 | 1 | 检查镜像/BuildKit 状态和缓存；用缓存重跑或拆分验证，避免原样重复无诊断构建 |
| WSL Cargo metadata + Python 单行解析因 PowerShell 嵌套引号被错误解析 | 1 | 不重复复杂嵌套命令；改用临时 Docker 容器直接验证 Rust 1.80 能否解析/检查 workspace |
| 一次性 Debian 容器安装依赖受本地 Docker/apt 网络影响超过 180 秒超时 | 1 | 不重复网络安装；采用 Debian 官方包索引作为包名证据，并继续做静态/本地构建验证 |
| Docker registry manifest 查询受本地 Docker Hub 网络影响超时 | 1 | 使用 Docker Hub 官方网页索引确认标签存在 |
| 多次跨 PowerShell/WSL 的复杂 MSRV 汇总命令出现引号解析错误 | 2 | 停止复杂管道；直接读取锁定关键依赖 `time-0.3.54` 的 Cargo.toml 得到明确 MSRV |
| WSL 未安装 Ruby，无法用其标准库解析 YAML | 1 | 检查现有 Python PyYAML；若不可用则以 workflow 差异检查和 GitHub YAML 结构审阅验证 |
