# 恢复官方 ICMP 代理、Magic DNS、出口节点与公网 IPv6 的方案

基于合并官方 `48ccc56a` 后的 `f7d12596`。目标是缩小与官方的差异：官方已有的功能直接用官方代码，自定义的网段出口只作为接在官方转发逻辑上的一层。

## 结论

- **ICMP 代理**：没有被删除，与官方只差改名和平台补丁，不需要“恢复”，只需把命名改回官方。
- **出口节点**：我们的网段出口在出站选路和出口转发两处都只是官方出口节点的改名加一个按网段选出口的钩子，包头标记是同一个比特位。恢复官方 `exit_nodes` / `enable_exit_node` 后，网段规则可以缩成官方选路函数里约 6 行的插入。
- **Magic DNS**：整块删除，约 3,100 行，全是官方代码。删除时它与当时复杂的 VPN 策略深度耦合；现在 VPN 已简化成“每个网段一个出口”，可以基本原样恢复，只需补回一个约 20 行的启动 DNS 过滤。
- **公网 IPv6**：整块删除，约 3,800 行，全是官方代码。可以原样恢复，需要定一下它和 IPv6 网段规则谁优先。

四项都恢复后，与官方的差异会减少约 7,000 行删除和一批改名；自定义保留的是网段规则、主机路由接管和 socket 保护这些官方没有的部分。

## 现状对照

| 功能 | 官方 | 我们现在 | 恢复方式 |
| --- | --- | --- | --- |
| ICMP 代理 | `icmp_proxy_*`，判断 `enable_exit_node` 与包头 `EXIT_NODE` | 同一套代码，字段名 `vpn_exit`、包头 `VPN_EXIT`；增加 Windows/macOS 丢弃出口 ICMPv6、原生 socket 保护 | 改回官方命名，保留两处补丁 |
| 出口节点（客户端） | `exit_nodes` 有序列表；非组网目的地址按顺序选第一个可达出口；`host_routing.local_exit_node_fallback` 本机兜底 | 删除；同一位置改为 `vpn_engine.resolve()`；兜底删除 | 恢复官方代码，规则作为前置钩子 |
| 出口节点（出口端） | `flags.enable_exit_node`，默认关闭 | `[vpn] exit_enabled`，**默认开启** | 恢复官方开关，默认关闭 |
| Magic DNS | core `gateway/magic_dns`、原生 `instance/dns_server`、`magic_dns.proto`，字段 `accept_dns`(22) / `tld_dns_zone`(31) / `enable_magic_dns`(42) | 删除，字段编号已保留 | 取消保留，原样恢复 |
| 公网 IPv6 | core `peers/public_ipv6`、原生 provider（Linux）、路由信息与管理 API 字段 | 删除，字段编号未被占用 | 原样恢复 |

线上兼容性已经核对：

- 出口标记都是 `PeerManagerHeader` 的 `0b0000_0100`。所以官方客户端用 `exit_nodes` 指向我们的节点、我们的规则指向开了 `enable_exit_node` 的官方节点，现在都能工作。
- 出口节点和公网 IPv6 的 protobuf 字段编号（`Flags.enable_exit_node = 7`、`NetworkConfig` 27/34/62–64、`RoutePeerInfo` 22/24、`PeerFeatureFlag` 11、`api_instance` 中的地址字段）都没有被其他字段占用，可以按原编号恢复。

## 出口和网段规则怎么接到官方转发逻辑上

### 客户端选路

官方 `get_msg_dst_peer_ipv4/ipv6` 的顺序是：组网广播 → 组网地址 → （IPv6：公网 IPv6 网关）→ `exit_nodes` 按序找第一个可达出口 → 本机兜底。

我们现在只把其中“遍历 `exit_nodes`”换成了 `vpn_engine.resolve()`。恢复后保持官方函数原样，只在 `exit_nodes` 循环前插入规则判断：

```rust
// 非组网目的地址
match self.vpn_engine.resolve(&dst) {
    VpnDecision::Exit(peer_id) => { dst_peers.push(peer_id); is_exit_node = true; }
    VpnDecision::Unreachable => return (vec![], false), // 规则出口失联：丢弃，不回退
    VpnDecision::NoMatch => { /* 官方 exit_nodes 循环 + 本机兜底，原样保留 */ }
}
```

语义如下：

- 网段规则优先于 `exit_nodes`。命中规则但出口不可达时丢弃，和现在一样，不会回落到 `exit_nodes`，避免流量从意料之外的出口出去。
- 没有命中规则时，完全是官方行为。只配了 `exit_nodes` 的用户与官方一致。
- 包头标记改回官方的 `set_exit_node`。

### 出口端

ICMP、TCP、UDP 三个代理引擎和 `ProxyRuntimeConfig` 中 `vpn_exit` 全部改回 `enable_exit_node`，这部分与官方的差异归零。保留的自定义部分是：

- Linux 出口端 IPv6 内核转发和 NAT66（官方没有 IPv6 出口转发）。
- Windows/macOS 丢弃出口 ICMPv6，防止未经 NAT 泄漏。

### 主机路由接管

路由接管、DNS 引导和 socket 保护是官方没有的功能，继续保留。接管范围只来自网段规则：官方的 `exit_nodes` 不自动接管系统路由，与官方一致。想要全局走出口时，配置 `0.0.0.0/0` 和 `::/0` 规则即可。

### VPN Portal 客户端

官方在基线提交 `5af90471` 里加了 Portal 客户端继承 `exit_nodes`（`inherit_exit_nodes`）。我们现在用 `follow_network_vpn_policy` 让 Portal 客户端继承网段规则。恢复后两者并存，分别继承各自的配置。

## 逐项恢复要点

### ICMP 代理

本次合并已经接上官方新版的异步原始 socket 和按需打开逻辑，测试也已对齐。剩余工作只有随出口节点一起把命名改回官方。

### 出口节点

- 恢复：配置 `exit_nodes` 与 `flags.enable_exit_node`、管理 API 字段 27/34、CLI `--exit-nodes` / `--enable-exit-node`、Web/GUI 官方表单项、`host_routing.local_exit_node_fallback` 与 `force_exit_node`（官方只在鸿蒙宿主开启，本机自己充当出口）、Portal 继承、官方测试。
- `[vpn]` 只保留 `rules`，`exit_enabled` 删除，出口端改用官方 `enable_exit_node`（默认关闭）。
- 能力标识 `config:vpn-subnet-exit-v1` 保持不变，规则结构没有变化。

### Magic DNS

- 从官方原样恢复 core、原生、proto、配置字段、CLI、Web/GUI 表单，以及移动端 `enable_magic_dns` 的传递。
- 补回 `bootstrap_without_magic_dns`。原因是受保护的启动解析走物理网卡，不能把 `100.100.100.101` 当上游。这是 `795ef999` 删除的约 20 行，连同测试一起恢复。
- 不再恢复当时“按 VPN 策略转发公共 DNS”的那套集成。现在没有阻断和直连规则，Magic DNS 的上游查询按普通流量走：命中规则就经出口，否则直连。这与官方行为和 `vpn-policy.md` 的说明一致。

### 公网 IPv6

- 从官方原样恢复 provider（Linux）、自动接收地址、租约 RPC、路由信息字段、管理 API、虚拟网卡地址更新和官方测试。本次合并时 `three_node.rs` 中官方更新过的公网 IPv6 测试也一起带回。
- 选路顺序按官方：公网 IPv6 网关在出口查找之前。也就是说，节点拿到公网 IPv6 租约后，IPv6 非组网流量走提供者，不再经过 IPv6 网段规则。与官方一致。
- Linux 出口 NAT66 与公网 IPv6 提供者可以共存：前者处理带出口标记的流量，后者处理租约地址的流量。

## 已定

1. **出口开关默认关闭**，与官方一致。`[vpn] exit_enabled` 不再保留；升级时不为已有配置自动开启，出口节点需显式设置 `enable_exit_node = true`。
2. **网段规则可以不写出口。** 不写 `exit` 的规则按官方 `exit_nodes` 顺序选第一个可达出口；写了 `exit` 的规则仍只用该出口，失联时丢弃。
3. **公网 IPv6 网关优先**于 IPv6 网段规则，与官方选路顺序一致。

## 进度

- 出口节点：已完成（`3ffb244a`、`7080f884`）。
- Magic DNS：已完成。恢复官方 core `gateway/magic_dns`、原生 `instance/dns_server`、`runtime_host/magic_dns.rs`、`magic_dns.proto`，以及字段 `accept_dns`(22) / `tld_dns_zone`(31) / `enable_magic_dns`(42)；补回 `bootstrap_without_magic_dns` 及其测试。移动端共享网卡按官方方式传递 Magic DNS 假地址。未恢复当年按 VPN 策略转发 DNS 的 `policy_io`。
- 公网 IPv6：待恢复。
- Magic DNS 验证（Windows）：core 库 1144 项通过；原生 444 项通过，除既有 5 项环境失败外，恢复的 4 项 Magic DNS 测试同样因 wintun 无法创建适配器失败（环境限制），另有 1 项 HTTP3 测试负载下偶发失败、单独复跑通过；Web 后端 364 项通过；前端 18 个测试文件 176 项及 `test-network-config` 通过；proto 字段清单 4 项通过。
- Go 绑定与内嵌 WASM：待全部恢复后统一重新生成。

## 建议顺序

1. 出口节点。改动集中在 core 选路、配置和管理 API，能一次性消掉最多的改名差异，并为后两项提供官方的 `exit_nodes` 基础。
2. Magic DNS。独立模块，原样恢复加启动 DNS 过滤。
3. 公网 IPv6。最大但最独立；需要 Linux 命名空间测试环境（WSL `Ubuntu-Build` 已有）。

每项单独提交，恢复部分尽量与官方文件逐字一致，以便以后合并官方时直接无冲突合入。
