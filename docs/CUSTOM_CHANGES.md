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
- 全局 SNI 为空时保留 URL 中显式配置的 SNI；HTTP3 使用 IPv6 地址作为默认服务器名时移除 URL 方括号，避免 TLS 服务器名校验失败。
- GUI/Web 编辑 URL 时保留路径、查询参数和 fragment。

相关代码：`easytier/src/tunnel/http3.rs`、`websocket.rs`、`protocol/adapters/`。这里的“伪装”描述传输格式与握手参数，不是不可识别或不可封锁的保证。HTTP3 隧道使用 TLS 1.3 和 `h3` ALPN，但应用流仍为自定义隧道帧，并非完整的浏览器 HTTP/3 请求；原生 QUIC 打洞的数据包已去除外层 EasyTier UDP 头部，NAT 控制探测包仍沿用现有 EasyTier 格式。

### 4. P2P 协议策略与连接管理

- 增加优先、禁用、仅使用 WSS/HTTP3 的 P2P/打洞策略；直连、TCP 打洞和 UDP 打洞都会参考对端能力。
- TCP 打洞成功的连接可升级为 WSS；双方支持伪装 P2P 时，UDP 打洞会按次协商 HTTP3。严格模式要求 HTTP3，不能降级；优先模式在远端或本机不支持时回退裸 UDP。
- 新版双方通过打洞 RPC 的 `native_http3` 能力协商原生 QUIC，去除 HTTP3 数据外层的 EasyTier UDP 头部。旧魔改节点未返回该能力时仍使用原封装，官方节点的空 scheme 仍表示裸 UDP；SNI、TLS 1.3、`h3` ALPN 和 BBR 沿用原 HTTP3 引擎。
- UDP 打洞监听器按裸 UDP、原生 HTTP3、旧版封装 HTTP3 分池复用，HTTP3 与 WSS 打洞一样使用打洞过程中的自动临时监听端口，不依赖手动公布的 HTTP3/WSS 服务监听器。
- 支持 HTTP3 的节点会在已配置的 UDP 公网监听端口同时接受 HTTP3，并仅公布已就绪的升级端点；自动P2P无需另加 HTTP3 监听或端口。普通 TCP 公网监听不因此变为 WSS。
- `default_protocol = "udp"` 将 HTTP3 排在 WSS 前，`"tcp"` 则相反；GUI/Web 提供该协议偏好选择。对普通 UDP/TCP 打洞，该选项分别对应 HTTP3/WSS 升级偏好。
- 打洞 RPC 的 scheme 字段为空表示官方主线裸 UDP/TCP 协议。官方 peer 不返回新字段时，本分支在优先模式下回退裸 UDP；严格模式不会接受降级连接。
- 已有回退连接仍会继续尝试首选协议；协商伪装且选择 `udp` 时，已有 WSS 连接仍会继续尝试 HTTP3，选择 `tcp` 时则优先 WSS。
- HTTP3 打洞立即进入首次调度；优先模式下 30 秒后允许并行尝试裸 UDP 回退，已有同级或更优连接时停止重复尝试。严格模式不启动裸 UDP 回退。
- 多网卡 HTTP3 拨号在协议握手成功后才选定网卡，避免把尚未验证的 UDP 会话当成可用链路；本机回环地址由系统选择回环路径。
- 可选 `close_redundant_conns_when_disguised`（界面为“首选连接建立后断开多余连接”）：首选连接确认可用后，清理同一对等节点较低优先级的自动P2P连接。协商伪装且选择 UDP 时 HTTP3 可替换 WSS；不主动伪装且选择 UDP 时普通 UDP 可替换 WSS。保留手动连接、普通入站连接、附加连接和同级连接，开关仍默认关闭。
- 新建配置及未指定字段统一默认“不主动使用伪装”和 UDP，GUI/Web 与内核一致；明确保存的伪装策略、TCP/UDP 选择保持不变。旧 TOML 若省略了当时的默认值，加载后采用新的默认值。
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
| `[flags] default_protocol` | `"udp"` | 自动P2P优先 UDP，协商伪装时优先 HTTP3 |
| `[flags] prefer_wss_http3_for_p2p` | `false` | 启用后优先协商 WSS/HTTP3；默认仅响应需要伪装的对端 |
| `[flags] disable_wss_http3_for_p2p` | `false` | 禁止自动 P2P 使用 WSS/HTTP3 |
| `[flags] only_use_wss_http3_for_hole_punching` | `false` | 限制自动 P2P/打洞使用伪装传输 |
| `[flags] close_redundant_conns_when_disguised` | `false` | 首选连接可用后清理较低优先级的自动P2P连接 |
| `[flags] enable_bbr` | `false` | 为 QUIC/HTTP3 发送端启用 BBR |

这些策略受本机构建能力和对端能力影响。与原版混用时不要假定扩展功能全部可用；强制仅使用 WSS/HTTP3 可能减少可连接路径。启用互相冲突的策略可能使连接无法建立。

## 构建与验证

沿用仓库的 Rust 工具链和构建方式（`rust-toolchain.toml` 当前指定 Rust 1.95），原有可执行文件名称仍为 EasyTier 系列。构建示例：

```sh
git clone https://github.com/225284228a-droid/EasyTier-Custom.git
cd EasyTier-Custom
cargo build --release --locked -p easytier
```

本次修改在 Windows 上完成以下验证：

- 核心连接与打洞测试 190 项、配置测试 78 项、监听测试 38 项、连接生命周期测试 11 项，以及 UDP/HTTP3 监听组装测试通过。
- 原生实例测试 9 项通过，覆盖 IPv4/IPv6 下 TCP/WSS 到 HTTP3 的自动升级、同一 UDP 端口双协议收发，以及 QUIC 报文格式和双向数据完整性。
- HTTP3 协议测试 11 项和 SNI 适配测试 6 项通过，覆盖 TLS 1.3、`h3` ALPN、BBR 独立开关和旧版 UDP 封装升级。
- 共享前端测试 28 项、配置导出测试 10 项通过；共享前端、GUI/Web 前端类型检查与构建通过。
- `cargo check --workspace --locked --offline`、`cargo fmt --all --check` 和 `git diff --check` 通过。

同端口性能检查使用本机回环与抓包转发器，对比独立 HTTP3 监听和 UDP 共用监听。双方路由就绪后，每方向发送 256 个数据包，最多保持 64 个未收齐包，避免触及核心现有 128 包主机出站队列的溢出丢包策略；校验逐包内容和完整性。三轮复测均通过，但短时 debug 吞吐存在调度波动，不是公网吞吐、真实 NAT 成功率或跨平台性能保证。本次交付为源码，不附带经过发布验证的安装包。

## 审阅完整差异

Git 历史保留了上游提交与本项目提交。克隆后可查看完整差异与变更记录：

```sh
git diff 0a783c8e04561d1fee4e3e922e9576402d5bfea3 HEAD
git log --oneline 0a783c8e04561d1fee4e3e922e9576402d5bfea3..HEAD
```

原版 README 的安装脚本、官方 Web 服务、发布下载、徽章和赞助链接仍指向上游。使用本项目功能请构建本仓库，问题请提交到本项目 Issues。
