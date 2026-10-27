# CI

[English](CI.md)

## 运行入口

两个自动工作流，测试与编译分开：

| 工作流 | 触发与职责 |
| --- | --- |
| **EasyTier CI**（`checks.yml`） | `main/develop` 推送、面向这两个分支的 PR，或手动运行：执行相关检查和测试 |
| **EasyTier Build**（`build.yml`） | `main` 推送或手动运行：编译 Release、打包、上传及清理旧产物 |

**EasyTier Release**、**EasyTier Docker** 继续作为手动入口。草稿 PR 跳过昂贵检查；
PR/develop 按相关路径检查，Go、JavaScript Hosts、Nix 保留独立路径过滤。
两个自动工作流各自取消同分支或 PR 的旧运行，互不取消。

**Build result** 汇总编译结果，**CI result** 汇总检查和测试结果。必选任务失败、
取消或意外跳过，都会让对应汇总失败。编译产物上传及清理独立于测试结果，测试失败
继续显示在 EasyTier CI。

移除定时构建、自动发布和重试、HarmonyOS、Magisk、FreeBSD，以及 Linux GUI 安装包。
对应平台源码保留。

## 平台和公共流程

| 产物 | 目标 / 可下载产物数量 |
| --- | --- |
| Core/CLI/Web | Linux、macOS、Windows，仅 x86-64 / 3 |
| 桌面 GUI | macOS、Windows，仅 x86-64 / 2 |
| Android APK | 仅 ARM64 手机，aarch64（arm64-v8a）/ 1 |

共 **6 份发布产物**。Windows 命令行与 GUI 使用独立任务、独立产物；macOS 使用
Intel runner。Linux x64 包含 mini，验证静态链接和启动；Windows 打包网络驱动并
验证运行库依赖。Linux/Windows 使用实际分发的 Core 和内嵌 Web 做网络创建冒烟测试。

分发产物使用 Release 模式。mini 在 Release 基础上优化体积，Tauri 桌面和 Android
沿用默认 Release，只收集 `release` 目录。默认未签名/未公证；手动 `sign_windows`
启用命令行程序 SignPath 测试签名，`sign_macos` 启用 macOS GUI 签名、公证，需对应 secrets。

编译工作流集中生成 proto、公共库、VPN 插件 API、Web、配置生成器 WASM 和 GUI 资源，
各平台通过当前运行限定的内部缓存复用，不上传前端资源产物。
Tauri CI 配置关闭打包时重复的前端钩子。CI 工作流只准备测试和 Rust
后端所需的前端资源。配置生成器或 Rust 接口有变化时，PR/develop 和手动 CI 另行编译
配置生成器 WASM；main 由生产编译覆盖。每个工作流内公共库只构建一次；前端
单测、CI 脚本单测只在 EasyTier CI 执行。

CI 保留格式、Clippy、WASI、锁定依赖、协议/Web/GUI 后端测试和 Linux 网络集成测试。
main 及手动 CI 检查 feature 组合。PR/develop 和手动 CI 做平台、Android 轻量检查；
main 推送时由 EasyTier Build 正式编译这些目标，省去重复预检查。Linux GUI 后端测试
保留，不生成 Linux GUI 安装包。网络测试只编译一次，沿用三个 nextest 分组和既有
独占/并发调度。Go 测试与 proto 一致性检查共用环境；Nix 省去重复的预检查。

## 下载、旧产物清理和手动发布

发布产物保留七天。6 个文件均使用 `actions/upload-artifact@v7`、`archive: false`
直接上传，下载时不额外套 ZIP：

| 下载文件 | 内容 |
| --- | --- |
| `easytier-linux-x86_64.tar.gz` | 命令行程序、提交记录及校验和 |
| `easytier-macos-x86_64.tar.gz` | 命令行程序、提交记录及校验和 |
| `easytier-windows-x86_64.zip` | 命令行程序、网络驱动、提交记录及校验和 |
| `easytier-gui-windows-x86_64.exe` | Windows GUI 安装程序 |
| `easytier-gui-macos-x86_64.dmg` | macOS GUI 安装程序 |
| `easytier-mobile-android-aarch64.apk` | ARM64 Android 安装程序 |

命令行包只需解压一次，tar 保留 Unix 可执行权限。EXE、DMG、APK 下载即为安装文件。
Web 面板和配置生成器内嵌在命令行包的 `easytier-web-embed` 中，不另提供 Web 包，
也不构建独立的 `easytier-web` 二进制。
每个文件的 SHA-256 和源提交记录显示在任务摘要中；手动 Release 根据 GitHub 记录
的产物摘要校验下载字节，直接复用原文件，另生成统一 `COMMIT.txt`、`SHA256SUMS`。

前端资源和网络测试归档通过当前 run 的精确缓存传递，不回退到其他构建。新运行
成功后清理同工作流、同分支较早且已结束的资源缓存；保留当前缓存供重试任务使用，
并遵循 GitHub 缓存回收策略。缓存缺失或 fork PR 只有缓存读权限时，在使用资源的
任务中本地重建，继续执行测试。下载产物列表只显示分发文件。

新的主线编译成功后，先验证 **6 份新产物** 全部存在、非空、未过期且来源正确，
再删除较早且已完成的主线构建产物。也清理已退休 Core/GUI/Mobile/Custom Static/OHOS
工作流遗留的主线产物，包括旧架构包。保留正在运行、更新、PR、develop、无关及
来源不明的运行。新编译失败或产物不完整时，保留上一份可用产物。不会删除 GitHub
Release 附件；最新成功产物仍受七天有效期限制。

版本发布时手动运行 **EasyTier Release**，填写一个成功全量 `build_run_id`、`version`
和 `make_latest`。校验本仓库的 6 份产物完整有效后，只下载这些文件，添加统一提交
记录和校验和，并在构建提交上创建发布草稿；不重新编译、不自动发布。需要选择
尚未清理或过期的构建。

Docker 保留现有手动上游镜像流程：run ID 指向 `EasyTier/EasyTier` 的 Core 构建，
执行者与镜像仓库配置沿用现状。

## 本地验证

执行 `actionlint -no-color` 和 `pnpm test:ci-scripts`。前端 CI 执行
`pnpm build:frontend:check` 后执行 `pnpm test:frontend:ci`。
`pnpm build:frontend:ci` 另验证完整生产前端及配置生成器 WASM，需要 Rust WASM target
和 wasm-pack。全平台编译及需要特权的网络测试以 GitHub runner 实际运行为准。
