# VPN 策略验证记录（2026-10-14）

验证对应本次统一规则结构、路由保护和平台生命周期修改。所有主机网络修改测试都在新建的 Linux 网络命名空间中执行；没有启用 Windows 主机的全局 VPN 接管。

本文保留当时验证记录。Magic DNS 功能现已移除；下文提到的相关测试结果和 `magic-dns` feature 集合仅描述当时的源码状态，不代表当前仍提供该功能，也不是当前构建建议。

## Core 和配置

- `cargo test --locked -p easytier-core --lib --features proxy-smoltcp-stack vpn -- --nocapture`：56 项通过。覆盖最长前缀、优先级和顺序、重复规则计数、有序出口失效与恢复、不回退覆盖规则、默认路由与 `/1`、内网及组网/子网豁免、窄出口的 DNS/IPv6 保护、远端公告失联保留，以及 no-TUN/地址状态热修改。
- 同配置下的 `gateway::dataplane` 筛选：67 项通过，包含 IPv6 TCP/UDP 实际 ring 收发、地址变化关闭旧连接，以及 IPv4 大包和分片。
- protobuf 删除字段保留编号及名称；Go 两份消息代码重新生成，`go test ./...` 通过。TypeScript 消息代码重新生成，并通过前端类型检查。

## 前端

- 前端库完整单元测试 139 项通过；随后新增的 Direct/Block 导出回归包含在最终配置组件测试中，最终配置组件测试 20 项通过。
- GUI mobile VPN 测试 11 项、类型检查与 production build 通过。
- `pnpm --dir easytier-web/frontend test:vpn-config-responsive`：Chromium 在 360、768、1280 像素宽度验证新增、编辑、排序和删除规则及出口，包含空/重复 CIDR、多条 IPv4/IPv6 出口和打开动作选择框；没有横向溢出或浏览器运行错误。

## Windows 和 Android 构建

- Windows 最终 `cargo check -p easytier --bins` 和 `cargo check --locked -p easytier-gui --lib` 通过。55 项专项测试通过，覆盖 socket 生命周期、TCP/UDP 双栈及 mapped 地址、通配监听、作用域恢复、保护失败和异步清理、DNS 与 WebSocket；其中执行了真实 socket 收发和 UDP DNS 请求/应答。
- 最终 DNS 输入测试 6 项通过，覆盖命名空间 resolver、systemd stub 上游、真实私网/链路本地 DNS 输入与策略豁免。Magic DNS 上游过滤后的空列表回归 1 项、bootstrap 2 项、缓存策略连接 2 项、DNS server 3 项均通过，包含组网域名的实际应答。
- Android 完整 GUI `cargo check --locked --target aarch64-linux-android --manifest-path easytier-gui/src-tauri/Cargo.toml` 通过，使用 NDK 27.2.12479018。Windows 环境以 ASCII 工作区路径构建，并向 bindgen 提供 NDK sysroot、Clang resource headers 及 aarch64 头文件；无需修改 `kcp-sys` 源码。
- Android 插件 `compileDebugKotlin`、`compileDebugUnitTestKotlin` 通过；编译后的 5 项生命周期测试经 `org.junit.runner.JUnitCore` 执行通过，覆盖保留旧接口、安装失败、过期 generation、重复请求和 high-water 保留。临时 standalone Gradle 的 `testDebugUnitTest` 仍在测试类加载阶段失败，报告 `ClassNotFoundException: com.plugin.vpnservice.ExampleUnitTest`，不能记为该 Gradle 任务通过。临时构建入口及 SDK override 已清理。

## Linux 隔离命名空间

WSL `Ubuntu-Build` 内使用真实 `ip`、`ip6tables` 和命名空间执行，环境具有相关网络管理权限。Linux 完整 feature 集合 `tun,management,dns-resolver,magic-dns,websocket,faketcp,upnp,linux-netlink` 的库检查及测试程序编译通过。

- `EASYTIER_RUN_VPN_EXIT_FORWARDING_NETNS_TESTS=1`：`linux_lease_restores_sysctls_and_only_owned_nat_in_fresh_namespaces` 通过，覆盖 forwarding 原值 0、1、混合接口值、共享租约、独立命名空间、前缀变化及已有 NAT 规则保留。forwarding 失败回滚与重试另有平台模拟测试。
- `EASYTIER_RUN_NETNS_GUARD_TESTS=1`：`nested_guards_restore_the_calling_thread_namespace` 通过。命名空间身份及恢复句柄使用当前线程所属命名空间。
- 最终 Linux 测试程序中，4 项 OUTPUT 分区及 IPv6 控制规则测试、路由目的地址和 unreachable 设备匹配测试、8 项出口转发单元测试通过。
- `EASYTIER_RUN_VPN_ROUTE_KERNEL_TESTS=1`：`dedicated_table_capture_and_socket_bypass_survive_exit_outage_and_cleanup` 完整通过。真实 UDP 收发验证 TUN 输入经物理接口转发及回程；全部出口失效后，普通捕获流量及强制绑定物理网卡的未保护 IPv4 DNS、IPv6 UDP 均未到达物理对端。带绕过标记且绑定物理网卡的 DNS 查询/应答和 IPv6 请求/回复通过，IPv6 验证前清空邻居缓存。
- 同一用例验证两个 owner 独立的 IPv4/IPv6 OUTPUT 链及跳转：第一个退出后第二个仍保护，最后退出后两套捕获/拒绝表、规则、链及绕过标记恢复，随后普通 UDP 恢复物理发送。外部路由优先级冲突或没有可用标记时拒绝接管，原 socket 标记保持不变。
- 独立完整 `::/0` 探针中，清空两端邻居缓存后受保护 IPv6 双向收发通过，ICMPv6 135/136 控制规则计数实际命中；未标记 UDP 和 Echo（含 hop limit 255）返回 EPERM，物理对端未收到。
- 上述路由、OUTPUT、forwarding 和命名空间恢复筛选均使用最终源码编译的同一份 Linux 测试程序重跑通过。

## 平台实测边界

macOS 构建当时尝试的 feature 集合为 `tun,magic-dns,smoltcp,management`，停在必需的 `sys-info` C 构建：当前 Windows 主机没有 Apple 目标的 `cc`、Xcode SDK 或 `xcrun`，现有 Zig 也未完成该 Darwin 源文件编译，因此 macOS 构建未通过。该集合仅作为历史记录；当前构建应使用仍然存在的 feature。

尚未进行 macOS 主机实测、Android 设备/模拟器流量测试，以及 Windows 主机全局接管、路由恢复和多网卡硬件切换测试。Linux 验证包含隔离命名空间中的内核路由和真实 socket 收发，不涵盖完整多节点 EasyTier 的长时间断网、打洞及重连设备回归。Core 数据面、Kotlin 生命周期与接口作用域测试不能替代这些设备验证。
