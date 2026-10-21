# Windows 网段出口 VPN 绕过保护修正

用户报告 Windows 配置 `0.0.0.0/0` 后出现流量回环和断网。检查发现，原有保护会直接沿用 socket 的旧接口及源地址所属接口，没有确认它们是否属于捕获 TUN；此外，保护激活与接口信息发布分成了两个步骤。

本次将默认出口和可用接口作为完整快照，在激活保护前准备好，更新时一并发布。TCP、UDP 的具体源地址优先于旧接口选择；旧接口只有仍在默认出口或可用接口集合内才会沿用。系统默认出口允许是其他 VPN 使用的虚拟网卡，此时 EasyTier 的底层连接仍交给原来的 VPN 路径；捕获 TUN 本身不进入可信集合。来源属于被排除接口时返回 `NetworkUnreachable`，让调用方处理连接失败，避免形成封装流量的回环。IPv6 显式作用域及默认出口也经过可信接口校验。

## 自动验证

Windows 命令：

```powershell
cargo test -p easytier --lib --no-default-features --features tun,management,dns-resolver,smoltcp socket_protector::desktop::tests --locked -- --test-threads=1
```

以上 6 项测试通过。覆盖内容包括接管前已建立的 TCP 连接、接管前创建的 TCP/UDP socket、DNS 收发、TCP 重连、IPv4 映射地址、接口选项字节序与还原、其他 VPN 虚拟默认出口，以及排除接口后的旧 TCP/UDP 接口纠正和具体源地址拒绝。

默认路由拆分测试另有 1 项通过；`cargo fmt --all -- --check` 与 `git diff --check` 通过。

WSL Ubuntu-Build 的 `cargo check -p easytier --no-default-features --features tun,management,dns-resolver,smoltcp --locked` 通过。最小 feature 集合保留既有 unused/dead-code 警告。

排除接口用例使用本机真实适配器和 socket 选项，通过从保护快照中移除该适配器模拟 TUN 排除；不会安装捕获路由。该用例需要至少一个具有私有 IPv4 地址的非回环适配器。

## 实测边界

当前执行进程没有 Windows 管理员权限，未进行真实 TUN 全局接管。上述测试验证保护逻辑，不能视为已在本机复现并消除用户报告的 `0.0.0.0/0` 故障。Linux/macOS 的实际网络行为也未在本次实测。

管理员环境应进一步使用真实远端出口节点验证：启用 `0.0.0.0/0` 前后的已有连接、新 UDP/TCP/QUIC 连接、启动 DNS、规则热更新及清空后的联网恢复。抓包应确认 EasyTier 的底层连接使用原生出口接口，TUN 中没有这些连接被反复封装的报文。
