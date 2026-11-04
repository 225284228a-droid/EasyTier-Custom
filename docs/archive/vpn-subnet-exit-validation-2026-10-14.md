# 网段出口 VPN 验证记录（2026-10-14）

## 实现及代码量

规则配置仅含 `cidr` 与 `exit`，管理协议为 `exit_addr`。Linux 每实例一张捕获表，无拒绝兜底表或 OUTPUT 防护链；保留 native socket 绕过及新格式租约回收。

本次手写生产源码净减少 **1,875 个非空行**。统计修改的 Rust、TypeScript、Vue、协议和界面文案，包含注释；排除独立测试、Rust 必须启用 `test` 的模块/辅助项、生成声明和文档。删除测试不计入此数。实现提交 `6cfa74cd` 的完整文本变更为增加 **1,594 行**、删除 **4,874 行**（含测试及文档）；Go 生成代码与产物另行提交，不计入手写生产源码统计。

## 已通过验证

- Windows：`cargo test -p easytier-core vpn --lib --locked`，50 项通过。
- Windows：`cargo check -p easytier --no-default-features --features tun,management,cli,dns-resolver,smoltcp --locked` 通过，含 CLI 与 native TUN 路由代码。
- Linux（WSL Ubuntu-Build）：同一 Core VPN 筛选，50 项通过；`management,proxy-smoltcp-stack,aes-gcm` 下 IPv6 筛选，24 项通过，包含 TCP/UDP 虚拟路径和地址变化关闭旧 generation。
- Linux：`tun,management,dns-resolver,smoltcp` 下路由管理 8 项通过；设置 `EASYTIER_RUN_VPN_ROUTE_KERNEL_TESTS=1` 后，两项 gated kernel 用例实际运行通过。使用独立临时命名空间，覆盖双族捕获、受保护 socket、出口离线、TUN 转发绕过、热更新、多 owner、部分失败、清理重试、死租约回收和命名空间文件锁。
- Linux：设置 `EASYTIER_RUN_VPN_EXIT_FORWARDING_NETNS_TESTS=1`，出口转发/NAT 筛选 9 项通过，含真实命名空间中的 forwarding 原值恢复和只移除自有 NAT 规则。
- 界面库：15 个文件、138 项测试通过；配置转换脚本 10 项检查通过；生产构建通过。
- GUI：移动 VPN 12 项、API 桥接 10 项通过；GUI TypeScript 检查通过。Web API 19 项与 TypeScript 检查通过。
- 浏览器：360、768、1280 像素宽度编辑/删除规则及长 IPv6 地址无横向溢出，无浏览器运行错误。截图位于忽略的 `target/vpn-ui-validation/`。
- 浏览器配置生成器：重新构建 `browser-config` WASM；单出口配置 JSON/TOML 往返和旧字段拒绝检查通过。
- Go（WSL Ubuntu-Build）：`go generate ./internal/artifact` 完成，使用 Binaryen 131 构建并优化 WASI 内核，同步 protobuf 绑定。`go test ./...` 的 13 个含测试包通过，98 个顶层测试通过（含子测试共 142 个通过事件）；外部 Web 管理端 E2E 因未启用环境条件而跳过。
- Go 新增内嵌内核回归通过：IPv4/IPv6 三条规则的单出口状态、不可达状态和捕获路由正确；旧选项/动作、重复网段和非法出口拒绝创建。内嵌 SHA 校验和公开产物/协议来源一致性检查通过。
- `cargo fmt --all -- --check` 与 `git diff --check` 通过。

## 平台边界与迁移

Windows 未进行真实全局 TUN 接管、恢复或多网卡硬件切换；macOS 未进行主机编译/实测；Android 已验证现有 JS 生命周期，但未进行设备/模拟器流量测试。

最小 feature 编译存在既有 unused/dead-code 警告。未将少 feature 集合导致的 Windows UDP broadcast 缺失作为通过结果，正式记录采用包含 `smoltcp` 的上述组合。

升级前正常停止旧实例，并按 `vpn-policy.md` 手动改写旧配置。Linux 同进程多实例、独立命名空间可并存；同命名空间另一活进程的捕获租约会明确报告冲突。

Go 生成绑定与内嵌 WASI 均来自干净源树提交 `6cfa74cd2028665aaf4fe121558160166bade5dd`。产物大小 7,242,229 字节，SHA-256 为 `5029c9bfdbd32111d43d5c08a9e79341ef904614fbd66197d72ba03c7985c321`；协议来源 SHA-256 为 `07847ad232d3d2afc3a2db7d0eb1257857336344e5f59b9c40ce3dfc467be897`。测试日志保存在忽略的 `target/vpn-go-tests.jsonl` 与 `target/vpn-go-tests.stderr.log`。
