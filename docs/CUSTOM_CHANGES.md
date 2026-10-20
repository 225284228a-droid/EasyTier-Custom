# EasyTier Custom：来源与改动说明

本项目是基于 [EasyTier（ET）](https://github.com/EasyTier/EasyTier) 的独立衍生项目，由本仓库维护者维护。EasyTier 的基础网络能力和原有代码来自上游及其贡献者；以下列出本分支在该基础上增加或调整的行为。此项目并非官方版本，也不承诺这些改动会进入官方主线。

## 上游来源与许可证

- 上游仓库：https://github.com/EasyTier/EasyTier
- 本项目仓库：https://github.com/225284228a-droid/EasyTier-Custom
- 最近已合入的上游基线：`5af90471bfe654d097f7e488a2699f2850db7dd2`（2026-10-19 合入）。
- 开源协议与该上游版本一致：GNU Lesser General Public License v3.0（LGPL-3.0），见根目录 [LICENSE](../LICENSE)。保留上游历史、作者归属及第三方组件各自的许可证；本项目修改按同一项目许可证发布。
- 本文对比上述固定基线，不把“尚未合入上游后续提交”当作本项目主动删除的功能。

## 相对原版的主要改动

### HTTP3/WSS 传输升级（2026-10-18）

- `http3://` 使用真正的 HTTP/3：标准 `h3` ALPN、TLS 1.3、控制流、SETTINGS、QPACK 和 HTTP 请求/响应；EasyTier 报文通过双向流式 POST 的 DATA body 承载。普通 GET 返回 HTTP 状态，原 UDP 共端口监听、SNI、打洞、准入控制和 BBR 继续生效。
- 不兼容旧定制客户端的自定义 HTTP3 应用流格式；新版定制节点需要一起升级。官方节点通过现有能力协商选择其支持的 TCP/UDP/QUIC/WS/WSS，不接收未协商的定制传输格式。
- HTTP3/WSS 共用测速和视频流量风格。测速持续发送，视频仅在持续大流中加入不超过 5ms 的短暂突发间隔；控制、探活和低延迟报文不等待，空闲时不持续发送填充。
- 连接 URL 的显式 `traffic_disguise=speedtest` / `traffic_disguise=video` 优先于全局风格；现有 SNI 覆盖及 Host、请求路径、User-Agent、语言和 padding 参数继续有效。WSS 根据风格选择默认请求路径、Accept 和 Cache-Control，显式 URL 参数优先；保持每条 binary 消息一个原报文，文本 padding 仍要求双方通过 Upgrade 声明支持。
- HTTP3 body 有界背压、请求头与帧长检查以及可取消的连接任务；关闭最后一个隧道端会释放 HTTP/3 驱动、QUIC 连接和 endpoint。

### 多连接并行传输（2026-10-18）

- 顶层 `parallel_connections` 取值 1–16，默认 1；`disable_parallel_connections` 默认 `false`，任意一端禁用即保持单连接；`traffic_disguise` 可选 `speedtest`（默认）和 `video`。默认值由后端统一省略，禁用并发不会清除已填写的数量。TOML、CLI、管理 API 和 GUI/Web 使用同一组配置，官方内核隐藏并过滤不支持的字段。
- 编辑网络时，SNI 与 WSS/HTTP3 流量风格并排各占半行，连接数紧随其后；禁用多连接并发归入现有功能开关。
- 普通和 Noise 握手通过版本化能力声明期望数量、禁用状态及资源上限。双方支持时，目标为两端数量平均值四舍五入，再受双方跨网络配额限制；例如 1 与 5 得到 3。缺少能力时使用官方原有报文和单通路，不发送并发外壳。
- 手动 URL 决定锚点及扩容协议；自动连接按双方伪装策略和协议偏好选取共同协议，偏好冲突由较小 Peer ID 的顺序裁决。物理成员使用独立连接或 UDP 会话与源端口；ring、attached 和虚拟 relay 会话不复制，中继业务可使用物理下一跳的连接池。
- 每节点同时最多补建两条连接，槽位预留、池 ID 和代次防止重复扩容，失败后有限退避。不同槽位允许共存，同槽重连替换；往返验证通过的成员才参与发送。控制消息使用稳定默认连接，业务报文在加密、压缩后按字节公平分配，保留可丢弃语义和队列背压。
- 接收端在每连接解密后共享重排，再把完整原报文交回原转发流程。每方向上限 8192 包、16MiB，全局上限 128MiB；缺口等待为最大成员 RTT 的三倍，限制在 20–250ms，超时和容量耗尽会跳过缺包，重复与迟到报文丢弃。连接断开后剩余成员继续工作并补连。
- 探活 Ping 的发送入队与 Pong 等待共用 2 秒期限；底层发送队列阻塞时不再让探活任务永久停住并保留旧的延迟/丢包状态，连续失败仍会触发断线重连。
- 协商后的配置更新通过默认连接同步确认；禁用时停止分流，短暂排空后关闭多余成员并保留锚点。界面仅在协议后显示活动连接倍数，如 `http3[×4]`，不增加独立并行状态栏；管理 API 保留目标数、实际成员、协议和回退原因，供诊断使用。
- 修正多连接状态下把各隧道窗口带宽估算相加的问题：界面改用默认活动连接的参考值，上下行来自同一条连接，不把共享瓶颈重复计入容量；默认连接无任何估算时才选取其他活动连接，单侧缺失不拼接其他成员的数据。延迟和丢包过滤已关闭与池外备用连接，未完成探测的零值不显示为零延迟。窗口带宽本身仍非测速结果，两端窗口不同、空闲或轻载等情况仍可能造成明显不对称，不能据此判断物理网线的速率。
- 修正低延迟链路的窗口估算：拥塞窗口只配对同一传输层的 RTT，去掉应用层 Ping RTT 下限和固定 1ms 下限，避免空闲及满载局域网都被系统性低估。Windows `Cwnd`/`SndWnd` 本身为字节，不再额外换算；trace 记录原始窗口、MSS、RTT 与连接端点便于诊断。代理/CDN 的结果只描述可观察的传输段，不代表其后端容量。
- 带宽估计升级为 v2，不发送任何主动测试流量。有窗口传输在活动期受实际发送吞吐约束；UDP、WireGuard 和 fake TCP 通过 Ping/Pong 交换双方最近 0.5 至 5 秒的业务吞吐，仅在连续观察到发送高于接收至少 15% 的饱和样本后，使用最近样本的下中位数作为参考。空闲、轻载、样本过小或超过 15 秒的旧样本返回未知，不再保留历史最大值。v2 节点仍用 v1 线格式回复旧节点。
- 同一对端的初次连接接入串行化，避免双方同时拨号时创建两份 Peer 状态并丢失已协商的接收代次；末条连接删除、代次重置与新连接接入同样互斥，连接关闭事件在状态清理完成后发布。

### 定制 protobuf 字段编号与版本兼容

- 2026-10-16 合并时，所有原先使用低位编号的 Custom protobuf 字段均迁移到高位：新编号为原 tag 加 `50000`；若该编号已被占用，则每次再加 `1000`，直到找到空闲编号。已有高位编号保持不变。
- 该次共迁移 100 个低位字段，保留 9 个已有高位字段；完整旧/新编号见 [Custom 字段清单](../easytier-proto/tests/custom-fields.json)，官方字段编号、类型和保留状态见 [官方 schema 快照](../easytier-proto/tests/upstream-fields.json)。descriptor 测试与 CI 会检查清单完整性和编号分配。
- 不保留旧低位编号的双读写兼容路径。使用这些扩展字段通信的定制 Core、Web、GUI、Go 和 JS 组件必须同步升级到对应版本；新版本不再识别旧定制 tag，混用时定制字段无法正确传递。下文的能力降级与旧接口说明不包含迁移前的定制协议版本。
- 此迁移只调整 protobuf 线上的字段编号；对应 TOML、CLI 和 JSON 字段名保持不变。

### 维护性整理（2026-10-16）

- 删除旧 VPN 策略遗留且已无调用的网关路由、NAT66 接口及辅助代码；文档统一为当前“每条 CIDR 指定一个出口、出口失联丢弃”的网段出口语义。
- WSS URL 参数和请求构造、padding 协商与帧处理分模块维护，保留既有公开入口。修复 `host=` 显式端口被拨号端口覆盖的问题，支持域名、IPv4 与 IPv6 authority，保持拨号地址和 TLS SNI 独立。
- QUIC/HTTP3 收发两端共用 4500 字节的 tunnel body 上限（含 PeerManagerHeader），修复服务端仍使用 2000 字节上限的问题，并覆盖双向边界和超限拒绝。
- P2P 直连、TCP/UDP 打洞复用协议许可与能力回退判断，保留未知对端的协商状态、现有连接排名和手动连接优先级。
- 配置能力及 Host 支持条件使用同一份元数据生成 Rust 和前端视图；普通 flag 的 API→TOML 路径由现有 schema 注解生成，组合字段仍显式处理，保留清空、显式 `false` 和未编辑 TOML 的原有行为。

本轮验证：Windows 默认功能集下 436 项传输、配置、P2P 与管理回归通过；protobuf 44 项通过、2 项默认跳过；独立官方程序的 6 项 WS/WSS 互通通过；前端 176 项回归及生产构建通过；Linux `full` 的 13 项路由/netlink 回归、Windows/Linux 严格 Clippy（`-D warnings`）、无默认功能的 Core 编译及格式检查通过。

### 本轮官方功能适配

- 官方 IPv4 DHCP 子网、IPv6 DHCP 和 IPv6 DHCP 子网沿用官方字段 74、75、76；TOML 转换和带 revision/CAS 的字段级保存支持这些设置，并保留未编辑的 TOML 内容及定制策略。基础设置中的 IP/DHCP 区域与已合入的官方基线一致，仅提供原有 IPv4 地址及 DHCP 控件；上述子网和 DHCPv6 设置通过 TOML 配置。
- 共享网卡按成员管理地址、路由和生命周期，成员退出时等待其路由清理完成。重叠的地址或路由 claims 会被拒绝以维持网络隔离；同地址族的全隧道 `/0` VPN 捕获路由因此不能与其他网络同时共享一个 TUN。
- 桌面 VPN 捕获路由在写入系统前先登记到共享网卡，参与目的地址分发和跨成员冲突检查；系统路由清理成功后才释放归属，失败重试期间继续保留隔离。失效网卡仍执行系统路由清理；清理持续失败时关闭共享网卡的数据分发，防止残留捕获路由把流量送入其他网络。此登记不重复安装系统路由。
- 移动端多网络配置保留 generation 和 candidate 确认流程，过期安装、停止和回滚操作不能拆除新一代连接；同一代失败重试会重新安装已回滚的成员。
- 移动端等待重配置确认后再次核对 generation，旧候选的迟到拒绝不会清空新连接状态。仅修改 DHCP IPv4 子网时保留未选中的静态 IPv4 TOML 字段。
- 合入官方 UDP 打洞与手动路由热更新，并保留本版 WSS/HTTP3 能力协商、TOML 权威和停用状态。Magic DNS 及此前删除的旧出口节点、public IPv6 配置功能仍保持删除。

### 1. 本地 TOML 与去中心化配置管理

- 修正 GUI 普通模式与服务模式切换时的后台复活：退休的普通模式先取消目录观察/自动恢复，再等待进程管理协调器并排空网络；等待中的目录扫描在拿到协调器后再次检查取消状态。安装、启停服务与 RPC 重连共用后端生命周期锁，服务运行时禁止重连流程启动普通 Core。CLI 正常退出同样先停止自动恢复和管理入口，再关闭实例；网络的启用意图仍保留，交给新模式恢复。
- 未编辑的配置会连同版本号同步刷新；启动未修改配置前重新读取当前版本，不再反复提交打开编辑器时缓存的旧版本。实际有修改时继续保留原版本的冲突检查，界面提供明确的重新加载入口，避免用户只能反复刷新状态却无法恢复编辑。
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

相关代码：`easytier/src/tunnel/http3.rs`、`websocket.rs`、`protocol/adapters/`。这里的“伪装”描述传输格式、握手参数和发送节奏，不保证特定浏览器指纹。HTTP3 隧道使用真实 HTTP/3 请求、响应与 DATA body；原生 QUIC 打洞的数据包已去除外层 EasyTier UDP 头部，NAT 控制探测包仍沿用现有 EasyTier 格式。

### 4. P2P 协议策略与连接管理

- 增加 `prefer`、`off`、`disabled`、`only` 四种 P2P/打洞伪装策略；直连、TCP 打洞和 UDP 打洞都会参考对端能力。
- TCP 打洞成功的连接可升级为 WSS；双方支持伪装 P2P 时，UDP 打洞会按次协商 HTTP3。严格模式要求 HTTP3，不能降级；优先模式在远端或本机不支持时回退裸 UDP。
- 新版双方通过打洞 RPC 的 `native_http3` 能力协商原生 HTTP/3，HTTP3 数据不增加 EasyTier UDP 外层。官方节点的空 scheme 仍表示裸 UDP；SNI、TLS 1.3、`h3` ALPN 和 BBR 保持有效。
- UDP 打洞监听器按传输类型分池复用，HTTP3 与 WSS 打洞一样使用打洞过程中的自动临时监听端口，不依赖手动公布的 HTTP3/WSS 服务监听器。
- 支持 HTTP3 的节点会在已配置的 UDP 公网监听端口同时接受 HTTP3，并仅公布已就绪的升级端点；自动P2P无需另加 HTTP3 监听或端口。普通 TCP 公网监听不因此变为 WSS。
- `default_protocol = "udp"` 将 HTTP3 排在 WSS 前，`"tcp"` 则相反；GUI/Web 提供该协议偏好选择。对普通 UDP/TCP 打洞，该选项分别对应 HTTP3/WSS 升级偏好。
- 打洞 RPC 的 scheme 字段为空表示官方主线裸 UDP/TCP 协议。官方 peer 不返回新字段时，本分支在优先模式下回退裸 UDP；严格模式不会接受降级连接。
- 已有回退连接仍会继续尝试首选协议；协商伪装且选择 `udp` 时，已有 WSS 连接仍会继续尝试 HTTP3，选择 `tcp` 时则优先 WSS。
- HTTP3 打洞立即进入首次调度；优先模式下 30 秒后允许并行尝试裸 UDP 回退，已有同级或更优连接时停止重复尝试。严格模式不启动裸 UDP 回退。
- 多网卡 HTTP3 拨号在协议握手成功后才选定网卡，避免把尚未验证的 UDP 会话当成可用链路；本机回环地址由系统选择回环路径。
- 可选 `close_redundant_conns_when_disguised`（界面为“首选连接建立后断开多余连接”），开关默认开启，可显式关闭。开启后，已建立且通过往返验证的 WSS/HTTP3 优先于裸 TCP/UDP，即使伪装策略为 `off`（不主动伪装）也如此；显式“禁止伪装”时仍按普通协议排序。此规则仅决定已建连接的保留优先级，不改变主动拨号的伪装策略。自动连接之间仍按 TCP 偏好 WSS、UDP 偏好 HTTP3 的顺序清理较低优先级连接，同级自动连接保留。
- 手动填写初始节点建立的连接优先级最高，不受伪装策略限制。双方新版握手协商并确认连接可用后，按共同顺序保留手动锚点及已协商的并发槽位；未启用并发时保留一条。被清理的其他手动 URL 暂停重拨，保留连接失败后恢复重拨。缺少清理能力的节点不强制清理连接。
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

- 保留带宽 Ping/Pong 扩展、双向流量统计、城市缓存及地球展示，组合到官方导航、主题、设备和网络页面。界面列名改为“带宽参考”，并明确 UDP 等无窗口传输在空闲或缺少饱和样本时显示未知。
- 带宽窗口、v2 双向速率报告与估计值的中性类型放在 `easytier-core/src/foundation/bandwidth.rs`，socket 与 tunnel 共用基础类型；ETBW v1/v2 线格式编解码仍由 tunnel 负责。
- 地球为独立 `NetworkTopologyGlobe.vue` 组件，采集使用共享 `useMeshTopology.ts`；展示合并官方别名、网络归属、NAT、丢包率和 VPN Portal，采集与在线数量过滤离线设备。
- Web 持久化按实际绝对 API 地址和账号隔离；后台刷新不会覆盖未保存的表单输入，目录版本变化时提示重新读取。

详细流量说明见 [network-telemetry.md](network-telemetry.md)。

## 常用新增配置

以下为选项位置和当前默认值，不建议不加区分地全部开启：

| TOML 选项 | 当前默认值 | 作用 |
| --- | --- | --- |
| 顶层 `sni` | 未设置 | 出站 WSS/HTTP3 的全局 SNI |
| 顶层 `parallel_connections` | `1` | 期望物理隧道数，范围 1–16；双方支持时取平均值四舍五入 |
| 顶层 `disable_parallel_connections` | `false` | 禁用并行传输，保留已配置数量；任意一端禁用即回退单连接 |
| 顶层 `traffic_disguise` | `"speedtest"` | HTTP3/WSS 流量风格，可选 `speedtest`、`video` |
| `[flags] default_protocol` | `"udp"` | 自动P2P优先 UDP，协商伪装时优先 HTTP3 |
| `[flags] private_mode` | 定制内核 `true`；官方内核保持官方默认 | 网页按能力显示默认值，默认不写入配置；官方节点没有定制能力标记时显示并沿用官方默认 `false` |
| `[flags] p2p_disguise_mode` | `"prefer"` | 自动 P2P 伪装策略；取值为 `prefer`、`off`、`disabled`、`only`，默认策略可省略 |
| `[flags] prefer_wss_http3_for_p2p` | `true`（旧字段） | 兼容旧定制内核的策略输入；新内核用 `p2p_disguise_mode` 保存单一策略 |
| `[flags] disable_wss_http3_for_p2p` | `false`（旧字段） | 兼容旧策略输入；`true` 对应 `disabled` |
| `[flags] only_use_wss_http3_for_hole_punching` | `false`（旧字段） | 兼容旧策略输入；`true` 对应 `only` |
| `[flags] close_redundant_conns_when_disguised` | `true` | 双方协商并验证连接可用后，优先保留手动锚点及并发槽位；否则优先 WSS/HTTP3（显式禁止伪装除外），清理较低优先级自动连接 |
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

### HTTP3 与多连接专项验证（2026-10-18）

- 真实 HTTP/3 专项 6 项通过，包含独立标准 `h3` 客户端的 IPv4/IPv6 GET 与双向 POST、UDP 分类入口、拆合帧和帧长边界；WebSocket 专项 30 项、流量风格 3 项通过。
- 与官方 Windows CLI `2.7.0-99db92a3` 的独立进程互通 8 项通过，覆盖 TCP/UDP/QUIC/WS/WSS 双向传输、Noise、管理 RPC，以及设置多连接和视频风格后缺少能力时的回退。官方端不接收并发外壳和未协商 padding。
- 原生组合 34 项通过，覆盖四协议的双栈手动连接池、1 与 5 协商为 3、独立源端口、每成员实际业务流量、断线补连、单端禁用、双边同时拨号、TCP/HTTP3 Noise，以及经 TCP/WSS 中继发现 HTTP3 后自动扩容；TCP/HTTP3 的真实中继业务双向转发确认每条物理下一跳成员均承载数据。
- Core 与原生联合 feature 配置的完整核心库测试 1269 项通过，包含默认省略、API/TOML 往返、协商矩阵、官方能力缺失、慢队列、动态禁用/恢复、foreign 握手配额及协议迁移（含 16 条上限）、8192 包/16MiB/128MiB 容量与溢出、重复、长缺口、epoch 授权和半断线重建；CLI 配置专项 22 项通过。
- 共享前端 185 项、Web 前端 198 项、GUI 前端 62 项通过；配置转换脚本 10 项、protobuf 字段所有权与高位编解码检查 4 项通过。共享组件和 GUI 类型检查与生产构建通过。
- Core、原生、proto 与 Web 的 all-targets 严格 Clippy（`-D warnings`）、Core 无默认功能编译、Rust 格式及 diff 检查通过。
- 可控的每连接限速夹具向同一个 `Peer::send_msg` 发送一条 192 包业务流：连续三次结果为 3.947、4.010 和 4.006 倍，全部成员承载业务数据。基准应与编译等重负载分开运行；本轮同时编译时曾测得 2.495 倍。该结果验证调度和聚合能力，不代表真实公网测速；实际带宽仍受总出口、拥塞控制、RTT、NAT 和对端限额影响。
- 本机执行 Windows 回环与内存传输验证；真实公网 NAT、防火墙环境和跨平台发布构建仍需对应环境验证。

CI 后续修复：同步遗漏的 Go protobuf 生成文件；新增两个控制连接事件时序的回归，旧实现均稳定复现业务拒收，修复后通过。连接接入与末条连接清理现在共享生命周期锁，空 Peer 清理也与接入串行化；保持原有代次授权和测试超时。WSL Linux 的完整 Core 与原生组合 1309 项、同时拨号及生命周期 10 轮压力复验（共 40 次）通过，严格 Clippy 与格式检查通过。

Go 内嵌 WASM 与 protobuf 来源统一刷新为干净源码提交 `48b01ce657164e9906557b3d3c2f0a5f71a86400`。优化后 WASM 为 7,596,355 字节，SHA-256 为 `62c80c2a0df3de0321d31173511dd846be59e8b0570b35eb14328c7c1999c1c3`；schema SHA-256 为 `635670b2b1c8c9d91688328e4760b7c15bf8cfc20590cafdf0e26fb1864c63f1`。两份 provenance、实际产物与 schema 摘要核对通过，重复生成 protobuf 无漂移；使用新内核的 Go 主模块和独立 TUN 示例模块完整测试（`-count=1`）通过。

### 官方主线合入验证（2026-10-19）

本轮固定官方基线为 `5af90471bfe654d097f7e488a2699f2850db7dd2`，在原 `main` 上保留双方历史合入 9 个官方提交，保留本地命名共享网卡路由修复。日志保存在本地 `.test-env/upstream-merge-2026-10-19/`。

- Portal IPv4 客户端继承所属网络的 `[vpn]` 配置并跟随热更新。每个客户端独立维护路由排除和出口可达状态，保留身份、ACL、组网及代理路由优先、最长前缀及出口失联丢弃语义；不恢复旧 `exit_nodes`，不扩大 WireGuard `AllowedIPs`。
- Web 心跳通知采用无负载唤醒，协调任务读取最新会话快照；本地 TOML 观察任务同步适配，保留本地文件权威、revision/CAS 和中央配置投递。合入官方的接管配置版本防护及迟到回调隔离。
- 合入 Windows NSIS 升级进程清理及服务退出状态、完整启动错误链、CLI 宽松布尔值、BPF 和共享 TUN 测试隔离及安全数据报宽限期测试修复。保持已退休 OHOS 工作流删除和定制构建矩阵。
- 本轮官方 schema 未改变，保留既有字段编号。Go WASM、绑定及来源记录通过现有生成器从干净源码提交重建，具体提交与摘要以 `provenance.go` 为准。

本轮实际验证：

- Windows 默认功能集的 GUI 后端 28 项、Web 后端 362 项通过（Web 原有 2 项默认跳过）；前端三组共 450 项测试及生产构建通过，CI 脚本 64 项通过。
- protobuf 库及 descriptor/字段归属检查共 44 项通过、2 项默认跳过；本轮没有改动 schema 或重新分配字段。
- Linux `full` 首轮网络回归 44 项全部通过，覆盖 Portal 普通/Secure Mode、配置订阅、ACL、并行池、BPF、共享 TUN 和双栈 DHCP。
- Linux 补充的 WireGuard Portal 5 项、共享网卡 61 项、Core 并行策略 20 项、监听器 20 项和安全数据报 9 项通过。Native 并行传输首轮 24 项通过、2 项默认跳过，另有 1 项 WSS IPv6 突发收包超时；编译结束后该单例独立复跑 3 轮均通过。首次失败及复跑日志分别保留在 `linux-followup-direct.log` 和 `wss-ipv6-recheck.log`，本轮未改动并行传输实现或断言。
- CLI 参数和配置单元回归 22 项、文件/stdin 配置与日志集成检查 6 项通过；默认启动错误日志核对实例 ID、监听 URL 和完整绑定失败错误链。
- Windows 共享网卡 60 项通过，包含命名网卡的 IPv4/IPv6 connected route 生命周期；1 项真实 TUN 测试按原配置跳过，要求管理员权限及 `wintun.dll`。
- Windows Core/原生/Web/GUI 默认功能集，以及 Linux Core/原生 `full` 全部目标严格 Clippy（`-D warnings`）通过；格式和 Core 无默认功能编译通过。
- Windows 安装脚本语法及隔离改名夹具的 2 项 GUI 场景通过，目标目录进程关闭、另一安装目录进程保留。本机已有运行服务且当前账户未提权，未执行真实服务更新、恢复、取消及管理员 TUN 重建；macOS/Android 构建与设备验收未在本机执行。
- 从干净源码合并提交 `73ffaece20a7ba435e6ad47fb744598b5a936238` 运行现有 `go generate ./...`，WASI release 构建及 Binaryen 131 优化通过；实际 WASM SHA-256 为 `1d6ad8fc089f2d5dd87a103115135e4dccfc25de7d5b673c4b41a67bad2e3bce`，与生成记录一致。
- 两份 Go 来源记录均指向该源码提交；protobuf 重新生成后绑定代码无差异，schema SHA-256 保持 `635670b2b1c8c9d91688328e4760b7c15bf8cfc20590cafdf0e26fb1864c63f1`，仅更新来源提交。
- 使用新 WASM 的 `go test -p 1 ./...` 全部包测试通过，包含 artifact、engine、host 及集成测试；实际文件摘要、两份来源提交和 schema 摘要均已核对。

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
- 首轮远端 Go、JS 与 Linux CI 脚本测试通过。Linux 编译发现网卡配置的 trait 实现在合并后被拆成重复的两段，已合并为单一实现，并删除 Linux 共享网卡测试中的未使用导入；路由方法逻辑保持不变。WSL 下 `linux-netlink,ring-crypto` 最小特性库编译通过，完整 Linux 检查继续由 CI 执行。
- 后续 Linux Clippy 发现两个随 Magic DNS 移除后已无调用方的共享网卡测试辅助函数，已删除这些函数及其专用导入；现有测试和断言保持不变。WSL 原生包 `cargo clippy --locked -p easytier --all-targets --features full -- -D warnings` 复验通过。
- Linux 网络 CI 的公开中继场景仍依赖上游 `private_mode=false` 的默认值，与 Custom 默认私有中继不符；现仅在这些测试节点上显式允许公开中继，保留生产准入策略和凭据拒绝、撤销、唯一归属、重连及限速断言。使用原 CI 二进制复现共享凭据和跨网络连接失败后，修复源码在 WSL 下的 159 项回归全部通过，包括 128 个公开中继子网代理组合。
- DHCP 双栈测试补齐 IPv4 租约唯一性及所有节点的双栈路由传播检查，沿用原 15 秒准备期限和 TCP 数据断言。CI 曾出现一次 IPv4 TCP 连接拒绝，原二进制在本机三场景和双栈 10 次重复均通过，故不将具体竞态原因记为已完全复现；修复后的三场景随上述 159 项通过，双栈另行重复 10/10 通过，WSL 临时调整的 bridge netfilter 参数已恢复。
- Android 四架构（aarch64、armv7、i686、x86_64）完整 APK 构建和后续 ARM64 编译检查均通过，临时启用的 Mobile 工作流已恢复为 `disabled_manually`。完整构建来源为 `a9c342ee`，后续变更仅涉及 Linux 代码、测试辅助函数和文档，Android 生产构建输入未变。
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
