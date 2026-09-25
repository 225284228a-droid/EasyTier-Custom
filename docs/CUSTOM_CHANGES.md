# EasyTier Custom：来源与改动说明

本项目是基于 [EasyTier（ET）](https://github.com/EasyTier/EasyTier) 的独立衍生项目，由本仓库维护者维护。EasyTier 的基础网络能力和原有代码来自上游及其贡献者；以下列出本分支在该基础上增加或调整的行为。此项目并非官方版本，也不承诺这些改动会进入官方主线。

## 上游来源与许可证

- 上游仓库：https://github.com/EasyTier/EasyTier
- 本项目仓库：https://github.com/225284228a-droid/EasyTier-Custom
- 最近已合入的上游基线：`0a783c8e04561d1fee4e3e922e9576402d5bfea3`（2026-09-23）。
- 开源协议与该上游版本一致：GNU Lesser General Public License v3.0（LGPL-3.0），见根目录 [LICENSE](../LICENSE)。保留上游历史、作者归属及第三方组件各自的许可证；本项目修改按同一项目许可证发布。
- 本文对比上述固定基线，不把“尚未合入上游后续提交”当作本项目主动删除的功能。

## 相对原版的主要改动

### 1. 本地 TOML 与去中心化配置管理

- CLI 默认使用 `config.d` 配置目录，保存网络配置，启动时加载配置并恢复启用状态。
- 引入共享的 `InstanceStateStore`，持久化实例启用/停用状态，供 RPC 管理与配置服务器客户端共用。
- 桌面 GUI 普通模式与服务模式共用服务配置目录（默认 `config.d`）和持久化状态；切换模式时停用网络保持停用，旧普通模式目录不再参与运行。
- 离开服务模式时卸载 GUI 安装的系统服务；切入本地模式时，Windows GUI 会按配置目录或 RPC 端口识别并移除冲突的 EasyTier 服务。所有桌面平台都会在 RPC 端口未释放时阻止新服务启动并报错。
- 向魔改控制台暴露本地配置支持能力，避免周期性同步覆盖本地变更。
- 可连接当前官方控制台，使用官方的 `User` / `Web` 配置来源规则：本地用户配置与控制台托管配置分别管理；旧版魔改管理协议不再支持。
- 补齐创建、保存、启用、停用和删除流程；不存在的配置文件允许创建，已存在只读文件仍受保护。
- 运行实例启动时检查实例名称冲突；保存但未启用的配置允许重名。GUI/Web 显示管理失败原因。

主要代码：`easytier-core/src/management/full/`、`easytier/src/core.rs`、`easytier/src/instance/config_storage.rs`、`easytier-gui/src-tauri/src/lib.rs`。

### 2. 多配置服务器与多协议监听

- CLI 的 `--config-server` 和 GUI 支持多个配置服务器地址；每个地址建立独立管理连接。
- Web 服务增加 `--config-server-listeners`，支持重复参数或逗号分隔的监听 URL，可同时使用 TCP、UDP、WS、WSS。
- 旧的 `--config-server-port` / `--config-server-protocol` 作为兼容入口保留；部分监听失败时可继续运行，全部失败才中止。

### 3. WSS / HTTP3 传输与伪装参数

- 新增 HTTP3 隧道实现和协议适配，扩展 WSS 传输，并将 HTTP3 默认监听端口设为 `11014`。
- WSS URL 支持 SNI、Host、请求路径、User-Agent、Accept-Language 和 padding 参数；padding 间隔/长度有边界限制。
- 增加全局 `sni` 配置，用于出站 WSS/HTTP3 连接。
- GUI/Web 编辑 URL 时保留路径、查询参数和 fragment。

相关代码：`easytier/src/tunnel/http3.rs`、`websocket.rs`、`protocol/adapters/`。这里的“伪装”描述传输格式与握手参数，不是不可识别或不可封锁的保证。

### 4. P2P 协议策略与连接管理

- 增加优先、禁用、仅使用 WSS/HTTP3 的 P2P/打洞策略；直连和 TCP 打洞会参考对端能力，HTTP3 UDP 打洞目前要求双方启用严格模式。
- TCP 打洞成功的连接可升级为 WSS；两端均启用 `only_use_wss_http3_for_hole_punching` 且支持 HTTP3 时，UDP 打洞连接可升级为 HTTP3。
- `default_protocol = "udp"` 将已公布的 HTTP3 直连监听器排在 WSS 前，`"tcp"` 则相反；GUI/Web 提供该协议偏好选择。此选项不创建 HTTP3/WSS 监听器，也不把普通 UDP 打洞升级为 HTTP3。Web 默认监听列表不包含 HTTP3/WSS，按需手动添加。
- 严格模式下选择 `udp` 时，已有 WSS 连接仍会继续尝试 HTTP3 打洞；已有 HTTP3 连接后停止重复打洞。选择 `tcp` 时，已有 WSS 连接可满足连接要求。
- 可选 `close_redundant_conns_when_disguised`：伪装连接建立后关闭自动 P2P 建立的普通连接，保留手动配置连接和入站连接。
- 修复 SNI 改写 URL 后连接身份不一致导致的重连堆积，并替换相同 URL 的重复客户端连接。
- `disable_p2p` 调整为半严格行为：拒绝普通节点发起的 TCP/UDP 打洞 RPC，保留声明 `need_p2p` 的节点例外，不主动断开已有连接。

### 5. 可选 BBR

- 增加 `enable_bbr` 开关，用于本地 QUIC/HTTP3 发送端和 QUIC 代理的拥塞控制，默认关闭。
- 该选项不是修改系统 TCP 拥塞控制，也不需要配置最大带宽；关闭时保留 Quinn 的默认控制器。

### 6. 实验性 TCP 对称 NAT 打洞增强

- TCP STUN 增加额外绑定探测，识别端口递增/递减的容易预测的对称 NAT。
- TCP 探测结果未知时借用 UDP NAT 类型作为近似值；这不等同于成功完成 TCP 探测。
- 打洞 RPC 增加端口预测能力协商与预测窗口；对称 NAT 响应端可用多个本地端口并发尝试连接，发起端对预测端口进行受限并发尝试。
- 限制预测数量、尝试窗口和重试行为，处理端口范围、方向及不支持扩展的对端。
- Linux/Android 打洞 socket 使用 `TCP_SYNCNT` 限制 SYN 重传；其他 TCP 连接保留原有行为。
- 具体效果依赖 NAT 映射规则、防火墙和对端版本；不能保证任意对称 NAT 都可打通。

相关代码：`easytier-core/src/connectivity/hole_punch/tcp.rs`、`connectivity/stun/`、`easytier-proto/proto/peer_rpc.proto`、`easytier/src/socket/tcp.rs`。

## 常用新增配置

以下为选项位置和当前默认值，不建议不加区分地全部开启：

| TOML 选项 | 当前默认值 | 作用 |
| --- | --- | --- |
| 顶层 `sni` | 未设置 | 出站 WSS/HTTP3 的全局 SNI |
| `[flags] prefer_wss_http3_for_p2p` | `true` | 优先协商 WSS/HTTP3 |
| `[flags] disable_wss_http3_for_p2p` | `false` | 禁止自动 P2P 使用 WSS/HTTP3 |
| `[flags] only_use_wss_http3_for_hole_punching` | `false` | 限制自动 P2P/打洞使用伪装传输 |
| `[flags] close_redundant_conns_when_disguised` | `false` | 清理自动建立的冗余普通连接 |
| `[flags] enable_bbr` | `false` | 为 QUIC/HTTP3 发送端启用 BBR |

这些策略受本机构建能力和对端能力影响。与原版混用时不要假定扩展功能全部可用；强制仅使用 WSS/HTTP3 可能减少可连接路径。启用互相冲突的策略可能使连接无法建立。

## 构建与验证

沿用仓库的 Rust 工具链和构建方式（`rust-toolchain.toml` 当前指定 Rust 1.95），原有可执行文件名称仍为 EasyTier 系列。构建示例：

```sh
git clone https://github.com/225284228a-droid/EasyTier-Custom.git
cd EasyTier-Custom
cargo build --release --locked -p easytier
```

此前源码发布在 Windows 上执行：

```sh
cargo test -p easytier-core --lib connectivity:: --locked
```

结果：157 个测试通过，0 失败。覆盖连接、打洞与 STUN 单元测试；这不代表所有平台构建、GUI、真实 NAT 网络或完整端到端场景已经验证。本次发布为源码发布，不附带经过发布验证的二进制。

## 审阅完整差异

Git 历史保留了上游提交与本项目提交。克隆后可查看完整差异与变更记录：

```sh
git diff 0a783c8e04561d1fee4e3e922e9576402d5bfea3 HEAD
git log --oneline 0a783c8e04561d1fee4e3e922e9576402d5bfea3..HEAD
```

原版 README 的安装脚本、官方 Web 服务、发布下载、徽章和赞助链接仍指向上游。使用本项目功能请构建本仓库，问题请提交到本项目 Issues。
