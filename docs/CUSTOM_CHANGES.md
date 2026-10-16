# EasyTier Custom：来源与改动说明

本项目是基于 [EasyTier（ET）](https://github.com/EasyTier/EasyTier) 的独立衍生项目，由本仓库维护者维护。EasyTier 的基础网络能力和原有代码来自上游及其贡献者；以下列出本分支在该基础上增加或调整的行为。此项目并非官方版本，也不承诺这些改动会进入官方主线。

## 上游来源与许可证

- 上游仓库：https://github.com/EasyTier/EasyTier
- 本项目仓库：https://github.com/225284228a-droid/EasyTier-Custom
- 最近已合入的上游基线：`99db92a3aeb3109efe1ace5a4a3e2a1bf3840ed7`（2026-10-16 合入）。
- 开源协议与该上游版本一致：GNU Lesser General Public License v3.0（LGPL-3.0），见根目录 [LICENSE](../LICENSE)。保留上游历史、作者归属及第三方组件各自的许可证；本项目修改按同一项目许可证发布。
- 本文对比上述固定基线，不把“尚未合入上游后续提交”当作本项目主动删除的功能。

## 相对原版的主要改动

### 定制 protobuf 字段编号与版本兼容

- 本次合并中，所有原先使用低位编号的 Custom protobuf 字段均迁移到高位：新编号为原 tag 加 `50000`；若该编号已被占用，则每次再加 `1000`，直到找到空闲编号。已有高位编号保持不变。
- 本轮共迁移 100 个低位字段，保留 9 个已有高位字段；完整旧/新编号见 [Custom 字段清单](../easytier-proto/tests/custom-fields.json)，官方字段编号、类型和保留状态见 [官方 schema 快照](../easytier-proto/tests/upstream-fields.json)。descriptor 测试与 CI 会检查清单完整性和编号分配。
- 不保留旧低位编号的双读写兼容路径。使用这些扩展字段通信的定制 Core、Web、GUI、Go 和 JS 组件必须同步升级到对应版本；新版本不再识别旧定制 tag，混用时定制字段无法正确传递。下文的能力降级与旧接口说明不包含迁移前的定制协议版本。
- 此迁移只调整 protobuf 线上的字段编号；对应 TOML、CLI 和 JSON 字段名保持不变。

### 本轮官方功能适配

- 官方 IPv4 DHCP 子网、IPv6 DHCP 和 IPv6 DHCP 子网沿用官方字段 74、75、76；配置表单、TOML 转换和带 revision/CAS 的字段级保存同时支持这些设置，并保留未编辑的 TOML 内容及定制策略。
- 共享网卡按成员管理地址、路由和生命周期，成员退出时等待其路由清理完成。重叠的地址或路由 claims 会被拒绝以维持网络隔离；同地址族的全隧道 `/0` VPN 捕获路由因此不能与其他网络同时共享一个 TUN。
- 桌面 VPN 捕获路由在写入系统前先登记到共享网卡，参与目的地址分发和跨成员冲突检查；系统路由清理成功后才释放归属，失败重试期间继续保留隔离。失效网卡仍执行系统路由清理；清理持续失败时关闭共享网卡的数据分发，防止残留捕获路由把流量送入其他网络。此登记不重复安装系统路由。
- 移动端多网络配置保留 generation 和 candidate 确认流程，过期安装、停止和回滚操作不能拆除新一代连接；同一代失败重试会重新安装已回滚的成员。
- 移动端等待重配置确认后再次核对 generation，旧候选的迟到拒绝不会清空新连接状态。仅修改 DHCP IPv4 子网时保留未选中的静态 IPv4 TOML 字段。
- 合入官方 UDP 打洞与手动路由热更新，并保留本版 WSS/HTTP3 能力协商、TOML 权威和停用状态。Magic DNS 及此前删除的旧出口节点、public IPv6 配置功能仍保持删除。

### 1. 本地 TOML 与去中心化配置管理

- CLI 默认使用 `config.d` 配置目录，保存网络配置，启动时加载配置并恢复启用状态。
- 保留官方每实例运行时，`--state-dir` / `ET_STATE_DIR` 控制机器身份的状态目录；默认配置目录、停用状态及多服务器管理继续采用上述定制行为。
- 引入共享的 `InstanceStateStore`，持久化实例启用/停用状态，供 RPC 管理与配置服务器客户端共用。
- 桌面 GUI 普通模式与服务模式共用服务配置目录（默认 `config.d`）和持久化状态；切换模式时停用网络保持停用，旧普通模式目录不再参与运行。GUI 普通模式恢复目录与 CLI 启动共用同一套隔离策略（`easytier::instance::config_dir_startup`）：非法文件、重复 UUID 与无法启动的条目逐项跳过，不阻塞其他网络加载。
- 离开服务模式时卸载 GUI 安装的系统服务；切入本地模式时，Windows GUI 会按配置目录或 RPC 端口识别并移除冲突的 EasyTier 服务。所有桌面平台都会在 RPC 端口未释放时阻止新服务启动并报错。
- 心跳和实例列表按实际构建与 Host 能力声明管理、配置扩展及传输能力；缺少能力的节点不收到扩展字段，包括 `false` 或清空值。
- 官方节点继续使用官方 `User` / `Web` 来源与中央编排流程。定制节点的本地 TOML 是配置权威；网页数据库只保存独立的只读镜像，不用于恢复或离线下发。旧定制节点保留原有单节点入口；若旧接口无法保留未声明能力的既有扩展配置，会保留编辑草稿并提示升级。
- 补齐创建、保存、启用、停用和删除流程；不存在的配置文件允许创建，已存在只读文件仍受保护。
- 运行实例启动时检查实例名称冲突；保存但未启用的配置允许重名。GUI/Web 显示管理失败原因。

- 一个 Core 进程共享配置目录快照，启动、文件系统通知、每 30 秒兜底扫描和管理操作完成通知使用同一观察器。原生内核自动应用本地修改，手动放入的新配置默认启用，手动删除配置会停止对应实例；已有的手动停用状态会保留。发现和删除同时更新 `easytier-state.json`，包括停机期间删除的启用、停用记录；目录未变化时不重复写入状态文件。
- 网页控制台通过自动同步的只读镜像展示配置，进入网络编辑时读取编辑基线，打开文件编辑器时复用同一基线与表单；重复打开不再强制刷新或替换未保存编辑的版本。保存时仍即时校验磁盘 revision，冲突后保留草稿，并提示先复制修改再刷新页面加载。
- 启动时隔离配置目录中无法恢复的文件和重复 UUID，其他配置继续加载；无法确认身份的文件不会触发停用状态清理，也不会意外创建默认网络。有效的只读配置、环境变量配置和旧无 UUID 配置保留原生加载行为；显式 `--config-file` 无法读取时仍会中止启动。
- 本地 TOML 启动加载可恢复能定位的格式、类型和字段错误：保留同一文件中的其他有效字段及数组项，记录告警后继续加载。管理 API、配置编辑、运行中的文件自动重载和显式校验仍使用严格解析与校验；无效的外部修改保留原有运行实例，不会意外启动默认网络或替换安全身份。
- `PersistedConfigService.ObserveConfigs` 返回进程 epoch、目录 generation、逐项 revision、磁盘与运行版本、权限、启停状态，以及允许读取的原始 TOML。
- `PatchConfig` 使用实例 ID、预期 revision 与字段 mask 执行 CAS；从最新原始 TOML 的语法树修改指定字段，保留未选字段、注释及尚未应用的磁盘修改。默认保存并应用，停用实例保持停用，另支持只保存。
- revision 覆盖内容、存在状态、权限、启停和运行代际；启停与删除也支持版本校验。取消请求不会释放仍在执行的实际写入任务锁；失败回滚先检查当前文件是否仍属于该次写入。
- 省略 revision 的旧启停、删除接口同样检查目录中的重复 UUID；批量操作保留冲突项并继续处理合法项，不改变官方无本地目录的管理流程。
- 声明 `management:persisted-config-apply-v1` 的节点支持对匹配 revision 发送 `SaveAndApply` 空字段 mask，应用完整的已保存 TOML，不改写磁盘原文；停用实例不会因此启动。GUI/Web 提供“应用已保存配置”，缺少新能力的节点不收到该请求；只保存后的再次运行、重新打开编辑器和手改配置后均可应用。
- 两个独立服务器各自读取节点快照并事务更新镜像，按账号、设备、会话归属、epoch 和 generation 拒绝迟到响应。断线保留最后镜像并标记过期，重连刷新，另每 30 秒完整复查。
- Web／GUI 统一通过普通设备／网络管理逐项操作配置，不再提供独立的“本地配置”总览和批量编辑页面。保留单设备读写、仅保存、保存并应用及版本冲突保护；Web 快照缓存与后台同步继续保留，超时后重新观察，不自动重发写入。
- 受保护配置只暴露允许公开的元数据；不镜像展开的环境变量内容。外部手工编辑器不参与 Core 的进程锁：能拒绝校验前已发生的修改，不承诺跨程序文件系统事务。

主要代码：`easytier-core/src/management/full/`、`easytier/src/core.rs`、`easytier/src/instance/config_storage.rs`、`easytier-gui/src-tauri/src/lib.rs`。

### 2. 多配置服务器与多协议监听

- CLI 的 `--config-server` 和 GUI 支持多个配置服务器地址；每个地址建立独立管理连接。
- Web 服务增加 `--config-server-listeners`，支持重复参数或逗号分隔的监听 URL，可同时使用 TCP、UDP、WS、WSS。
- 旧的 `--config-server-port` / `--config-server-protocol` 作为兼容入口保留；部分监听失败时可继续运行，全部失败才中止。
- 控制台接入命令使用配置顺序中第一个成功绑定的监听协议和实际端口，包含动态分配端口；失败的监听地址不会被用于接入提示。

### 3. WSS / HTTP3 传输与伪装参数

- 新增 HTTP3 隧道实现和协议适配，扩展 WSS 传输，并将 HTTP3 默认监听端口设为 `11014`。
- WSS URL 支持 SNI、Host、请求路径、User-Agent、Accept-Language 和 padding 参数；padding 间隔/长度有边界限制。文本 padding 通过真实 HTTP Upgrade 请求与响应的 `X-EasyTier-Transport-Features: ws-text-padding-v1` 协商，双方声明后才启用；padding 双向生效——监听 URL 配置了 `?padding=` 且对端在 Upgrade 请求中声明该能力时，服务端出站帧同样填充；官方、旧节点及缺少声明的 JS/WASI 入口保持 binary。
- 增加全局 `sni` 配置，用于出站 WSS/HTTP3 连接。
- 全局 SNI 为空时保留 URL 中显式配置的 SNI；HTTP3 使用 IPv6 地址作为默认服务器名时移除 URL 方括号，避免 TLS 服务器名校验失败。WSS 伪装参数生效时，请求行中的 IPv6 字面量主机同样保持完整（含方括号），不会被截断或改写。
- GUI/Web 编辑 URL 时保留路径、查询参数和 fragment。

相关代码：`easytier/src/tunnel/http3.rs`、`websocket.rs`、`protocol/adapters/`。这里的“伪装”描述传输格式与握手参数，不是不可识别或不可封锁的保证。HTTP3 隧道使用 TLS 1.3 和 `h3` ALPN，但应用流仍为自定义隧道帧，并非完整的浏览器 HTTP/3 请求；原生 QUIC 打洞的数据包已去除外层 EasyTier UDP 头部，NAT 控制探测包仍沿用现有 EasyTier 格式。

### 4. P2P 协议策略与连接管理

- 增加 `prefer`、`off`、`disabled`、`only` 四种 P2P/打洞伪装策略；直连、TCP 打洞和 UDP 打洞都会参考对端能力。
- TCP 打洞成功的连接可升级为 WSS；双方支持伪装 P2P 时，UDP 打洞会按次协商 HTTP3。严格模式要求 HTTP3，不能降级；优先模式在远端或本机不支持时回退裸 UDP。
- 新版双方通过打洞 RPC 的 `native_http3` 能力协商原生 QUIC，去除 HTTP3 数据外层的 EasyTier UDP 头部。旧魔改节点未返回该能力时仍使用原封装，官方节点的空 scheme 仍表示裸 UDP；SNI、TLS 1.3、`h3` ALPN 和 BBR 沿用原 HTTP3 引擎。
- UDP 打洞监听器按裸 UDP、原生 HTTP3、旧版封装 HTTP3 分池复用，HTTP3 与 WSS 打洞一样使用打洞过程中的自动临时监听端口，不依赖手动公布的 HTTP3/WSS 服务监听器。
- 支持 HTTP3 的节点会在已配置的 UDP 公网监听端口同时接受 HTTP3，并仅公布已就绪的升级端点；自动P2P无需另加 HTTP3 监听或端口。普通 TCP 公网监听不因此变为 WSS。
- `default_protocol = "udp"` 将 HTTP3 排在 WSS 前，`"tcp"` 则相反；GUI/Web 提供该协议偏好选择。对普通 UDP/TCP 打洞，该选项分别对应 HTTP3/WSS 升级偏好。
- 打洞 RPC 的 scheme 字段为空表示官方主线裸 UDP/TCP 协议。官方 peer 不返回新字段时，本分支在优先模式下回退裸 UDP；严格模式不会接受降级连接。
- 已有回退连接仍会继续尝试首选协议；协商伪装且选择 `udp` 时，已有 WSS 连接仍会继续尝试 HTTP3，选择 `tcp` 时则优先 WSS。
- HTTP3 打洞立即进入首次调度；优先模式下 30 秒后允许并行尝试裸 UDP 回退，已有同级或更优连接时停止重复尝试。严格模式不启动裸 UDP 回退。
- 多网卡 HTTP3 拨号在协议握手成功后才选定网卡，避免把尚未验证的 UDP 会话当成可用链路；本机回环地址由系统选择回环路径。
- 可选 `close_redundant_conns_when_disguised`（界面为“首选连接建立后断开多余连接”），开关默认开启，可显式关闭。开启后，已建立且通过往返验证的 WSS/HTTP3 优先于裸 TCP/UDP，即使伪装策略为 `off`（不主动伪装）也如此；显式“禁止伪装”时仍按普通协议排序。此规则仅决定已建连接的保留优先级，不改变主动拨号的伪装策略。自动连接之间仍按 TCP 偏好 WSS、UDP 偏好 HTTP3 的顺序清理较低优先级连接，同级自动连接保留。
- 手动填写初始节点建立的连接优先级最高，不受伪装策略限制。双方新版握手协商并确认连接可用后，按双方一致的顺序保留一条手动连接，断开同一对等节点的其他连接（包括其他手动连接）。被清理的手动连接 URL 暂停重拨，保留连接失败后恢复重拨。旧版或不支持新协商的节点不强制清理连接。
- 冗余清理通过普通和 Noise 握手已有的 `features` 声明 `p2p-cleanup-v2` 策略快照，包含连接来源和双方共享排序，不修改官方线格式。自动连接只有在双方都认定其优先级更低时才清理；未知或非法声明不影响握手，但不触发清理。热修改排序配置后，旧连接暂停清理，直到重新握手取得匹配快照。
- 新配置与没有旧布尔覆盖的旧配置默认使用 `prefer`、UDP 和开启冗余连接清理，GUI/Web 与内核一致；旧布尔字段仍按原含义读取，GUI/Web 将新策略下拉选择转换为旧字段供兼容内核使用。
- 省略 `default_protocol` 时优先 UDP；需要优先 TCP 时，在 `[flags]` 显式保存 `default_protocol = "tcp"`。省略清理开关时默认开启；需要禁用时显式保存 `close_redundant_conns_when_disguised = false`。
- 修复 SNI 改写 URL 后连接身份不一致导致的重连堆积，并替换相同 URL 的重复客户端连接。
- `disable_p2p` 调整为半严格行为：拒绝普通节点发起的 TCP/UDP 打洞 RPC，保留声明 `need_p2p` 的节点例外，不主动断开已有连接。

- 连接来源收敛为官方六类 `PeerConnectionOrigin`，由来源派生打洞保活和清理资格；TCP→WSS 保留 TCP 打洞来源，UDP→HTTP3 保留 UDP 打洞来源。
- TCP 打洞 Ping 最大间隔采用官方 1 秒限制，保留带宽 Ping/Pong 扩展；清理队列在入队与出队时复核开关、协议偏好和可用替代连接，关闭网络连接发生在锁外。
- 修复升序排序后从末尾消费导致的拨号偏好颠倒，并验证实际消费顺序。

### 5. 实验性 TCP 对称 NAT 打洞增强

- TCP STUN 增加额外绑定探测，识别端口递增/递减的容易预测的对称 NAT。
- TCP 探测结果未知时借用 UDP NAT 类型作为近似值；这不等同于成功完成 TCP 探测。
- 打洞 RPC 增加端口预测能力协商与预测窗口；对称 NAT 响应端可用多个本地端口并发尝试连接，发起端对预测端口进行受限并发尝试。
- 限制预测数量、尝试窗口和重试行为，处理端口范围、方向及不支持扩展的对端。
- Linux/Android 打洞 socket 使用 `TCP_SYNCNT` 限制 SYN 重传；其他 TCP 连接保留原有行为。
- 具体效果依赖 NAT 映射规则、防火墙和对端版本；不能保证任意对称 NAT 都可打通。

相关代码：`easytier-core/src/connectivity/hole_punch/tcp.rs`、`connectivity/stun/`、`easytier-proto/proto/peer_rpc.proto`、`easytier/src/socket/tcp.rs`。

### 6. 流量、带宽与地理展示

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
| `[flags] default_protocol` | `"udp"` | 自动P2P优先 UDP，协商伪装时优先 HTTP3 |
| `[flags] private_mode` | 定制内核 `true`；官方内核保持官方默认 | 网页按能力显示默认值，默认不写入配置；官方节点没有定制能力标记时显示并沿用官方默认 `false` |
| `[flags] p2p_disguise_mode` | `"prefer"` | 自动 P2P 伪装策略；取值为 `prefer`、`off`、`disabled`、`only`，默认策略可省略 |
| `[flags] prefer_wss_http3_for_p2p` | `true`（旧字段） | 兼容旧定制内核的策略输入；新内核用 `p2p_disguise_mode` 保存单一策略 |
| `[flags] disable_wss_http3_for_p2p` | `false`（旧字段） | 兼容旧策略输入；`true` 对应 `disabled` |
| `[flags] only_use_wss_http3_for_hole_punching` | `false`（旧字段） | 兼容旧策略输入；`true` 对应 `only` |
| `[flags] close_redundant_conns_when_disguised` | `true` | 双方协商并验证连接可用后，优先保留一条手动初始节点连接；否则优先保留 WSS/HTTP3（显式禁止伪装除外），清理较低优先级自动连接 |
| `[vpn].exit_enabled` | `true` | 允许本节点提供出口转发；没有规则时不会启动 VPN 客户端，默认空策略不需要写 `[vpn]` |

这些策略受本机构建能力和对端能力影响。与原版混用时不要假定扩展功能全部可用；强制仅使用 WSS/HTTP3 可能减少可连接路径。启用互相冲突的策略可能使连接无法建立。

配置标志使用 schema 生成的 `Flags` / `FlagsPatch`、CLI 和管理表单。网页生成 TOML 时会省略与目标默认值相同的表单值，减少冗余；能改变行为的显式 `false`、有意义的 `0`，以及用于关闭隐式默认的空 `listeners`、`routes` 仍会保留。`private_mode` 的定制默认值为 `true`，由能力 `config:private-mode-default-true-v1` 声明；网页将该默认留空，由内核应用。官方节点没有此能力时，网页显示 `false` 并继续交给官方内核按官方默认处理。用户明确修改后，值仍作为显式字段发送。

新定制内核以 `[flags].p2p_disguise_mode` 保存一个伪装策略：`prefer` 在双方都偏好或一方要求时协商伪装，并允许回退普通 TCP/UDP；`off` 不主动发起伪装，但仍接受对端要求；`disabled` 禁止自动伪装；`only` 要求自动 P2P 只使用伪装。默认 `prefer` 不写入 TOML。旧的三个布尔字段继续读取；向仅支持旧字段的定制内核保存时，网页会将下拉选择转换为对应布尔值。官方节点不会收到不支持的定制策略字段。GUI/Web 使用紧凑下拉框编辑这四种模式；互相冲突的旧布尔组合在编辑、API 和严格 TOML 加载时会被拒绝。

VPN 出口的 `exit_enabled` 默认 `true`，表示本节点允许提供出口转发；它本身不会启动客户端。`rules` 为空时没有客户端捕获规则，网页输出会省略默认的 `[vpn]`；显式关闭出口时仍保存 `exit_enabled = false`。缺少数据包转发能力的构建或主机会保持普通网络可运行，并在运行配置中关闭默认出口提供能力。`default_protocol` 的表单/API 名称仍为 `p2p_prefer_protocol`。定制 protobuf 字段采用本轮高位清单中的编号；既有 CLI 参数和 `ET_*` 环境变量保持不变。目录的“待应用”比较使用解析后的运行默认值，同时继续以原始文件内容保护 revision/CAS。

已合入官方 UDP 绑定的虚拟端口所有权和分片缓存：未先发送的绑定 socket 可接收数据；关闭后重绑不会接收旧绑定的残留分片。`PeerPacketFilterResult::PassBatch` 中的包按原顺序从产生批次的下一个过滤器继续处理。本地实际监听端口的防回环检查与上述虚拟端口所有权共同生效。

## 构建与验证

沿用仓库的 Rust 工具链和构建方式（`rust-toolchain.toml` 当前指定 Rust 1.95），原有可执行文件名称仍为 EasyTier 系列。构建示例：

```sh
git clone https://github.com/225284228a-droid/EasyTier-Custom.git
cd EasyTier-Custom
cargo build --release --locked -p easytier
```

### 官方主线合入验证（2026-10-16）

本轮固定官方基线为 `99db92a3aeb3109efe1ace5a4a3e2a1bf3840ed7`，在原 `main` 上保留双方历史合并。日志保存在本地 `.test-env/upstream-merge-2026-10-16/`，以下结果与旧轮次分开记录。

- 审查后的源码合并提交为 `5ff6fefb7f0a40fe431b206e7e2f3de472e58048`。Go 内嵌 WASM 从包含后续修复的干净源码提交重新生成并单独提交产物，实际来源以两份 `provenance.go` 为准；来源提交表示此次构建的实际输入，未来重新生成时随源码更新，不是永久固定的版本。
- 首次使用新 WASM 的 Go 集成测试发现 WASI 创建 ABI 使用容错加载器，导致非法配置被丢弃后仍返回成功。已恢复该 API 的严格解析，保留 Host 运行能力过滤和桌面配置文件的容错恢复策略；回归覆盖非法 TOML、DHCP、VPN 字段、重复子网和非法出口地址，以及合法但 Host 不支持的配置只在运行时过滤。
- 新增 WASI schema 两项回归通过。Go WASM 从干净源码提交 `55ed334dbe1eefd3851b88e114b3cc43b26e3c3c` 重新生成，使用 protoc 35.1、protoc-gen-go v1.36.11 和 Binaryen 131。产物为 7,337,384 字节，SHA-256 为 `163ecfb7e1c262dcb4d05d12ee009eb16dec194ab3927dab11c2b9df87243f84`；schema 输入 SHA-256 为 `02a55bf66f39f791a14971428bb3b104a7d0afa58493af0f2d7538d86dceec3e`。两份 provenance、实际文件摘要和生成的 protobuf 无漂移检查均通过。
- Go 主模块完整测试和独立 TUN 示例模块测试通过，覆盖新 WASM 的 ABI、引擎、管理配置校验和公开 API 网络通信。网络测试辅助代码改为仅记录 `TCPListenDirect` 端口，避免 VPN 出口的 NAT 监听覆盖正式节点端口；固定同一失败产物进行前后对照后，四项网络测试均通过，保留路由、双向收发及最大 UDP 载荷断言。
- 相邻入口的 C ABI `run_network_instance` 和 CLI `--check-config` 同样恢复严格校验，普通启动及 `config.d` 的容错恢复保持不变。C ABI 拒绝非法输入的回归 1 项通过，CLI 文件/标准输入及日志配置测试 6 项通过；这两组检查不会启动真实网卡。
- GUI 私有提权模块的旧示例错误引用不存在的外部 `elevated_command` 包，已改为说明内部调用方式；没有为文档扩大公开 API 或关闭 doctest。串行 `cargo test -p easytier-gui --doc` 通过，目前该包没有可执行文档示例。
- Core 完整库测试 1230 项通过；Web 后端 355 项通过、1 项默认忽略；GUI 后端 28 项通过。手动连接竞争测试修复了对已被合法清理连接的 RTT 等待，保留共同胜出连接、双向通信和关闭断言，连续重复 20 次通过。
- `cargo fmt --all -- --check` 和选定包（Core、原生、proto、Web、GUI、FFI）的 all-targets Clippy `-D warnings` 通过；该 Windows 检查不等于 Linux 全 workspace/full 特性验证。
- protobuf 库测试 39 项通过、2 项默认忽略；descriptor 检查 4 项通过，覆盖全部 109 个定制字段的所有权、编号、类型、presence 和高位编解码，以及官方 DHCP 字段和定制策略同时往返。
- 原生组合专项 17 项通过，覆盖 IPv4/IPv6 上通过 TCP/WSS 中继发现 HTTP3、手动初始连接优先级、UDP/HTTP3 共端口和 KCP/TCP/WSS 通信。与自报版本 `2.7.0-99db92a3` 的官方 Windows CLI 真实进程互通 6 项通过，包括 WS/WSS 双向 peer、Noise、管理反向 RPC 及未协商 padding 拒绝。
- 原生共享网卡、移动 TUN、VPN 路由和配置持久化定向测试 99 项通过、3 项忽略；另有 1 项真实 TUN 测试因 Windows 无法创建 Wintun adapter 失败。新增回归覆盖 VPN 捕获规则的归属冲突、IPv4/IPv6 dispatcher 选择、清理失败时保留归属、无效网卡仍执行清理，以及清理耗尽后先隔离共享 dispatcher 再释放成员。
- 共享前端 176 项、Web 前端 198 项、GUI 前端 62 项单测通过；配置转换脚本 10 项通过。Web、GUI 和配置生成器类型检查、生产构建通过，配置生成器 WASM 与 VPN service JS 插件构建通过；审查修复后再次通过 GUI 类型检查和共享配置 defaults/UI 29 项定向测试。
- JS 浏览器和 Cloudflare WASI profile 构建、优化及 ABI 检查通过，运行时 28 项、浏览器 2 项、Cloudflare 2 项测试通过；示例类型检查、构建及 Wrangler 部署 dry-run 通过。
- actionlint 检查全部 14 个 workflow 通过。本机 CI 脚本测试 31 项通过，9 项因缺少 Unix `zip` 命令失败；这些打包测试交由 Linux CI 验证。
- Windows 本机不能验证 Linux TUN/netns、真实跨网络共享 TUN、内核转发和 Android 系统 VPN 生命周期；Linux 集成测试及 Android 完整构建由相应 CI 执行。移动端单元测试验证 generation、candidate 确认、过期停止和失败重试，不能替代真机 VPN 验收。

### 官方主线合入验证（2026-10-11）

本轮使用 merge commit 合入官方 `7592951b808c4839cb9d77cc4bc642f124915951`，保留原有定制提交历史。日志保存在本地 `.test-env/upstream-merge-2026-10-11/`；以下仅列本轮实际执行的检查。

- `cargo fmt --all -- --check`、`cargo metadata --locked --offline` 通过；锁文件按最终依赖解析。
- Core 启用 `proxy-smoltcp-stack,browser-config` 后完整库测试 1158 项通过。覆盖 UDP 绑定、端口所有权、分片缓存及乱序、关闭重绑、过滤器批次顺序、连接选择/清理、配置显式值和目录语义比较；最初运行中的两项调度超时和默认值比较失败均在最终完整运行中通过。
- CLI 配置及参数测试 17 项通过，包含默认 `config.d`、`--state-dir`、schema 参数、定制开关与环境变量、显式值及目录启动隔离。
- 原生 `instance::` 定向回归 70 项通过，覆盖每实例执行器、四组 TCP/WSS 经中继发现自动 HTTP3、四组手动初始连接保留及双向数据、配置持久化事务与运行配置同步。HTTP3 7 项、WebSocket 20 项通过。首次原生运行还出现 5 项 Windows DNS/TUN 环境失败；最终 `instance::` 运行排除 DNS 模块和 `tun_test`，没有重跑修改主机 DNS 或依赖 TUN 驱动的测试，不能将其记为通过。
- 原生 CLI 日志与 schema 环境变量集成测试 4 项、named WireGuard 密钥回归 1 项通过。Web 后端完整运行 329 项通过、1 项既有容量压力测试默认忽略，包含持久化快照、多服务器 CAS/同步、停用状态、运行时能力过滤及 OIDC/依赖迁移相关回归。
- GUI 后端 27 项通过，覆盖普通/服务模式配置目录、停用状态、服务参数、RPC 占用判断及 Windows 服务冲突识别；本次两个 Windows Clippy 修复仅调整等价写法。
- protobuf 库单元测试 36 项通过、2 项默认忽略，覆盖 Flags schema 默认值/字段编号、显式值与 unset、API 序列化和当前握手线格式。
- 使用此前独立构建的官方 `4837468d` CLI，重新执行 WS/WSS 真实进程互通 6 项，全部通过：双向 peer、Noise、管理反向 RPC 与未协商 padding 拒绝。该项验证旧官方节点兼容性，未将旧二进制标记为本轮官方 `7592951b` 产物；本轮 HTTP3 实例和传输测试使用合入后的源码。
- `script/build-wasi-core.sh` 的 `wasm32-wasip1` release 构建及 Binaryen 131 优化通过，启用 `management-rpc,proxy-smoltcp-stack,ring-crypto,wasi-crypto-offload`。Windows 本机使用已有 Zig 编译器提供 WASI C 后端；Go 生成器的可重现构建环境与 schema 输入摘要单元测试通过。嵌入产物在源码合并提交后重新生成并独立记录来源。
- 已从干净的源码合并提交 `938e7d1eccc6e7bd8da07cf993854bd646887879` 运行完整 Go/WASM 生成器，使用 protoc 35.1、protoc-gen-go v1.36.11 和 Binaryen 131。Windows 的 protoc 中文输出路径失败后，改用指向同一源码的 ASCII 别名成功生成，没有手写产物或来源记录。
- 新内嵌 WASM 为 7,153,347 字节，SHA-256 为 `be88f075d3f56fbbda194d9a4282812b27d30cd5d3f3311f82e4447ac72c001e`；schema 输入 SHA-256 为 `692365b296dd5dfe4d94b8682a45f43a0a41297931507198d167c980e186ccbe`。两份 provenance 均指向上述源码提交。Go 主模块和 TUN 示例模块 `go test ./...` 通过，实际 WASM ABI/引擎/Host 测试通过；再次使用固定生成器刷新 protobuf 后，所有 `.pb.go` 文件无漂移。
- 共享/Web/GUI 前端单元测试共 342 项、配置转换检查 10 项、完整浏览器回归 42 项通过；Web 与 GUI 类型检查和生产构建通过。浏览器仍使用项目既有 `CHROMIUM_ANGLE_BACKEND=d3d11` 验证路径，没有降低地图精度或断言。
- Windows 生产代码 Clippy 通过：`cargo clippy --locked -p easytier-core -p easytier -p easytier-web -p easytier-gui --lib --bins -- -D warnings`。本机缺少完整 Perl/OpenSSL 构建环境，且 OHOS crate 不适用于 Windows，不能据此声称整个 workspace 的 `--features full --all-targets` 通过；完整特性检查由 Linux CI 执行。
- 修复 Web API 测试中缺少 `runtime_capabilities` 的初始化，保留运行时能力声明。此前 Linux CI 的双管理员 credential 测试在已完成故障转移后的稳定性检查阶段触及整段 60 秒预算；仅对该单项设置 Nextest 180 秒预算，保留默认 60 秒以及内部所有阶段期限、唯一性和数据断言。

### 四项回归修复验证（2026-10-08）

- 实时查询官方远端 `refs/heads/main` 得到 `4837468d43b6fe48865b2ce926312b66afd10bef`，与本次独立构建的互通基线一致；未额外合入上游提交。独立源码目录的握手、WebSocket、进程管理及锁文件已与该提交核对。
- Core 默认库测试 967 项通过；新增测试包括普通/Noise 握手扩展、双端四种拨号方向、协议偏好冲突、单端清理开关、双向收发、策略热修改、排队后复验和重连恢复。
- Web 后端 325 项通过，1 项既有大容量连接测试默认忽略；覆盖旧节点能力过滤以及 TCP/UDP/WS/WSS 实际监听端口、配置顺序和首项绑定失败回退。
- 原生持久化配置事务 24 项通过，覆盖旧请求调用前及加载/hook 期间出现的重复 UUID、混合批次部分成功、权限/CAS、完整原文应用、来源切换、停止实例不启动和失败回滚；WS/WSS 协商 20 项通过。
- 独立官方 `4837468d43b6fe48865b2ce926312b66afd10bef` 进程互通 6 项通过：WS/WSS 双向各两种 padding、WS/WSS 双向 Noise、管理连接及反向 RPC、拒绝未协商文本 padding 的探针。官方未声明清理扩展时连接继续存活并双向交换数据。
- 共享前端 31 个文件、297 项测试，配置导出 10 项检查，GUI 前端 4 个文件、25 项测试通过；共享库、Web、GUI 的类型检查和生产构建均通过。

### 此前验证

以下为截至 `8f5e08a5` 的历史验证记录；未重新运行的历史验收不计入本轮结果：

- Core 默认构建的 957 项库测试通过，覆盖实际拨号顺序、显式协议偏好、连接来源与清理、打洞协商、DHCP、安全中继和 VPN Portal。
- 最小 feature profile（关闭默认特性，仅 `test-utils,proxy-cidr-monitor`）测试通过。
- Web 后端完整默认测试运行通过：321 项通过、1 项既有容量压力测试默认忽略；版本冲突、旧会话响应、离线保护及官方中央编排回归包含在本次运行中。
- 原生持久化配置事务测试 20 项通过，覆盖停用状态、仅保存后热修改、取消中的写入、外部修改后的拒绝及回滚保护；双服务器镜像与版本校验测试 9 项通过，包括两个独立数据库同时修改同一版本和手改后 10 秒内刷新。
- 配置目录启动修复的 4 项定向测试与真实 CLI 基本启动验证通过：非法/重复身份配置隔离、停用状态保留、只读/环境变量/旧无 UUID 配置正常加载，不自动创建替代网络。
- 两个真实 Web 服务、两个独立 SQLite 数据库和两个浏览器会话的基本验收通过：网页保存后两侧页面最慢 4.215 秒刷新，稳定手改后最慢 5.027 秒刷新；同版本同时提交仅一侧成功，另一侧返回 409 并保留草稿。断线时镜像可读、写入被拒绝，重连无写入重放；账号隔离、原始注释和非表单字段保留也通过。
- WS/WSS 协商测试 20 项通过，覆盖实际 Upgrade 请求与响应、缺少能力头和未知 token 的 binary 回退、配置服务器首个 RPC。
- 使用固定官方 `4837468d` 源码独立构建的真实进程执行 5 项互通测试，全部通过：WS/WSS × 双向 × 两种 padding 配置的 8 组 peer 连接、2 组管理连接及反向 RPC、2 组官方端拒绝未协商文本 padding 的探针。回环测试显式关闭设备绑定，不修改产品的设备绑定策略。
- 共享前端最终完整运行 31 个测试文件、282 项测试及 GUI 前端 24 项测试通过；配置导出测试 10 项通过，共享前端与 GUI/Web 的类型检查和生产构建通过。
- 官方控制台浏览器场景和定制地球/流量完整浏览器验收通过。地球使用本机 Intel GPU 的 ANGLE D3D11，覆盖桌面和手机尺寸、10m 高精度地图、双向流量/RTT、暂停与拖拽、缩放和标签布局；SwiftShader 软件渲染会显著拖慢地图读取，不据此降低地图精度或验收断言。可用 `CHROMIUM_ANGLE_BACKEND=d3d11` 选择同一验收路径。
- GUI Rust 检查与原生可执行文件构建通过；`cargo fmt --all -- --check` 和 `git diff --check` 通过。
- Go 绑定及内嵌 WASI 已通过实际生成脚本刷新，生成输入为源码提交 `e7ea7b98b168252891df1c73d6329285a0bc7425`；后续原生 CLI 启动修复和测试文件修改不影响该 WASI 构建输入。内嵌 WASI 为 7,119,615 字节，SHA-256 为 `47d630a900a4b8e6b4a698912a8b02c956eeebafa71d2a861f3516321fd2256d`，provenance 保留实际生成记录。
- Go 现有测试与真实 Web 管理端基本 E2E 通过，覆盖实例创建、Host 不支持的 WG/VPN Portal 过滤、返回 204 的在线配置修改、TCP 端口实际热修改、原实例对象保留及删除。E2E 配置改用官方现行的 named-client VPN Portal 结构；旧 `enable_vpn_portal = true` 输入在官方基线和本分支都被拒绝。
- JS 浏览器与 Cloudflare 两个 WASI profile 构建、优化与 ABI 检查通过，32 项运行时测试、示例类型检查、浏览器构建和 Worker 部署 dry-run 通过。Go/JS WASI 使用本机已有 LLVM，无需安装完整 WASI SDK；原生编译也不因此新增该依赖。
- `cargo clippy -p easytier-core -p easytier -p easytier-web --all-targets --locked --offline` 通过；Windows 构建仍有既有测试辅助代码的未使用警告，本条不表示全部 CI 平台的检查结果。

原生完整测试另运行了 291 项：272 项通过、11 项失败、8 项默认忽略。失败涉及 Windows 管理员防火墙操作、Wintun DLL 搜索路径、外网 DNS 返回数量、指定回环接口及 socket 绑定/UDPv6 条件；不能把本机未满足的这些测试条件记为通过。Wintun 失败时测试程序未附带 DLL，系统搜索到了 Cloudflare WARP 的版本；仓库及 GUI 附带的 WireGuard DLL 与合入前、官方基线一致且签名有效。HTTP3/WSS 实际升级、IPv4/IPv6 数据通道、BBR 和持久化配置事务在该次运行中通过。默认忽略项含需单独提供官方进程的 5 项互通测试，以及防火墙与约 16 GB 压力测试。

既有同端口性能检查使用本机回环与抓包转发器，对比独立 HTTP3 监听和 UDP 共用监听。双方路由就绪后，每方向发送 256 个数据包，最多保持 64 个未收齐包，避免触及核心现有 128 包主机出站队列的溢出丢包策略；校验逐包内容和完整性。三轮复测均通过，但短时 debug 吞吐存在调度波动。上述验证不代表公网吞吐、真实 NAT 成功率或全部平台组合；本轮不发布安装包。

## 审阅完整差异

Git 历史保留了上游提交与本项目提交。克隆后可查看完整差异与变更记录：

```sh
git diff 99db92a3aeb3109efe1ace5a4a3e2a1bf3840ed7 HEAD
git log --oneline 99db92a3aeb3109efe1ace5a4a3e2a1bf3840ed7..HEAD
```

原版 README 的安装脚本、官方 Web 服务、发布下载、徽章和赞助链接仍指向上游。使用本项目功能请构建本仓库，问题请提交到本项目 Issues。
