# EasyTier Custom：来源与改动说明

本项目是基于 [EasyTier（ET）](https://github.com/EasyTier/EasyTier) 的独立衍生项目，由本仓库维护者维护。EasyTier 的基础网络能力和原有代码来自上游及其贡献者；以下列出本分支在该基础上增加或调整的行为。此项目并非官方版本，也不承诺这些改动会进入官方主线。

## 上游来源与许可证

- 上游仓库：https://github.com/EasyTier/EasyTier
- 本项目仓库：https://github.com/225284228a-droid/EasyTier-Custom
- 最近已合入的上游基线：`4837468d43b6fe48865b2ce926312b66afd10bef`（固定合入基线）。
- 开源协议与该上游版本一致：GNU Lesser General Public License v3.0（LGPL-3.0），见根目录 [LICENSE](../LICENSE)。保留上游历史、作者归属及第三方组件各自的许可证；本项目修改按同一项目许可证发布。
- 本文对比上述固定基线，不把“尚未合入上游后续提交”当作本项目主动删除的功能。

## 相对原版的主要改动

### 1. 本地 TOML 与去中心化配置管理

- CLI 默认使用 `config.d` 配置目录，保存网络配置，启动时加载配置并恢复启用状态。
- 引入共享的 `InstanceStateStore`，持久化实例启用/停用状态，供 RPC 管理与配置服务器客户端共用。
- 桌面 GUI 普通模式与服务模式共用服务配置目录（默认 `config.d`）和持久化状态；切换模式时停用网络保持停用，旧普通模式目录不再参与运行。
- 离开服务模式时卸载 GUI 安装的系统服务；切入本地模式时，Windows GUI 会按配置目录或 RPC 端口识别并移除冲突的 EasyTier 服务。所有桌面平台都会在 RPC 端口未释放时阻止新服务启动并报错。
- 心跳和实例列表按实际构建与 Host 能力声明管理、配置扩展及传输能力；缺少能力的节点不收到扩展字段，包括 `false` 或清空值。
- 官方节点继续使用官方 `User` / `Web` 来源与中央编排流程。定制节点的本地 TOML 是配置权威；网页数据库只保存独立的只读镜像，不用于恢复或离线下发。旧定制节点保留原有单节点入口；若旧接口无法保留未声明能力的既有扩展配置，会保留编辑草稿并提示升级。
- 补齐创建、保存、启用、停用和删除流程；不存在的配置文件允许创建，已存在只读文件仍受保护。
- 运行实例启动时检查实例名称冲突；保存但未启用的配置允许重名。GUI/Web 显示管理失败原因。

- 一个 Core 进程共享配置目录快照，启动、每 2 秒扫描和管理操作完成通知使用同一观察器。手改、新增、删除、非法文件和重复身份更新目录状态，不自动改变实例启停。
- `PersistedConfigService.ObserveConfigs` 返回进程 epoch、目录 generation、逐项 revision、磁盘与运行版本、权限、启停状态，以及允许读取的原始 TOML。
- `PatchConfig` 使用实例 ID、预期 revision 与字段 mask 执行 CAS；从最新原始 TOML 的语法树修改指定字段，保留未选字段、注释及尚未应用的磁盘修改。默认保存并应用，停用实例保持停用，另支持只保存。
- revision 覆盖内容、存在状态、权限、启停和运行代际；启停与删除也支持版本校验。取消请求不会释放仍在执行的实际写入任务锁；失败回滚先检查当前文件是否仍属于该次写入。
- 两个独立服务器各自读取节点快照并事务更新镜像，按账号、设备、会话归属、epoch 和 generation 拒绝迟到响应。断线保留最后镜像并标记过期，重连刷新，另每 30 秒完整复查。
- 新批量入口仅在线操作，每个实例单独报告结果并保留成功项；冲突保留表单输入。超时后重新观察，不自动重发写入；首次能力未知和旧节点不开启新批量写入。
- 受保护配置只暴露允许公开的元数据；不镜像展开的环境变量内容。外部手工编辑器不参与 Core 的进程锁：能拒绝校验前已发生的修改，不承诺跨程序文件系统事务。

主要代码：`easytier-core/src/management/full/`、`easytier/src/core.rs`、`easytier/src/instance/config_storage.rs`、`easytier-gui/src-tauri/src/lib.rs`。

### 2. 多配置服务器与多协议监听

- CLI 的 `--config-server` 和 GUI 支持多个配置服务器地址；每个地址建立独立管理连接。
- Web 服务增加 `--config-server-listeners`，支持重复参数或逗号分隔的监听 URL，可同时使用 TCP、UDP、WS、WSS。
- 旧的 `--config-server-port` / `--config-server-protocol` 作为兼容入口保留；部分监听失败时可继续运行，全部失败才中止。

### 3. WSS / HTTP3 传输与伪装参数

- 新增 HTTP3 隧道实现和协议适配，扩展 WSS 传输，并将 HTTP3 默认监听端口设为 `11014`。
- WSS URL 支持 SNI、Host、请求路径、User-Agent、Accept-Language 和 padding 参数；padding 间隔/长度有边界限制。文本 padding 通过真实 HTTP Upgrade 请求与响应的 `X-EasyTier-Transport-Features: ws-text-padding-v1` 协商，双方声明后才启用；官方、旧节点及缺少声明的 JS/WASI 入口保持 binary。
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
- 可选 `close_redundant_conns_when_disguised`（界面为“首选连接建立后断开多余连接”）：首选连接确认可用后，清理同一对等节点较低优先级的自动P2P连接，包含 TCP/UDP 打洞的客户端和服务端。协商伪装且选择 UDP 时 HTTP3 可替换 WSS；不主动伪装且选择 UDP 时普通 UDP 可替换 WSS。保留手动连接、普通入站连接、附加连接和同级连接，开关仍默认关闭。
- 新建配置及省略字段的旧配置统一默认“不主动使用伪装”和 TCP，GUI/Web 与内核一致；明确保存的伪装策略、TCP/UDP 选择保持不变。
- 原来省略 `default_protocol`、依赖旧 UDP 默认值的配置，升级后采用 TCP；需要继续优先 UDP 时，在 `[flags]` 显式保存 `default_protocol = "udp"`。
- 修复 SNI 改写 URL 后连接身份不一致导致的重连堆积，并替换相同 URL 的重复客户端连接。
- `disable_p2p` 调整为半严格行为：拒绝普通节点发起的 TCP/UDP 打洞 RPC，保留声明 `need_p2p` 的节点例外，不主动断开已有连接。

- 连接来源收敛为官方六类 `PeerConnectionOrigin`，由来源派生打洞保活和清理资格；TCP→WSS 保留 TCP 打洞来源，UDP→HTTP3 保留 UDP 打洞来源。
- TCP 打洞 Ping 最大间隔采用官方 1 秒限制，保留带宽 Ping/Pong 扩展；清理队列在入队与出队时复核开关、协议偏好和可用替代连接，关闭网络连接发生在锁外。
- 修复升序排序后从末尾消费导致的拨号偏好颠倒，并验证实际消费顺序。

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

### 7. 流量、带宽与地理展示

- 保留带宽 Ping/Pong 扩展、双向流量统计、城市缓存及地球展示，组合到官方导航、主题、设备和网络页面。
- 带宽窗口与估计值的中性类型放在 `easytier-core/src/foundation/bandwidth.rs`，socket 与 tunnel 共用基础类型；ETBW 线格式编解码仍由 tunnel 负责。
- 地球为独立 `NetworkTopologyGlobe.vue` 组件，采集使用共享 `useMeshTopology.ts`；展示合并官方别名、网络归属、NAT、丢包率和 VPN Portal，采集与在线数量过滤离线设备。
- Web 持久化按实际绝对 API 地址和账号隔离；后台刷新不会覆盖未保存的表单输入，目录版本变化时提示重新读取。

详细流量说明见 [network-telemetry.md](network-telemetry.md)。

## 常用新增配置

以下为选项位置和当前默认值，不建议不加区分地全部开启：

| TOML 选项 | 当前默认值 | 作用 |
| --- | --- | --- |
| 顶层 `sni` | 未设置 | 出站 WSS/HTTP3 的全局 SNI |
| `[flags] default_protocol` | `"tcp"` | 自动P2P优先 TCP，协商伪装时优先 WSS |
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

- Core 默认构建的 957 项库测试通过，覆盖实际拨号顺序、默认 TCP 与显式 UDP、连接来源与清理、打洞协商、DHCP、安全中继和 VPN Portal。
- 最小 feature profile（关闭默认特性，仅 `test-utils,proxy-cidr-monitor`）测试通过。
- Web 后端完整默认测试运行通过：321 项通过、1 项既有容量压力测试默认忽略；版本冲突、旧会话响应、离线保护及官方中央编排回归包含在本次运行中。
- 原生持久化配置事务测试 20 项通过，覆盖停用状态、仅保存后热修改、取消中的写入、外部修改后的拒绝及回滚保护；双服务器镜像与版本校验测试 9 项通过，包括两个独立数据库同时修改同一版本和手改后 10 秒内刷新。
- WS/WSS 协商测试 20 项通过，覆盖实际 Upgrade 请求与响应、缺少能力头和未知 token 的 binary 回退、配置服务器首个 RPC；原生 QUIC 的 BBR 开关四种组合通过。
- 使用固定官方 `4837468d` 源码独立构建的真实进程执行 5 项互通测试，全部通过：WS/WSS × 双向 × 两种 padding 配置的 8 组 peer 连接、2 组管理连接及反向 RPC、2 组官方端拒绝未协商文本 padding 的探针。回环测试显式关闭设备绑定，不修改产品的设备绑定策略。
- 共享前端最终完整运行 31 个测试文件、273 项测试及 GUI 前端 24 项测试通过；配置导出测试 10 项通过，共享前端与 GUI/Web 的类型检查和生产构建通过。
- 官方控制台浏览器场景和定制地球/流量完整浏览器验收通过。地球使用本机 Intel GPU 的 ANGLE D3D11，覆盖桌面和手机尺寸、10m 高精度地图、双向流量/RTT、暂停与拖拽、缩放和标签布局；SwiftShader 软件渲染会显著拖慢地图读取，不据此降低地图精度或验收断言。可用 `CHROMIUM_ANGLE_BACKEND=d3d11` 选择同一验收路径。
- GUI Rust 检查与原生可执行文件构建通过；`cargo fmt --all -- --check` 和 `git diff --check` 通过。
- `cargo clippy -p easytier-core -p easytier -p easytier-web --all-targets --locked --offline` 通过；Windows 构建仍有既有测试辅助代码的未使用警告，本条不表示全部 CI 平台的检查结果。

原生完整测试另运行了 291 项：272 项通过、11 项失败、8 项默认忽略。失败涉及 Windows 管理员防火墙操作、Wintun DLL 签名/监听权限、外网 DNS 返回数量、指定回环接口及 socket 绑定/UDPv6 条件；不能把本机未满足的这些测试条件记为通过。HTTP3/WSS 实际升级、IPv4/IPv6 数据通道、BBR 和持久化配置事务在该次运行中通过。默认忽略项含需单独提供官方进程的 5 项互通测试，以及防火墙与约 16 GB 压力测试。

既有同端口性能检查使用本机回环与抓包转发器，对比独立 HTTP3 监听和 UDP 共用监听。双方路由就绪后，每方向发送 256 个数据包，最多保持 64 个未收齐包，避免触及核心现有 128 包主机出站队列的溢出丢包策略；校验逐包内容和完整性。三轮复测均通过，但短时 debug 吞吐存在调度波动。上述验证不代表公网吞吐、真实 NAT 成功率或全部平台组合；本轮不发布安装包。

## 审阅完整差异

Git 历史保留了上游提交与本项目提交。克隆后可查看完整差异与变更记录：

```sh
git diff 0a783c8e04561d1fee4e3e922e9576402d5bfea3 HEAD
git log --oneline 0a783c8e04561d1fee4e3e922e9576402d5bfea3..HEAD
```

原版 README 的安装脚本、官方 Web 服务、发布下载、徽章和赞助链接仍指向上游。使用本项目功能请构建本仓库，问题请提交到本项目 Issues。
