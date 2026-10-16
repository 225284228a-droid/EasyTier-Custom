# CI

[English](CI.md)

## 运行入口

保留三个工作流：**EasyTier Build**、**EasyTier Release**、**EasyTier Docker**。
Build 是唯一自动运行入口。

| 触发方式 | 执行内容 |
| --- | --- |
| 推送到 `main` | 全平台编译、检查，上传可下载的 Actions 产物 |
| 推送到 `develop`，或目标为 `main/develop` 的 PR | 按相关路径检查，不生成发布二进制和安装包 |
| 任意分支手动运行 EasyTier Build | 全平台编译及全部检查 |
| 草稿 PR | 跳过构建和测试，保留结果汇总 |

准备任务统一计算路径过滤与构建矩阵。同分支或 PR、同模式的新运行会取消旧运行，
检查模式与全量模式分别控制并发。Go、JavaScript Hosts、Nix 继续按各自路径运行。
唯一汇总检查是 **Build result**（`build-result`），必选任务失败、取消或意外跳过
都会导致汇总失败。

已移除定时构建、自动发布、HarmonyOS/Magisk 构建及 Linux GUI 安装包。
对应源码目录继续保留。

## 平台和公共流程

| 产物 | 目标 |
| --- | --- |
| Core/CLI/Web | 16 个：Linux x64、ARM64、RISC-V、LoongArch、ARMv7 硬/软浮点、ARM 硬/软浮点、MIPS/MIPSEL；FreeBSD x64；macOS Intel/ARM64；Windows x64/x86/ARM64 |
| 桌面 GUI | 5 个：macOS Intel/ARM64，Windows x64/x86/ARM64 |
| Android APK | 仅 ARM64 手机：aarch64（arm64-v8a） |

Windows 命令行与 GUI 使用独立任务、独立产物。MIPS 沿用现有 Core/CLI 内容。
Linux x64 包含 mini，并检查静态链接和程序启动。Windows 检查运行库依赖并打包
网络驱动。Linux/Windows x64 使用实际分发的 Core 和内嵌 Web 运行网络创建冒烟测试。

公共前端任务统一生成 proto，构建公共库、VPN 插件 API、Web、配置生成器、GUI，
并执行前端单元测试。其他任务下载公共资源复用。CI 专用 Tauri 配置关闭重复的
前端构建钩子，本地开发命令保持原有行为。环境准备只安装工具和依赖。

CI 脚本单测只执行一次。Rust 检查保留格式、Clippy、全量模式的 feature 组合、
WASI、锁定依赖、协议/Web/GUI 后端测试及 Linux 网络集成测试。与正式编译重复
的平台预检查仅在检查模式运行。GUI 后端继续在 Linux 上进行无界面测试。
网络测试只编译一次，沿用三个互斥的 nextest 分组和原有独占/并发调度。
Go 测试和生成代码一致性检查共用一次环境安装；Nix 直接构建，省去重复预检查。

桌面/命令行默认生成未签名/未公证产物。手动 `sign_windows` 启用命令行程序的 SignPath
测试签名，`sign_macos` 启用 macOS GUI 签名和公证，需配置对应仓库 secrets。

## 下载、旧产物清理及手动发布

发布产物保留七天。命令行产物名为 `easytier-<平台>`，包含 Unix `.tar.gz` 或
Windows `.zip` 成品包；tar 保留可执行权限。GUI 产物为 `easytier-gui-<平台>`，
APK 为 `easytier-mobile-android-<架构>`，原生安装包保留原始文件。
二进制/安装包附带 `COMMIT.txt` 和 `SHA256SUMS`，校验覆盖随包驱动。
`easytier-web-dashboard` 包含前端 tar 成品包。内部复用的 `frontend-assets` 和
`tests` 保留一天，不进入版本发布。

新的主线全量编译及所有必选检查成功后，清理较早且已结束的主线构建产物，
也清理旧 Core/GUI/Mobile/Custom Static/OHOS 工作流遗留的主线产物。
保留正在运行、更新、PR、develop、无关工作流及来源不明的产物；新构建失败时
保留上一份可用产物。不会删除 GitHub Release 附件。通常只保留最新一轮成功
主线构建的下载内容，仍受七天有效期限制。

版本发布时，手动运行 **EasyTier Release**，填写一个 `build_run_id`、`version`
和 `make_latest`。构建必须来自本仓库成功的 EasyTier Build 全量运行，23 个预期
发布产物必须齐全且未过期。Release 只下载通过校验的产物，直接复用压缩包和原生
安装包，添加统一提交记录与校验和，并在构建提交上创建发布草稿，不重新编译或
自动发布。需选择尚未被清理或过期的构建。

Docker 保留现有手动上游镜像流程：run ID 仍指向 `EasyTier/EasyTier` 的 Core
构建，执行者和镜像仓库配置沿用现状。

## 本地验证

执行 `actionlint -no-color`、`pnpm test:ci-scripts`、`pnpm test:unit`。
`pnpm build:frontend:ci` 额外验证公共生产前端构建，需要 Rust WASM target 和
wasm-pack。全平台编译及需要特权的 Linux 网络测试在 GitHub runner 上验证。
