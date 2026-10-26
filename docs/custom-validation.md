# EasyTier Custom：验证记录

各轮合入与专项改动的验证范围、结果和未通过项，按时间倒序排列。改动说明见 [CUSTOM_CHANGES.md](CUSTOM_CHANGES.md)。

## 兼容层清理与并行传输改进验证（2026-10-27）

在 Windows 本机运行，覆盖 P2P 伪装策略合并、并行发送计划、调度与重排、送达速率带宽估计和地球服务端汇总。

- Rust：`easytier-core` 完整库测试中 peer 与 tunnel 相关 332 项、`easytier-proto` 44 项、`easytier-web` 361 项通过；`easytier` 配置、Core 与并行组合测试 99 项及 composition/parallel 40 项通过（2 项默认忽略）。`easytier-core` 的 WASI 特性组合检查通过。
- 并行传输稳定性：并行相关测试连续 15 轮与另 20 轮全部通过。旧的按字节轮流调度在 25 轮中曾有 1 次吞吐倍数断言失败。端到端 composition 重复 3 轮中有 1 次失败于 IPv6/WSS 场景的 HTTP3 伴随连接建立超时，该用例与条带调度无关，单独复跑 4 轮均通过。
- 前端：前端库 168 项、网页前端 199 项（含批量采集新增 2 项）、GUI 56 项通过，`test-network-config` 通过。共享的新建网络夹具 `network-create-patches.json` 由 `buildLocalConfigCreatePatch` 重新生成，Rust 侧 `persisted_config_tests` 用同一夹具验证通过。
- 本机环境限制：沙箱禁止 `target/` 下的测试程序和 esbuild 写入系统临时目录，相关用例在 `TMP` 指向 `target/tmp` 后通过；前端库生产构建因 esbuild 无法删除临时文件，改用 `vite build --minify false --target esnext` 生成测试用产物，未验证压缩后的发布构建。WASI 检查需要短目标路径（`CARGO_TARGET_DIR`）避开 Windows 命令行长度限制。
- 未运行：Linux 专属网络测试、浏览器端到端测试和真实公网多连接测速。

## HTTP3 与多连接专项验证（2026-10-18）

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

## 官方主线合入验证（2026-10-19）

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

## 官方主线合入验证（2026-10-16）

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

## 官方主线合入验证（2026-10-11）

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

## 四项回归修复验证（2026-10-08）

- 实时查询官方远端 `refs/heads/main` 得到 `4837468d43b6fe48865b2ce926312b66afd10bef`，与本次独立构建的互通基线一致；未额外合入上游提交。独立源码目录的握手、WebSocket、进程管理及锁文件已与该提交核对。
- Core 默认库测试 967 项通过；新增测试包括普通/Noise 握手扩展、双端四种拨号方向、协议偏好冲突、单端清理开关、双向收发、策略热修改、排队后复验和重连恢复。
- Web 后端 325 项通过，1 项既有大容量连接测试默认忽略；覆盖旧节点能力过滤以及 TCP/UDP/WS/WSS 实际监听端口、配置顺序和首项绑定失败回退。
- 原生持久化配置事务 24 项通过，覆盖旧请求调用前及加载/hook 期间出现的重复 UUID、混合批次部分成功、权限/CAS、完整原文应用、来源切换、停止实例不启动和失败回滚；WS/WSS 协商 20 项通过。
- 独立官方 `4837468d43b6fe48865b2ce926312b66afd10bef` 进程互通 6 项通过：WS/WSS 双向各两种 padding、WS/WSS 双向 Noise、管理连接及反向 RPC、拒绝未协商文本 padding 的探针。官方未声明清理扩展时连接继续存活并双向交换数据。
- 共享前端 31 个文件、297 项测试，配置导出 10 项检查，GUI 前端 4 个文件、25 项测试通过；共享库、Web、GUI 的类型检查和生产构建均通过。

## 此前验证

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
