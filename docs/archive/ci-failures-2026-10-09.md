# GitHub Actions 失败排查（2026-10-09）

检查对象为 `225284228a-droid/EasyTier-Custom` 的 `main`，远端提交
`0ad9b428de1c11cbe23451558a8015683cc010c3`。日期使用 Pacific/Auckland；
本次运行开始于 2026-10-09 00:41:48 NZDT，即日志中的 2026-10-08 11:41:48 UTC。

## 当前提交的确定失败

| 检查 | 日志证据 | 原因与修复 |
| --- | --- | --- |
| [Go proto freshness](https://github.com/225284228a-droid/EasyTier-Custom/actions/runs/37771755404/job/113292785201) | `caution: filename not matched: include/google`，退出码 11 | `unzip` 精确匹配不到该归档成员，尚未运行 proto 比较。改为带引号的 `include/google/**`，匹配实际 protobuf 文件。 |
| [Frontend/GUI unit tests](https://github.com/225284228a-droid/EasyTier-Custom/actions/runs/37771755337/job/113292841668) | `Failed to resolve entry for package "tauri-plugin-vpnservice-api"`，GUI 8 项测试失败 | CI 默认只构建 web 包，VPN 插件的 `dist-js` 不存在；Vitest 在应用 mock 前就无法解析入口。测试配置显式解析到插件真实 `guest-js/index.ts`，保留原 mock、断言和测试范围。 |
| [Clippy](https://github.com/225284228a-droid/EasyTier-Custom/actions/runs/37771755337/job/113292841515) | `process_rpc.rs:651`，`this function has too many arguments (8/7)`，退出码 101 | `-D warnings` 将参数数量警告升级为错误。将持久化内容、apply-only 标记及预期修订打包为内部 `InstanceRunPersistence`，两个调用位置保持原值及调用顺序。 |
| [test 汇总](https://github.com/225284228a-droid/EasyTier-Custom/actions/runs/37771755337/job/113299801223) | `Mark result as failed` | 汇总前面失败的作业，不是独立的新错误。前端作业失败还使后端测试、网络测试归档与网络矩阵未运行。 |

同次运行中格式检查、功能组合检查、WASI 检查和锁文件检查通过；Go 两个模块测试、
JavaScript Hosts 检查通过。上一轮的格式和另外几项 Clippy 警告在当前提交已修正，
不能将旧日志作为当前问题重复处理。

修复首个 Clippy 错误后，本地 Linux `full` 复检还发现
`easytier/src/core.rs:2287` 的 `clippy::cloned_ref_to_slice_refs`：测试用例以
`&[explicit_invalid.clone()]` 构造单元素切片。现已改用
`std::slice::from_ref(&explicit_invalid)`。它被先前核心库编译失败挡住，原 GitHub
作业还没有检查到这一处；本地证据为
`.test-env/ci-actions-2026-10-09/core-native-full-clippy-first.log`。

## 上一轮 UPnP 网络测试

[run 37713144611 / job 113105415371](https://github.com/225284228a-droid/EasyTier-Custom/actions/runs/37713144611/job/113105415371)
对应 `54f5069b`。`Test (easytier)` 共运行 1424 项，1423 项通过，只有
`instances_build_direct_connection_via_upnp_udp_hole_punch` 失败；其余两个三节点矩阵通过。

该用例在 `wait_instance_direct_peer_via_upnp_and_route_cost_1` 阶段超时。
网关发现、创建映射、查询映射已完成，日志中的 `UPnPError 713` 是代码已处理的枚举结束。
原断言还要求两端连接地址分别匹配对方第一条 listener 映射，而实际允许独立客户端
临时端口。旧日志没有连接和路由快照，单凭它无法判断是建链失败还是地址断言失配。

本地 Ubuntu 24.04 / miniupnpd 2.3.4 / Rust 1.95 的第二次运行复现了误判：
`direct_a=true, direct_c=true, route_a=true, route_c=true, mapped_a=true, mapped_c=false`
持续到超时。双方已经直连且路由 cost 都是 1；C 的客户端连接使用 A 的第二条长期映射
`11.22.33.1:60514`，服务端连接则看到 A 的临时映射 `11.22.33.1:57192`，
均不同于 A 的首个 listener 映射地址 `11.22.33.1:42203`。
A 到 C 的已验证 listener 映射 `11.22.33.2:56901` 则匹配。证据保存在
`.test-env/ci-actions-2026-10-09/upnp-run-2.log`。

因此，一条正常双向 UDP 隧道会被原先要求两端同时匹配 listener 地址的断言误报为失败。
修复保留双向直连、双方 cost=1、UPnP 映射创建与查询、以及最终实际 ping 检查，
重新查询两台 IGD 当前有效的映射集合，要求至少一个方向的真实 UDP 远端属于对方
已验证的映射集合；另一端可以使用自己的客户端临时端口，listener 也不必是首次创建的那个。
修复后连续三次运行均通过（10.84 秒、10.77 秒、10.79 秒），每次都完成原有实际
ping 阶段，测试进程退出码均为 0。日志为同目录的 `upnp-fixed-run-1.log`、
`upnp-fixed-run-2.log`、`upnp-fixed-run-3.log`。保持原 20 秒直连判定超时，未跳过该用例。

## 历史工作流

| 工作流 | 最近历史失败证据 | 当前情况 |
| --- | --- | --- |
| [EasyTier GUI](https://github.com/225284228a-droid/EasyTier-Custom/actions/runs/36211610858/job/108319186939) | macOS x86_64 的 `Validate macOS signing secrets` 缺少 `APPLE_CERTIFICATE`、`APPLE_CERTIFICATE_PASSWORD`、`APPLE_SIGNING_IDENTITY`、`APPLE_ID`、`APPLE_PASSWORD`、`APPLE_TEAM_ID`；其余六项构建被取消 | 仓库状态为 `disabled_manually`；没有修改凭据或启用工作流。 |
| [ohos](https://github.com/225284228a-droid/EasyTier-Custom/actions/runs/36211610911/job/108319189775) | `Build HAR` 的 `cargo test --locked` 发现旧独立 `easytier-contrib/easytier-ohrs/Cargo.lock` 需要更新 | `eafd46b9` 已将其并入根 workspace 并删除旧锁。工作流状态为 `disabled_manually`，历史错误不能代表当前构建结果。 |

GUI/OHOS 的 YAML 仍监听 `main`；近期没有运行是因为仓库工作流被手动停用。
EasyTier Core 和 Mobile 也处于 `disabled_manually`。这些启停状态通过 GitHub API
只读查询确认，本次未修改。

## 已完成验证

- 移走 VPN 插件 `dist-js` 后，准确复现原来的 8 项失败；修复后在同样条件下运行
  `pnpm test:unit`：frontend-lib 107、frontend 190、GUI 25、CI 脚本 3，共 325 项通过。
- 用临时 ZIP 复现原 `unzip` 匹配错误；修复后的表达式成功解压多层 protobuf 文件。
- 使用 protoc 35.1 与 protoc-gen-go v1.36.11 实际运行 `generate-proto.sh`，
  `git diff --exit-code -- easytier-go/proto` 通过，确认没有生成代码漂移。
- `actionlint .github/workflows/go.yml`、`cargo fmt --all -- --check`、`git diff --check` 通过。
- 独立复核确认参数默认值仍为 `None / false / None`，持久化校验、写入和回滚顺序不变。
- WSL Linux 下持久化管理回归 15 项、配置合并回归 7 项通过，共 22 项，退出码均为 0。
  日志保存在 `.test-env/ci-actions-2026-10-09/core-persisted-management.log` 和
  `core-persisted-merge.log`。另一个 `persisted_config` 过滤器匹配 0 项，不计入通过数量。
- Linux 原生 `persisted_config_tests` 24 项通过，覆盖保存并应用、仅应用、修订冲突与失败回滚；
  `config_dir_startup_isolates_register_run_errors_and_keeps_explicit_fatal` 1 项通过。
  两者退出码均为 0，日志分别为同目录的 `native-persisted-config.log` 与
  `native-config-dir-startup.log`。至此相关 Rust 回归共 47 项通过。
- Linux 核心与原生组件的最终 Clippy 检查通过，退出码 0：
  `cargo clippy --locked -p easytier -p easytier-core --all-targets --features full -- -D warnings`。
  日志为同目录的 `core-native-full-clippy-final.log`；本地没有将此结果扩称为整个 workspace 的 CI 通过。

本次本地 Windows 完整 Clippy 曾被 Perl 缺失、构建资源下载以及 protoc 中文输出路径
阻断；这些是本地复现限制，不是 GitHub Linux 日志中的失败原因。上述最终结果来自 WSL Linux 验证。
本地修复尚未推送或在 GitHub 上重跑，不能据此宣称远端所有检查已通过。
