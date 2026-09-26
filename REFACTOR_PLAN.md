# EasyTier 魔改版 重构执行计划（移交文档）

> 本文档由代码审查线移交，供独立执行。所有 file:line 锚点已经过逐一核实（以 2026-09 工作区为准）。
> 配套的缺陷修复（F1-F14）由另一条线并行实施，见文末"与修复线的冲突协调"。

## 前置状态

- 工作区可能有未提交修改（历史上有 `easytier-core/src/connectivity/hole_punch/tcp.rs`、`instance/tests.rs`、`management/full/process_rpc.rs` 的 WIP）。**动手前 `git status` 确认，勿覆盖他人未提交内容。**
- 上游对比基线：`0a783c8e`（merge 提交 `d7a0d568` 的第二父，即上游 main）。所有"魔改新增"的判定以此为准：`git diff 0a783c8e..HEAD -- easytier-proto/proto/`。
- 已拍板决策：**本 fork 产物未对外分发 → proto 重编号立即执行**（任务 0）。

---

## 红线约束（必读，优先级高于一切重构项）

**上游主线：https://github.com/EasyTier/EasyTier**。本仓库是它的魔改 fork；重构的目的是让魔改部分更好维护，**绝不允许顺手重写上游的干净代码**——那会在下次合并上游时制造大量无意义冲突。

### 判定方法（hunk 级，不是文件级）

```bash
git diff 0a783c8e..HEAD -- <file>   # 0a783c8e = 最近一次合并的上游 main
```

- **diff 覆盖的行 = fork 责任区**（魔改新增/修改），可以自由重构。
- **diff 之外的行 = 上游文本**，必须保持字节级不变（包括空行、注释、格式——格式漂移同样是合并冲突源）。
- **函数级判定**：若某函数整体落在 fork diff 内（魔改新增或整体重写），视为 fork-owned，可自由拆分重组；若函数只有部分 hunk 属于魔改，只整理魔改 hunk，**不重组上游的控制流骨架**。

### 硬规则

1. 重构只发生在 fork 责任区；需要上游文件"配合"时（如把一个类型 `pub` 化、加一个钩子调用点），改动最小化，并在该 hunk 内加 `// fork:` 注释标注，方便日后 rebase 时辨认。
2. 抽出的共享代码放**新建的 fork-owned 文件**（模块 doc 注释以 `Custom (fork):` 开头），不把上游代码搬进共享模块再"顺理成章"地改写。
3. 宁可保留一份上游侧拷贝，也不为消除重复去改写上游文本（见 D6/D7 的重新划定）。**去重收益 < 合并冲突成本时，放弃该项。**
4. 每步验收追加一条：对涉及上游共享的文件，重构后 `git diff 0a783c8e..HEAD -- <file>` 中上游区域的 hunk 不得增加（理想是减少）。总指标：**将来 rebase 上游 main 时的冲突面单调不增**。

### 可维护性要求（与红线同级）

- **行为不变**：除任务 0 的 wire tag 外，任何重构不得改变行为。发现 bug 只记录移交（标注文件:行），严禁混入重构提交。
- **小步提交**：一个计划项一个提交，提交信息引用编号（如 `refactor(d6): ...`），可独立 review 与回滚。
- **命名与注释跟随所在文件现状**；注释只写"为什么"（约束、坑），不写"改了什么"。
- **抽象克制**：≥2 处真实重复才抽公共层；抽出的东西要有准确的名字；不为一处使用建间接层。
- 每步跑与修复线相同的回归：`cargo test --workspace` + `cargo clippy` + `go build/test`（涉及时）+ `vue-tsc`（涉及时）。

---

## 任务 0（最高优先）：proto 魔改字段重编号到 50001+ 高位

### 原理与约定

- protobuf 字段 tag 就是 wire key，两侧（Rust prost / Go pb.go / wasm 内嵌 core）必须一致，改完必须同步再生成全部产物。
- 本仓库已有合规先例：`web.proto` 的 `support_local_configs = 50001`（带注释 "Custom-only capability outside the upstream field range"）。全部魔改字段照此办理。
- **每个 message 内从 50001 顺次分配**（按现有 tag 升序对应），保持注释原样。
- **不要加 `reserved`**：我们腾出官方顺位 tag 正是为了让上游将来可以自由使用，reserved 反而会造成合并冲突。
- 新增的 message/enum 类型（如下清单未列出的）是全新类型，内部 tag 不占上游槽位，保持不动。

### 字段重编号完整清单（已逐条 diff 上游核实）

| 文件 | message | 字段 | 现 tag | 新 tag |
|---|---|---|---|---|
| common.proto | FlagsInConfig | only_use_wss_http3_for_hole_punching | 45 | 50001 |
| common.proto | FlagsInConfig | prefer_wss_http3_for_p2p | 46 | 50002 |
| common.proto | FlagsInConfig | disable_wss_http3_for_p2p | 47 | 50003 |
| common.proto | FlagsInConfig | enable_bbr | 48 | 50004 |
| common.proto | FlagsInConfig | close_redundant_conns_when_disguised | 49 | 50005 |
| common.proto | PeerFeatureFlag | prefer_wss_http3_for_p2p | 12 | 50001 |
| common.proto | PeerFeatureFlag | disable_wss_http3_for_p2p | 13 | 50002 |
| common.proto | PeerFeatureFlag | only_use_wss_http3_for_p2p | 14 | 50003 |
| api_manage.proto | NetworkConfig | sni | 73 | 50001 |
| api_manage.proto | NetworkConfig | only_use_wss_http3_for_hole_punching | 74 | 50002 |
| api_manage.proto | NetworkConfig | prefer_wss_http3_for_p2p | 75 | 50003 |
| api_manage.proto | NetworkConfig | disable_wss_http3_for_p2p | 76 | 50004 |
| api_manage.proto | NetworkConfig | enable_bbr | 77 | 50005 |
| api_manage.proto | NetworkConfig | p2p_prefer_protocol | 78 | 50006 |
| api_manage.proto | NetworkConfig | close_redundant_conns_when_disguised | 79 | 50007 |
| api_manage.proto | ListNetworkInstanceResponse | disabled_inst_ids | 2 | 50001 |
| api_manage.proto | ListNetworkInstanceResponse | supports_persisted_config_management | 3 | 50002 |
| peer_rpc.proto | GetIpListResponse | udp_http3_listeners | 6 | 50001 |
| peer_rpc.proto | SelectPunchListenerRequest | scheme | 3 | 50001 |
| peer_rpc.proto | SelectPunchListenerRequest | native_http3 | 4 | 50002 |
| peer_rpc.proto | SelectPunchListenerResponse | scheme | 2 | 50001 |
| peer_rpc.proto | SelectPunchListenerResponse | native_http3 | 3 | 50002 |
| peer_rpc.proto | SendPunchPacketBothEasySymRequest | scheme | 6 | 50001 |
| peer_rpc.proto | SendPunchPacketBothEasySymRequest | native_http3 | 7 | 50002 |
| peer_rpc.proto | SendPunchPacketBothEasySymResponse | scheme | 3 | 50001 |
| peer_rpc.proto | SendPunchPacketBothEasySymResponse | native_http3 | 4 | 50002 |
| peer_rpc.proto | TcpHolePunchRequest | scheme | 2 | 50001 |
| peer_rpc.proto | TcpHolePunchRequest | supports_port_prediction | 3 | 50002 |
| peer_rpc.proto | TcpHolePunchResponse | scheme | 2 | 50001 |
| peer_rpc.proto | TcpHolePunchResponse | predicted_ports | 3 | 50002 |

新类型不动：`PortSequenceDirection` 枚举、`TcpHolePunchPredictedPorts`、`SaveNetworkInstanceConfigRequest/Response`、`SetNetworkInstanceEnabledRequest/Response`、`RemoveNetworkInstanceConfigRequest/Response`（内部 tag 全新）。

### WebClientService 追加的 3 个 RPC 方法 → 迁独立服务

**机制（已核实）**：
- RPC 方法索引 **1-based、按服务独立**：`easytier-proto/build/rpc.rs:86` `Method::new((i + 1) as u8, method)`；分发按 (service, index)，错误 `RpcError::InvalidMethodIndex(index, service_name)`（`easytier-proto/src/rpc_types/error.rs:19`，查找逻辑 `rpc_types/handler.rs:39`）。
- 上游 WebClientService 有 8 个方法（索引 1-8），魔改追加 SaveNetworkInstanceConfig/SetNetworkInstanceEnabled/RemoveNetworkInstanceConfig（索引 9/10/11）。若上游未来加第 9 个方法，索引冲突且探测误判。
- **做法**：在 api_manage.proto 新增独立 service（建议名 `NetworkInstanceConfigService`），把 3 个 rpc 连同其 request/response message 移入；WebClientService 恢复与上游完全一致。迁后 3 个方法在新服务内索引为 1/2/3。

**必须同步更新的全部引用点（已 grep 核实）**：
1. `easytier-core/src/management/full/process_rpc.rs:19`（import）与 `:1122` `impl WebClientService for ProcessManagementRpc` —— 把 3 个方法的实现移到新的 `impl NetworkInstanceConfigService for ...` 块。
2. `easytier-core/src/management/full/remote_client.rs:9,28` —— `Box<dyn WebClientService>` 客户端句柄需同时暴露新服务（再加一个 boxed trait 或合并为带两组方法的内部结构，按现有模式扩展）。
3. `easytier-web/src/restful/rpc.rs:49-142` —— 字符串分发 match 增加 `"api.manage.NetworkInstanceConfigService"` 分支（照抄 WebClientService 工厂模式）；`:159-183` `proxy_rpc_mutates_runtime_config`：save/set_enabled/remove 都是持久化配置变更，**评估**是否需要加入 runtime-config 失效清单（当前 WebClientService 的 run/retain/delete 在列）。
4. `easytier-web/src/client_manager/session.rs`、`mod.rs` —— 服务端 serve 注册点，新增服务注册。
5. `easytier-gui/src-tauri/src/lib.rs`：
   - `unsupported_method`（:1445-1449）：匹配的服务名从 "WebClientService" 扩展到新服务名；
   - 探测调用 `compatible_rpc_call(..., 9/10/11, ...)`（:1595、:1851、:2000）→ 新服务索引 1/2/3；
   - 测试 :2112、:2345-2412 中所有 `InvalidMethodIndex(9/10/11, "WebClientService")` 字面量同步改。
6. `easytier-core/src/wasi/web_client.rs`（WebClientService 引用）与 `easytier-core/src/instance/tests.rs`、`easytier-core/src/management/full/mod.rs` 的引用。
7. 前端 TS（easytier-web 前端、GUI）若以 `api.manage.WebClientService` 字符串直调这三个方法，改为新服务名（全仓 grep `"WebClientService"` 兜底确认）。

**已知残余风险（接受）**：`SaveNetworkInstanceConfigRequest` 等 message 名若上游未来也采用同名类型，包内会撞名。概率低；若要彻底规避可给 9 个 message 加 `Custom`/`X` 前缀，代价是 Rust/Go 代码全部改名。默认不加前缀。

### 产物再生成（重编号的强制配套，缺一不可）

1. **Rust prost**：build.rs 自动生成，`cargo build` 即可。
2. **pb.go**：`easytier-go/proto/`（api_manage.pb.go、common.pb.go 等）必须用仓库工具链重新生成——先找 easytier-go 下的生成脚本（buf 或 protoc 调用），没有则按 pb.go 头部注释的 protoc-gen-go 版本手动生成。当前 pb.go 已落后 HEAD 14 个提交，重生成会顺带带入 `supports_persisted_config_management`、`udp_http3_listeners` 等全部新字段。
3. **wasm**：`script/build-wasi-core.sh` 重新构建 `easytier-go/internal/artifact/easytier_core.wasm`（当前已落后 HEAD 11 个提交、6.7MB）。
4. **CI 漂移检测**（修复线 F9 会加白名单字段，注意合并）：Go workflow 增加 proto/** 触发路径 + "再生成后 `git diff --exit-code`"步骤。

### 任务 0 验收

- `cargo test --workspace` 通过（prost 自动重生成后全绿）；
- `go build ./... && go test ./...`（easytier-go）通过，pb.go 与 proto 一致；
- 本机起两个节点互连（含 wss/http3 伪装路径），确认 route/feature flag/打洞协商正常；
- `git diff 0a783c8e..HEAD -- easytier-proto/proto/` 中不再有任何 1-50000 区间的非上游 tag（新类型除外）。

---

## 阶段 0：死代码与琐碎坏味道（无行为变化）

| 项 | 内容（锚点已核实） |
|---|---|
| D1 死代码清理 | 删 `config/mod.rs:338-343` `wss_http3_p2p_allowed`/`wss_http3_p2p_preferred`（后者零调用）；删 `has_disguised_conn` 整链（`peer_manager.rs:1575`、`peer.rs:522`）；三个写死 AtomicBool `try_cone_before_sym`（udp/connector.rs:128/146）、`try_direct_connect`、`punch_predictably`（udp/client.rs:206-207）生产无写路径——删掉硬编码 true 及其分支（保留行为：按 true 化简）；`add_intreast_tid`→`add_interest_tid` 改名，4 个生产调用点（udp/server.rs:928、udp/client.rs:118/421/551）与定义（socket_array.rs:172-176） |
| D2 日志统一 | `easytier-web/src/main.rs` **仅 6 处 fork 新增的 `eprintln!`**（config-server 监听路径，用 `git diff 0a783c8e..HEAD` 确认，是 `+` 行）→ tracing；**上游自带的 6 处 eprintln 不动** |
| D3 默认值常量 | config 层加 `pub const DEFAULT_P2P_PROTOCOL: &str = "udp"`，替换 `config/toml.rs:32`、`direct/mod.rs:149` 两处裸字面量（注意 `easytier-web/main.rs:87` 是 `config_server_protocol` 的默认值，勿混淆）；升级文档注明"旧 TOML 未写 default_protocol 的配置升级后默认 tcp→udp" |
| D4 残留硬编码阶梯 | `direct/mod.rs:417` `["wss","http3"].find(...)` fallback 改为复用 `p2p_protocol_rank`/`preferred_disguised_scheme`（`config/mod.rs:364-390`，commit 719a490f 已中心化，此处是唯一漂移点） |
| D5 自签证书缓存 | `tunnel/insecure_tls.rs:81-90` `get_insecure_tls_cert()` 每次 rcgen 生成——用 `LazyLock` 缓存证书+rustls ServerCertVerified key；调用点 `websocket.rs:409`、`http3.rs:71` 不动 |

## 阶段 1：模块内去重（各项独立，可并行分支）

**D6 http3/quic 隧道去重（已按红线重新划定：单向去重，quic.rs 不动）**
事实：`quic.rs` 相对上游仅 +101/-19（魔改 hunk），主体是上游文本；`http3.rs` 是 fork 新增文件（其"重复"正是当初从 quic.rs 拷贝所致）。
做法：把可共享的会话机制抽到**新建** `tunnel/quic_common.rs`（fork-owned，doc 注释 `Custom (fork):`），**只让 http3.rs 改用它**；`quic.rs` 保持原样（其自身的 +101/-19 魔改 hunk 若顺手可迁入共享模块，其余一律不碰；确需 pub 化 quic.rs 某类型时按红线规则 1 最小化 + `// fork:` 标注）。
**明确接受**：quic.rs 暂时保留一份拷贝——上游将来改 quic.rs 时我们不产生冲突，这比双文件强一致更可维护。**验收：http3.rs 现有集成测试（echo/多连接/bbr）一字不改全过；quic.rs 的 diff 行数不增加。**

**D7 UDP 打洞侧去重（已按红线重新划定：只做 fork hunk 级去重）** — `hole_punch/udp/`
- ~~"三个 punch 循环抽单一参数化函数"~~ **取消**：`cone_to_cone`/`sym_to_cone`/`both_easy_sym` 的骨架是上游函数，统一化=重写上游代码。只有当某个循环整体处于魔改 diff 内才允许动（先 diff 确认）。
- 保留（均为魔改新增块）：server.rs 两处 HTTP3 协商降级守卫（:269-287、:884-897）抽 helper；client.rs 三处 native_http3 地址选择（:158,342,619）抽 `fn punch_remote_addr`。
- rpc.rs 5 处 `inbound_gate.allow_inbound_punch` 块（:512,536,557,581,605）：**先 `git diff 0a783c8e..HEAD` 确认是否魔改新增**；是则抽宏/函数，否则跳过。
- `try_cone_before_sym` 相关块：若 D1 已删该开关则自然消失；删除本身也需先确认整块在魔改 diff 内。

**D8 RunTransaction** — `management/full/process_rpc.rs`
`run_network_instance_locked`（:472-639，168 行；`restore_enabled_state` 7 处、`ensure_overwritable` 4 次）抽事务对象 backup→write→start→commit，回滚集中一处；给 `stop_instances_locked`（:282-325，:316 先写 disabled :318 再删实例、失败无回滚）补对称回滚。**注意该文件历史上常有 WIP 未提交改动，先确认工作区干净。**

**D9 legacy core 兼容层下沉**
GUI（`lib.rs:1444-1491` 助手 + :1573/:1851/:2000 调用）与 web（`client_manager/mod.rs:663`、:744）各实现一遍 "disable=save+delete / enable=run+overwrite / delete 后追 config 删除"。下沉到公共层（remote_client.rs 或新模块），两种探测机制（GUI 的 method-index 探测 vs web 的 `supports_persisted_config_management` 能力位）参数化为策略。**注意：任务 0 迁服务会改 GUI 的探测索引，D9 必须排在任务 0 之后。**

## 阶段 2：GUI 拆分

**D10 `easytier-gui/src-tauri/src/lib.rs` 2848 行拆分（已按红线划定搬运边界）**
- `mod manager`（:987-2580，1594 行含 506 行测试）是 **fork 新增**（上游 lib.rs 仅 1551 行）→ 整体迁出为 `manager.rs`（测试随迁），可自由整理；
- 上游衍生的 ~1250 行（上游命令、setup 流程等）**原位保留**；确需移动时整块字节不变搬运，不顺手重排/改名；
- fork 新增的命令（retire_conflicting_services、web client 系列、load_configs 等，用 `git diff 0a783c8e..HEAD` 确认清单）按域归入 `service.rs`/`rpc.rs`/`commands/`；上游命令留在 lib.rs 或同样字节不变搬运；
- **9 个**顶层 static（:56,62,65,68,71,74,90,96,100）——先 diff 确认哪些是 fork 新增：fork 新增的收拢进 `BackendState`（仍由现有 BACKEND_LIFECYCLE 锁保护，只搬迁不改并发语义）；上游自带的 static 定义原样保留；
- 29 个 command 名称/签名不变，前端零改动；
- `load_configs`（`mod manager` :1646-1762，117 行）是 fork 新增，随迁移顺手拆分。
验收：workspace 测试全绿 + GUI 手工冒烟（normal↔service 模式切换、配置增删改、服务启停）+ lib.rs 相对上游的 diff 中上游区域 hunk 不增加。

## 阶段 3：巨型函数（大半随前述阶段完成）

每项动手前先 `git diff 0a783c8e..HEAD` 判定：函数体是否**整体**处于魔改 diff 内（fork-dominated）。是 → 可自由拆分；只有部分 hunk → 只把魔改段抽成 helper，上游控制流骨架保持原样。

- `do_punch_as_initiator`（`hole_punch/tcp.rs`，~235 行，魔改重度——scheme/wss/端口预测均为 fork 加；但骨架含上游遗产，按 hunk 判定后拆）：资格判定 / scheme 选择 / punch 执行 / 结果处理。依赖的修复线 F13（并发限制）已实现。
- `reconcile_peer_connections`（`peer.rs`，7 参数；整个冗余清理逻辑为 fork 加，函数属 fork-dominated）引入 `ReconcileContext` 收拢参数，拆四步。依赖的修复线 F11/F12（查询副作用/验证门槛）已实现。
- `load_configs` 随 D10 完成。

## 待拍板（默认建议已给）

**wasm 入库策略**：`easytier-go/internal/artifact/easytier_core.wasm` 6.7MB、git 历史已 4 个版本（另 `testdata/wasi_socket_guest.wasm` 9.5MB）。默认建议迁 Git LFS 或改 CI 构建产物；至少约定"更新必须替换而非叠加"。

---

## 与修复线（F1-F14）的冲突协调

**状态更新：修复线 F1-F14 代码已全部完成并回归通过，但改动尚未提交**（20 个文件，工作区 `git status` 可见）。重构开工前必须先让修复线改动落盘（提交或由用户确认基线），否则会在 peer.rs / tcp.rs / process_rpc.rs 等重叠文件上互相覆盖。

| 重构项 | 冲突文件 | 顺序要求 |
|---|---|---|
| 任务 0 重编号 | proto + pb.go + wasm | F9 已加 CI 漂移检测与 proto 触发路径；任务 0 完成后统一再生成一次 pb.go + wasm 即可（会顺带带入 F9 的白名单字段） |
| D7 | hole_punch/udp/* | 修复线不动 udp/，无冲突 |
| D8 | process_rpc.rs | 修复线不动此文件；注意用户 WIP 提交（bc768048）已含 stop 校验改动 |
| D9 | lib.rs | 排任务 0 之后；F4/F10 已改 lib.rs 但位置不同，rebase 即可 |
| D11-do_punch | hole_punch/tcp.rs | F13 已实现（busy 门）；tcp.rs 还有用户提交 bc768048 的 WSS 黑名单改动，以当前工作区为准 |
| D11-reconcile | peer.rs | F11/F12 已实现；拆分时保留其语义与测试 |

## 总验收

- 每阶段：`cargo test --workspace` + `cargo clippy` + `go build/test`（涉及时）+ `vue-tsc`（涉及时）；
- **红线验收（每步）**：对涉及上游共享的文件，`git diff 0a783c8e..HEAD -- <file>` 中上游区域的 hunk 不得增加；将来 rebase 上游 main 的冲突面单调不增；
- 任务 0 后：双节点互连冒烟（含伪装协议）；
- D10 后：GUI 双模式手工冒烟；
- 全程不改变任何对外行为（除任务 0 的 wire tag，其本身对未分发用户透明）；发现 bug 只记录移交，不混入重构提交。
