# 与官方差异的重构评估（2026-11-04）

## 结论摘要

- 定制代码的约 56%（49,337 行）在新增文件里，这部分基本不产生上游合并冲突；真正要处理的是塞进 **311 个上游文件**的 39,242 行新增和 8,632 行删除。
- 按 hunk 分散度看，**97 个上游文件有 10 个以上 hunk**，118 个只有 1–3 个。合并成本集中在最前面的 6 个文件（约 8,900 行 churn）。
- 最便宜的一刀是**把内联测试搬出官方文件**：批次 1 已实测搬出 3,864 行、约 42 个 hunk，行为完全不变，涉及 `service.rs`、`client_manager/mod.rs`、GUI `lib.rs`、`composition.rs`、core `instance/tests.rs`、web `main.rs`。
- 第二刀是**把整块自定义实现外移成子模块**：`peer.rs` 顶部 570 行调度器、`process_rpc.rs` 约 600 行配置目录生命周期、`service.rs` 491 行控制台 API、`client_manager` 约 600 行地理位置逻辑、`virtual_nic.rs` 的 netns 装饰器、GUI 共享配置目录等，合计约 3,000 行。
- 第三刀是**去掉重复表达**：伪装策略有三份同义推导；两套 feature offer 编解码；路由有“事件 + 5 秒轮询”和 `VpnRoutePlan` 两套来源；能力有布尔和字符串两套声明；目录扫描有三份实现。
- 遗留包袱主要在 Web 前端和 CI/文档：`TemporaryDevices.vue` 仍是一整页官方网络详情；CI fixture 仍引用已删工作流；`go-artifacts` 自动 push 生成物制造提交噪音；9 份带日期的过期文档没有归档。
- **不建议**为了减少 diff 去动打洞状态机、HTTP3/WSS 帧处理、VPN 路由管理器本体、中央管理的 mesh 模块、proto 高位编号机制和已提交的 Go/WASM 生成物。这些是真实功能或协议兼容，抽离只会制造更差的边界。

## 进度

- 批次 1（内联测试外移 + 死代码）已完成，提交 `7ed8e217`。`cargo check --tests` 覆盖 easytier、easytier-core、easytier-web、easytier-gui；定向测试 service 38、client_manager 18、web 全量 407、GUI 4、composition 37、core 持久化管理 15 全部通过。`server_protocol_for_connected` 经复核**不能删**（两个 trait object 类型不同），评估文档已更正。
- 批次 2（整块实现外移）已完成，提交 `5cf55117`。peer 调度器 570 行、process 生命周期约 1,100 行、service 控制台 API 565 行、client 地理位置/变更守卫 600 行、GUI 共享配置 90 行移入新文件；上游文件新增行 35,426→32,619，hunk 3,788→3,753。定向测试 core 管理 89、peer 64、service 38、client 18、GUI 4 全部通过，`easytier --tests` 编译通过。
- 批次 3a（WebSocket 伪装头外移）已完成，提交 `48528934`。`websocket.rs` 约 45 行头装配移入 `websocket/disguise.rs` 的 `apply_client_headers`，30 项 websocket 测试通过。socket 保护包装复核后**撤销**（所有调用点都消费 registration）。`virtual_nic` 的 netns 装饰器经评估**暂缓**：它改的是 setns + spawn_blocking 的线程局部语义，当前环境无法跑 Linux netns 测试，风险高于可量化收益；需要 Linux 环境单独验证。
- 批次 3b（core 启动配置外移）已完成，提交 `fecb98a5`。`StartupConfigFile`/`StartupConfigs`/`prepare_startup_configs`/`start_prepared_configs` 移入 `core/startup.rs`，core.rs +6/−156；core::tests 23 项、config-dir startup 4 项通过。字段清单同步修正（提交 `99f2e6f1`）：117 条 = 91 迁移 + 26 原生高位，上游清单来源改为实际包含 `tld_dns_zone=73` 的 `4bdba58c`，4 项 descriptor 测试通过。
- 批次 3c（配置测试外移）已完成，提交 `cfc80d26`：`api_input.rs` 的 15 个定制测试（440 行）与 `toml.rs` 的 11 个（266 行）移入各自的 `tests/custom_tests.rs`，上游文件合计少 709 行；`config::` 129 项测试通过，测试总数保持 `api_input` 31、`toml` 46。
- 配置遗留修复（提交 `94a5a1b2`）：`browser-config` 下两个既有失败已修——显式 `accept_dns=false` 不再被表单合并删除；存在 `p2p_disguise_mode` 时，合并后的 TOML 会清掉三个旧 `wss_http3` 布尔。`browser-config` 配置测试 135/135、默认 129/129。
- 批次 4a（protobuf 定制块拆分）已完成，提交 `bd987b27`：只把持久化配置块（`ObserveConfigs*`、`PersistedConfigEntry`、`PatchPersistedConfig*`、`PersistedConfigService`）拆到单向依赖的 `custom_manage.proto`；官方 `api_manage.proto` −66 行，官方 `api_manage.pb.go` 净 −526 行，package/wire 编号/Rust/Go API 不变。全量拆 `VpnConfig`/`NodeLocation` 会形成循环 import，已实测不可行。字段清单 4 项、core 编译、`go build ./...`、`go test ./proto/...` 通过；schema provenance 已按相同算法刷新（`059bb4ff`）。
- 累计：上游文件新增行 39,242→31,191（约 −20.5%），hunk 3,830→3,748，最热的 peer/service/process/client 四个文件已从上千行 churn 降到数百行。剩余大头是生成物（Go/WASM）、GUI 生命周期、hole punch、virtual_nic 和前端测试。
- 批次 4b（能力声明去重）已完成：删除 `ListNetworkInstanceResponse.supports_persisted_config_management`（保留 `reserved 50003`），web/OHOS 统一检查 `runtime_capabilities` 的 `management:persisted-config-revision-v2`；自定义字段清单 117→116（90 迁移 + 26 原生高位），Go 绑定与 schema provenance 同步。字段清单 4/4、core 编译、web client 18 项、Go build/test 通过；OHOS crate 在 Windows 上因既有 Unix-only import 无法编译，未验证。
- 批次 5a（文档/CI 遗留）部分完成，提交 `c3a0e7f6`：9 份带日期的过程/验证文档归档到 `docs/archive/`（只互相引用，相对链接保持有效）；CI 旧工作流 artifact 迁移 shim 加了 2027-01-31 的到期删除说明。
- 批次 4（其余重复表达）与批次 5（`TemporaryDevices.vue` 瘦身、`dashboard.test.mjs` 外移、剩余 CI 清理）待做。

## 一、比较口径

- 本地评估起点：`bbef67a9`（2026-11-04）；批次完成后的 HEAD 见“进度”。
- 官方基线：`git merge-base HEAD upstream/main` = `48ccc56a`（2026-10-27）。
- `upstream/main` 当前为 `3ed7fcec`，只比基线多 3 个提交：
  - `6487a5e4` peer_map 未测量连接不再报 1ms；
  - `4bdba58c` 官方也加了 `api.manage.NetworkConfig.tld_dns_zone = 73`；
  - `3ed7fcec` wintun 身份保持（只动 `Cargo.lock`/`Cargo.toml`）。
- 差异规模：526 个文件，`+88,745 / -13,436`。

| 类别 | 文件数 | 新增行 | 删除行 | 含义 |
| --- | ---: | ---: | ---: | --- |
| 新增文件 | 196 | 49,337 | 0 | 不产生同文件冲突，维护成本低 |
| 改写上游文件 | 311 | 39,242 | 8,632 | **主要合并成本** |
| 删除上游文件 | 18 | 0 | 4,658 | 11 个 workflow、3 个 nightly 脚本、2 份 docs、2 个前端/配置源文件 |
| 重命名 | 1 | 166 | 146 | `NetworkDetail.vue` → `TemporaryDevices.vue` |

311 个改写文件里：

| 每文件 hunk 数 | 文件数 | 处理建议 |
| --- | ---: | --- |
| 1–3 | 118 | 多数是字段/分支小改，随功能走，不必单独重构 |
| 4–9 | 96 | 选择性外移，优先看下列清单 |
| ≥10 | 97 | 合并冲突高发区，优先处理前 20 个 |

## 二、先分清三种“差异”

1. **合并面差异**：同一官方文件里散落的改写。它决定每次 merge 的冲突成本，但不一定代表行为差异。处理方式是外移到新模块/子模块，或在官方扩展点后挂钩。
2. **实现重复/兼容层**：同一语义有多份实现或两套声明。它既增加代码量，也容易产生不一致。处理方式是合并到一个真源。
3. **协议/行为差异**：定制功能本身，例如并行多连接、HTTP3 伪装、网段出口、持久化配置、中央网络 ACL。**这些不应为了 diff 好看而删除**；只应把接入点收窄。

下面的批次按这个顺序组织。

## 三、P0：零行为变化（先做，收益/风险比最高）

### 3.1 内联测试搬出官方文件

| 文件 | 测试区 | 实测搬出 | 做法 |
| --- | --- | ---: | --- |
| `easytier-web/src/central_network/service.rs` | `service.rs:2017` 起 `mod tests` | 16 个定制测试 + 3 个 helper，1,055 行；hunk 108→68 | 新建 `central_network/service/tests/custom_tests.rs`，官方测试留在原处 |
| `easytier-web/src/client_manager/mod.rs` | `mod.rs:1264` 起 | 10 个定制测试 + 6 个 helper，602 行；hunk 52→50 | 新建 `client_manager/tests/custom_tests.rs` |
| `easytier-gui/src-tauri/src/lib.rs` | `mod managed_restore_tests`（559–658） | 4 个测试，98 行 | 新建 `src-tauri/src/managed_restore_tests.rs` |
| `easytier/src/instance/composition.rs` | 文件末尾追加 | 1 个参数化测试，1,305 行 | 新建 `instance/composition/tests/custom_tests.rs` |
| `easytier-core/src/instance/tests.rs` | `portable_runtime` 内追加 | 15 个测试，666 行 | 新建 `instance/tests/portable_runtime/persisted_management.rs` |
| `easytier-web/src/main.rs` | 文件末尾测试模块 | 8 个测试，136 行 | 新建 `src/tests/main.rs`，用 `#[path]` 挂载 |

合计 **3,864 行、约 42 个 hunk**。这不会减少仓库总 diff，但把 6 个上游文件里的新增行从 39,242 降到 35,426，hunk 从 3,830 降到 3,788。仓库已有先例：`instance/mod.rs` 把 2,606 行测试放在 `persisted_config_tests.rs`。

实现注意：测试里的 `super::*` 私有 helper（例如 `service.rs` 的 `generated_network_name`）需要 `pub(crate)`，或留在原文件由测试 `use super::...`。实际搬迁中 `service.rs` 的两个基线测试在 HEAD 里早已被定制版替换，替换版按定制测试移出，官方文件保留 22 个官方测试。

### 3.2 删除死代码和推测性抽象

- `easytier/src/instance/composition.rs:247-254`：`server_protocol_for_connected` 与 `server_protocol` 由同一个 `runtime_server_protocol_upgrader` 构造，注释称 “currently identical”。**复核后确认不能删**：两个字段的 trait object 类型分别是 `ServerProtocolUpgrader<HostAcceptedTcpSocket<H>>` 和 `ServerProtocolUpgrader<<H as VirtualTcpSocketFactory>::Socket>`，同一个 concrete upgrader 必须被调用两次并分别强转，不能共用一个 `Arc`。“identical” 指的是实现逻辑相同，不是同一个对象。保留现状，并在注释里说明类型原因。
- `easytier/src/tunnel/protocol/adapters/websocket.rs:76-80,105-109`：`unwrap_or_else(|| "speedtest")` 不可达，`WebSocketAdapter` 只在 `client_adapter`/`server_adapter` 构造，且都传 `Some(global_ctx)`。已改为 `expect(...)`，减少两处兜底。
- `easytier-core/src/peers/conn/peer.rs:403`：`disguised_tunnel_scheme` 带 `#[cfg(test)]` 且只被 `peer/tests.rs` 使用，移进测试文件。

### 3.3 需要产品确认的两处“兼容层”

- `easytier-core/src/config/toml.rs:530-586`：三个旧伪装布尔仍会在加载时迁移成 `p2p_disguise_mode`，但 `[vpn] exit_enabled` 已经是硬拒绝（`config/vpn.rs:278` 附近有测试）。两种遗留策略不一致。若确认没有在网旧 TOML，删迁移可少约 56 行和一组测试；否则把两者都保留，不要只留一半。
- `easytier-web/src/migrator/m20261008_000011_local_config_snapshots.rs` 建表、`m20261026_000013_drop_local_config_snapshots.rs` 删表。对已经部署过的库这是必要的升级路径，**不建议压平**；如果确认从未发布过该表，才能合并迁移。

## 四、P1：整块自定义实现外移（行为不变）

技术前提：Rust 允许在**子模块**里写 `impl` 并访问父模块私有字段。`service.rs`、`process_rpc.rs`、`peer.rs` 都是 `foo.rs` 形式，可以直接声明 `mod xxx;` 并放到 `foo/xxx.rs`，无需改成 `foo/mod.rs`。仓库已有 `peer.rs` + `peer/tests.rs` 的先例。

| 文件 | 要外移的块 | 行数 | 目标 | 预计 hunk |
| --- | --- | ---: | --- | ---: |
| `easytier-core/src/peers/conn/peer.rs:41-612` | `ParallelSendState`/`LaneModel`/`SendPlan`/`SendPlanCache`/`compute_send_plan`/`parallel_members`/连接选择/关闭事件 | 570 | `peer/parallel_send.rs`、`peer/selection.rs`、`peer/close_events.rs` | ~15 |
| `peer.rs:1021-1431` | `send_plan`、`send_striped`、`parallel_connection_status`、`reserve_parallel_slot*`、`has_connection_at_least_as_preferred` | ~290 | 随字段收进 `Peer` 的 `Arc<ParallelTransport>`；`impl` 放子模块 | 同上 |
| `easytier-core/src/management/full/process_rpc.rs:305-1600` | `load_saved_config`、`verify_lifecycle_file_locked`、`stop_instances_locked`、`ensure_overwritable`、`apply_file_cleanup`、`rollback_started_instance`、`save|set_enabled|remove_with_revision` 等新增私有方法 | ~600 | `process_rpc/config_dir_lifecycle.rs` | ~12 |
| `process_rpc.rs:1959-2041` | 3 个自定义 `WebClientService` RPC | 82 | `management/full/persisted_rpc.rs`（已有同型 impl） | 1 |
| `easytier-web/src/central_network/service.rs:567-1090` | 14 个控制台 API（`apply_core_change` 等）+ `NetworkAclView` 等 view | ~560 | `central_network/service/console_api.rs` | ~4 |
| `easytier-web/src/client_manager/mod.rs:211,586,984` | `network_info_cache`、`lookup_ip_location`、`runtime_mutation_guard` 三块 | ~600 | `client_manager/geo.rs` | 3 |
| `easytier/src/instance/virtual_nic.rs:754-950` | 15 个 `IfConfiguerTrait` 方法改为 netns `configure_interface` 调用 | ~200 | 新建 `NetNsIfConfiger` 装饰器实现同一 trait，官方文件只改构造处 | ~15 |
| `easytier-gui/src-tauri/src/lib.rs:466-522` | `shared_config_dir`、`gui_machine_id`、`restore_managed_configs` | 256 | `src-tauri/src/shared_config.rs` | 1 |
| `easytier/src/core.rs:1285-1437` | `StartupConfigFile`、`prepare_startup_configs`、`start_prepared_configs` | 154 | 已存在的 `instance/config_dir_startup.rs` | 1 |
| `easytier/src/tunnel/websocket.rs:283-350` | SNI/UA/Accept/Accept-Language/Cache-Control/padding 头装配 | ~70 | 已存在的 `websocket/disguise.rs` | 2 |
| `easytier-core/src/peers/peer_manager.rs:796-873` | `collect_vpn_route_exclusions` 纯集合运算 | 78 | `peers/vpn_policy.rs` | 1 |

合计约 **3,000 行**。外移后的收益不是“总行数变少”，而是这些官方文件里的新增块变成一两行调用；上游改同一函数时，冲突从“整块重复”缩成“调用点”。

两个需要特别说明的块：

- `peer.rs` 顶部 570 行是纯自由函数，只依赖 `ConnMap`/`Flags`/close sender，最容易搬。搬的时候把 `compute_send_plan` 里 `receiver.set_max_rtt_us` 的接收侧副作用一起带进调度模块，否则计算函数仍被接收器污染。
- `process_rpc.rs` 里既有新增方法，也有对官方方法的改写。只搬**新增的私有方法**和 3 个新 RPC；官方 `run/retain/delete_network_instance` 的函数体保留在原文件，用 `self.xxx_locked(...)` 调用外移实现。这样 hunk 能从 65 降到约 25 以内。

## 五、P2：合并重复实现、恢复官方结构

### 5.1 伪装策略三份同义推导

- `easytier-core/src/peers/conn/peer.rs:411` `use_disguise_preference`
- `easytier-core/src/peers/conn/cleanup_policy.rs:267` `agreed_disguise`
- `easytier-core/src/config/mod.rs:400` `negotiated_disguise`

三处都在用 `(mode, negotiated, peer_flags)` 推导“是否伪装”，且 `peer.rs:344-345` 同时调用前两者做一致性判断。收敛到一个 `P2pDisguisePolicy` 方法，约少 20–40 行，并消除三处漂移的风险。同时把打洞路径里仍读旧三个布尔的地方（`hole_punch/tcp.rs` 约 1018、1642）改成读模式；旧布尔只保留在 TOML 加载迁移一层。

### 5.2 两套 feature offer 编解码

`peer_conn.rs:494-512` 的 `handshake_features` 拼四种前缀，解析散在 `peer.rs:130/907`、`cleanup_policy.rs:81`、`parallel.rs:225`、`peer_conn.rs:1103/1292/1636`。`CleanupOffer` 和 `ParallelOffer` 都是“版本化字符串 + 字段解析”。抽一个 `PeerFeatures` codec（注册前缀、解析、序列化），约少 60 行，也避免新增第三种能力时再复制一套。

### 5.3 路由两套算路

- `easytier/src/instance/virtual_nic.rs:96-105` 的 `overlay_routes()` 手写 `subtract_cidrs`；
- `easytier-core/src/config/vpn/routes.rs:110-115` 的 `compile_vpn_routes` 已有 overlay 分支；
- `virtual_nic.rs:1516-1575` 把官方的事件过滤 + `proxy_cidr_diff` 换成“任何事件全量重算 + 每 5 秒轮询”，并且 `let _ = event;` 丢弃事件。

建议：给 `VpnRoutePlan` 增加 `overlay_routes` 字段，桌面 updater 和 `mobile_tun_claims`（`virtual_nic.rs:1909`）都用同一份 plan；恢复 `CoreEvent::ProxyCidrsUpdated` 过滤和 `proxy_cidr_diff`（`packet_plane.rs:74` 仍在），去掉 5 秒轮询。这样既减少改写，也避免空转唤醒和“事件来了却不用”的怪逻辑。这是行为上的小优化，需 Linux netns 测试确认。

### 5.4 `protect_native_socket` 的签名（复核后不改）

`easytier/src/socket_protector.rs:331-352` 返回 `Option<Arc<SocketRegistration>>`，看起来是“签名扩散”，但复核发现 **6 个调用点全部消费 registration**：`socket/tcp.rs:262,344` 把它交给 `ProtectedSocket::new`，`tunnel/common.rs:242` 同样保留保护生命周期，`gateway/icmp_proxy.rs:45` 与 `instance/vpn_routes.rs:1528,1557` 用它维持 socket 作用域。`socket/tcp.rs:480` 的测试还断言 `registration` 必须为 `Some`。因此原签名是正确的，不能改成丢弃返回值；本项从重构清单移除。

### 5.5 能力声明的双表达

- `web.proto:28-32` 的 `support_local_configs` / `support_local_config_revision` 布尔，与 `runtime_capabilities` 字符串并存；前端在 `configStore.ts:62`、`localConfigPatch.ts:67` 用 OR 兜底。
- `api_manage.proto` 的 `supports_persisted_config_management` 已删除（批次 4），`runtime_capabilities` 成为唯一表达。

`runtime_capabilities` 作为单一真源：`supports_persisted_config_management` 已删除，直连 list 响应与心跳共用能力字符串。`web.proto` 的 `support_local_configs` / `support_local_config_revision` 仍与能力字符串并存，前端 OR 兜底保留，作为后续项。

### 5.6 三份目录扫描器

- `easytier/src/instance/config_dir_startup.rs:96-160`（CLI 启动）
- `easytier-core/src/management/full/local_catalog.rs:259-475`（catalog）
- `easytier-core/src/management/full/local_reconcile.rs:111+`（热协调）

三处都实现“列文件、取 identity、跳过非法/重复 UUID”。只抽共享的“列举 + identity + 去重”，不要合并副作用：启动路径有“无效文件不触发停用状态清理”的专门语义（`config_dir_startup.rs:64-77`）。同时 `process_rpc.rs:1766-1796/1918-1930` 的 disabled 集合有 catalog 和 state_store 两个来源，统一成一个视图，避免“是否禁用”三种答案。

### 5.7 官方 ACL 编译器的去留

`central_network/compiler.rs` 删掉了官方的 `compile_acl`/`validate_acl` 等约 423 行，改成新文件 `network_acl.rs` 的 `compile_with_acl`。这是有意的语义替换（网络链、设备组、凭据组），**不建议硬搬回官方实现**。但可以评估：若自定义 `NetworkAcl` 能无损映射到官方 `NetworkConfig.acl`，复用官方编译和校验能少 200+ 行；若映射不无损，就保持现状，并在 `compiler.rs` 注释里写清为什么不能用官方函数，避免后来者误恢复。

### 5.8 `P2pPolicyFlags` 的派生布尔

`easytier-core/src/config/mod.rs:321-328` 存了 4 个布尔，其中 `prefer_http3_for_p2p` 完全由 `default_protocol` 派生（`peers/peer_manager.rs:1620`）。改成存 `(P2pDisguiseMode, default_protocol)`，调用点（`hole_punch/udp/task.rs:53`）机械替换。约少 20 行和一类“两个字段互相矛盾”的配置。

## 六、P3：遗留包袱

### 6.1 `TemporaryDevices.vue` 仍是整页官方网络详情

`easytier-web/frontend/src/components/network/TemporaryDevices.vue` 是官方 `NetworkDetail.vue` 的 R088 改名，1,678 行，由 `NetworkTemporary.vue` 挂载；它仍渲染 `members`/`settings`/`credentials` 等完整 tab，带 `settingsForm`、`configForm`、节点详情。新页面 `NetworkSettings.vue`、`NetworkMembers.vue`、`NetworkAcl.vue`、`NetworkAdvanced.vue` 已经覆盖同样职责。建议只保留临时设备表格/创建/删除，其余删除，预计少约 1,000–1,200 行。这是本批最大的一块前端遗留。

### 6.2 CI 与生成物

- 官方 14 个工作流文件 → 5 个 workflow yml（另有 `Dockerfile`）是**有意合并**（`checks.yml` 已覆盖 go/js/nix/android；OHOS 工作流按 `custom-validation.md` 已退休），不建议恢复官方 `core.yml`/`gui.yml`/`mobile.yml`，否则会重复触发。
- 残留的迁移 shim：`.github/scripts/prune-build-artifacts.cjs:4-10` 仍硬编码 `core/gui/mobile/custom-static/ohos` 五个已删工作流；`builds.test.js:104`、`prune-build-artifacts.test.cjs:25-33` 的 fixture 仍用旧文件名。清掉并加一条“新 workflow 上线后删除旧路径”的注释。
- `go-artifacts.yml` 会在 main 上自动重新生成并 push 回 `build(go): regenerate ...` 提交（历史上已有 8 个）。改成漂移检测（`git diff --exit-code` 失败即红），生成仍由人或合并时做一次。减少提交噪音，不改变产物。
- Go 的 `api_manage.pb.go` +1,645/−310 和 6.65MB `easytier_core.wasm` 不能靠“构建时生成”消掉：官方同样提交生成物，且这些行 100% 来自自定义 proto 字段/消息。**不要把生成物移出仓库**。
- 若要把 Go 生成文件的 diff 再缩小，可把整块自定义消息/服务（`PersistedConfigEntry`、`PatchPersistedConfig*`、`PersistedConfigService`、`VpnConfig`、`NodeLocation` 等）搬到同 package 的 `custom_manage.proto`；Rust 按 package 生成，不受影响；Go 会多一个 `custom_manage.pb.go`，官方 `api_manage.pb.go` 只剩 `NetworkConfig` 字段增量。`api_instance.proto` 的 `VpnRuleStatus` 同理。注意同步 `easytier-proto/build/main.rs` 的 proto 列表和 `easytier-proto/src/api.rs` 的再导出。

### 6.3 proto 字段清单

- `api_manage.proto:53` 的 `tld_dns_zone = 73` 现在**确实是官方字段**：`upstream/main` 的 `4bdba58c` 也加了同一字段同一编号，合并时不会冲突，不要改成 50,173。
- 但 `easytier-proto/tests/upstream-fields.json` 头部仍写来源 `99db92a`，而该提交没有这个字段；清单不可复现。用当前 `upstream/main` 重新生成上游清单，并把 `field_numbers.rs` 的“custom ≥ 50000”断言保持为硬规则即可。
- `docs/CUSTOM_CHANGES.md:42` 写“迁移 100 个、保留 9 个”，实际清单 116 条（90 条低→高 + 26 条原生高位）。改由脚本生成计数，别再手写。
- 已删功能的 reserved（`api_manage.proto:371-372` 的 `50011 pending_apply`、`api_instance.proto:435-452` 的旧 VPN 字段）**要保留**：它们防止编号复用，属于正确的线协议卫生，不是包袱。

### 6.4 文档

`docs/` 下有 9 份带日期的过程文档（`ci-failures-2026-10-09`、`official-feature-restore-plan-2026-10-28`、`kcp-*`、`tcp-proxy-*`、`vpn-*validation-2026-10`）。建议移到 `docs/archive/`，只保留 `CUSTOM_CHANGES.md`、`core-architecture.md`、`custom-validation.md` 和仍有效的设计文档，减少“看起来仍在维护”的噪音。被删的两份官方 plan docs（`data-plane-runtime-plan.md`、`easytier-web-managed-config-sync-plan.md`）如果只是过时，可以恢复以免制造无意义的删除差异；如果确认已被新文档取代，在 `CUSTOM_CHANGES.md` 里写一句即可。

### 6.5 不建议仅为差异而回退的行为

- `service.rs` 的 per-user `mutation_lock`（14 个调用点）确实制造了散点改写，但它让慢设备操作不再阻塞其他账号，是行为改进。若一定要回退，改成“锁内按 user 分片、调用点仍叫 `self.mutation_lock.lock()`”即可；不值得为了 14 个 hunk 放弃隔离。
- `config_server: Vec<String>`、4 个新 CLI flag、`traffic_disguise` 默认注入：这些是真实的 CLI/行为契约，冲突必现，保持现状。

## 七、不建议动的部分

- `easytier-core/src/connectivity/hole_punch/tcp.rs`（+918/−370，85 hunk）、`udp/*`、`direct/mod.rs`、`manual/mod.rs`：端口预测、原生 HTTP3、伪装协商与官方状态机深度交织。硬抽会把状态机切成两半，风险大于收益。
- `easytier/src/tunnel/websocket.rs` 的帧处理与 `http3.rs`：真实协议实现。只把“头装配”和“帧编解码”外移（`websocket/codec.rs`、`disguise.rs`、`http3.rs` 已经在做），不要重写状态机。
- `easytier/src/instance/vpn_routes.rs`（1,774 行）与 `vpn_exit_forwarding.rs`（1,384 行）：虽大，但与官方只有 4 个钩子（`packet_plane.rs:32-38`、`peer_manager.rs:1961`、`vpn_policy.rs:179`、`composition.rs:276`），出口选择仍在官方 `outbound_packet_router`。这是“新文件 + 薄钩子”的正确形状。
- 中央管理的 6 个新模块（`core_sync`、`mesh_network`、`network_acl`、`delivery`、`local_delivery`、`advanced`）：已经用 `&Db`/`&ClientManager` 解耦，`service.rs` 只是门面。只做上面的门面外移，不要重写。
- proto 高位编号 + 清单测试、已提交的 Go/WASM 生成物：前者保护线协议，后者是官方也采用的发布方式。不要为了 diff 停止提交或改编号。

## 八、收益估算

| 指标 | 原始 | 批次 1 后 | P1 后（估） |
| --- | ---: | ---: | ---: |
| 改写上游文件的新增行 | 39,242 | 35,426 | ~30,000 |
| 改写上游文件的总 churn | ~47,900 | ~44,100 | ~38,000 |
| 前 6 大文件 churn | ~8,900 | ~7,400 | ~4,900 |
| hunk 总数（改写文件） | 3,830 | 3,788 | ~3,700 |
| 仓库总新增行 | 88,745 | 89,038 | 88,500–89,000 |

说明：

- 批次 1 是**搬家**：仓库总新增行略增（新文件多了模块声明和注释），但上游文件里的新增行少了 3,816 行，前 6 个文件的 churn 少了约 1,500。
- P1 继续外移整块实现后，上游文件新增行预计再少约 5,000，前 6 大文件 churn 回到约 4,900。
- P2/P3 才真正减少实现体量（重复逻辑、`TemporaryDevices.vue`、CI shim、过期文档），预计净少 2,000–3,000 行，且不删功能。
- hunk 数的下降看起来不惊人，是因为 118 个文件只有 1–3 个 hunk；真正的收益在“高 churn 文件回到接近官方形状”。

## 九、建议批次与验证

### 批次 1（已完成，`7ed8e217`）

1. 搬 6 处内联测试（service、client_manager、GUI lib、composition、core instance tests、web main）。
2. 删 websocket adapter 的不可达默认分支、`disguised_tunnel_scheme` 测试专用函数（后续批次）；`server_protocol_for_connected` 复核后保留。

验证：4 个 crate `--tests` 编译；service 38、client_manager 18、web 全量 407、GUI 4、composition 37、core 15 通过；`cargo fmt --check` 通过。

### 批次 2（数据面，最热路径）

1. `peer.rs` 顶部 570 行 → 子模块；`Peer` 的并行字段收成 `Arc<ParallelTransport>`。
2. 伪装策略三份推导合并。
3. 两套 feature offer 合并成 codec。
4. `peer_manager` 的 VPN 排除函数移入 `vpn_policy`。

验证：并行/重排/带宽相关测试连续 15 轮；composition 端到端（WSS/HTTP3/并行池）3 轮；严格 Clippy。

### 批次 3（管理面与原生运行时）

1. `process_rpc` 生命周期方法 + 3 个 RPC 外移；官方方法体只留调用。
2. `service.rs` 491 行控制台 API + view 外移。
3. `client_manager` 三块地理位置逻辑外移。
4. `virtual_nic` 的 `NetNsIfConfiger` 装饰器。
5. GUI `shared_config.rs`；`core.rs` 启动配置外移 + CLI flatten；`websocket` 伪装头下沉；`protect_native_socket` 薄包装。

验证：core 完整库测试、web 后端、GUI、Linux netns 路由测试；Windows 下确认 netns 装饰器不影响编译。

### 批次 4（路由与去重）

1. `VpnRoutePlan` 增加 overlay 字段，恢复 `ProxyCidrsUpdated` + `proxy_cidr_diff`，去掉 5 秒轮询。
2. 目录扫描共享“列举 + identity + 去重”；disabled 集合单一视图。
3. 能力声明去布尔，保留 `runtime_capabilities`；前端删 OR 兜底。
4. `P2pPolicyFlags` 收敛为 `(mode, default_protocol)`。

验证：Linux 路由/策略表测试、移动端 shared NIC 测试、旧版能力协商测试、Web/GUI 表单回归。

### 批次 5（遗留清理）

1. `TemporaryDevices.vue` 瘦身。
2. CI fixture 旧工作流名、`prune-build-artifacts` 迁移 shim、`go-artifacts` 改漂移检测。
3. 归档带日期文档；修 `CUSTOM_CHANGES.md` 计数；用 `upstream/main` 重新生成字段清单。
4. 产品确认后再决定 TOML 旧布尔迁移是否删除。

验证：前端构建与测试；CI 在 PR 上跑通；生成物无漂移；文档链接检查。

## 附：复查命令

```sh
mb=$(git merge-base HEAD upstream/main)
git rev-list --left-right --count HEAD...upstream/main
git diff --shortstat "$mb" HEAD
git diff --name-status -M "$mb" HEAD | cut -f1 | sort | uniq -c
git diff --numstat -M "$mb" HEAD | sort -k1,1nr | head -30
git diff -U0 "$mb" HEAD -- <file> | grep -c '^@@'
```

判断块是否可外移的两个快速检查：

1. 这段代码是否只被 `Peer`/`CentralNetworkService` 自己的方法调用？是 → 可以放子模块。
2. 它是否依赖父模块的私有字段？依赖 → 用 `foo.rs` + `foo/bar.rs` 的子模块形式，而不是兄弟模块；子模块可以访问父模块私有项。
