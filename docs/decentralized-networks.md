# 去中心化网络管理（识别 + 批量修改）

## 背景

原“网络”页是中心化模型：控制台数据库保存网络意图（名称、密码、组网方式、成员、ACL），
`central_network` 服务把编译后的整份配置发布到成员设备。拓扑图则相反：它不读控制台的
网络记录，而是从每台设备运行中的实例里读出 `network_name`，以 `name:<network_name>`
作为网络身份，把同名实例的节点和连接画到一起。

两边识别方式不同，导致同一个网络在拓扑图上存在，在网络页却看不到（例如设备本地或 GUI
创建的网络），反之亦然。

## 现在的模型

网络页改为与拓扑图同一套识别方式，控制台不再持有网络状态：

- **识别**：`networkDiscovery.ts` 对每台在线设备调用 `list_network_instance_ids` 与
  `get_network_metas`，取到运行中和已停止实例的 `network_name`，按名称分组。分组键用
  `networkTopology.ts` 导出的 `networkNameIdentity()`，与拓扑节点的 `networkIdentity`
  完全一致，所以网络页的一个网络就是地球仪上的同一组节点。官方节点（没有 revision 存储）
  同样能被识别。
- **共享扫描**：`MainPage` 创建唯一的 `NetworkDiscovery` 并 `provide`，侧边栏、网络列表、
  网络详情和仪表盘共用这一个 10 秒轮询，不重复请求设备。
- **批量修改器**：网络详情页（`networks/view/:networkName`）列出所有成员实例，默认全选可
  编辑的实例。“批量修改”读取每个成员各自的配置，以第一个成员为模板显示完整配置表单；
  应用时只取模板中**被改动的字段**（`changedConfigFields`），逐台合并到成员自己的配置
  （`mergeConfigFields`），再经该设备的 `ConfigStore` 保存——与设备管理页同一条写入路径：
  - 支持 revision 的节点走 `local-configs/patch`，带 `expected_revision`，并发修改会以冲突
    失败而不是覆盖；
  - 官方节点走 `save_config`/`run_network`；
  - 运行中的网络保存即重启生效，已停止的保持停止。
  每个成员的结果（已保存 / 冲突 / 启动失败 / 失败）单独显示。成员间取值不同的字段会列出，
  未改动时各自保留。主机名、虚拟 IP、DHCP、子网代理等设备专属字段被改动时会先确认。
  安全模式只同步开关，各节点保留自己的密钥。改网络名即批量改名，成功后跳到新名称。
- **批量启停**：选中成员一键启动或停止。
- **批量创建**：在选中的设备上各创建一份同名网络配置（名称、密码、初始节点、监听地址），
  可选立即启动。与已有网络同名即加入该网络。
- **地球仪**：详情页嵌入 `NetworkTopologyGlobe`，复用 `useMeshTopology` 的完整拓扑，只保留
  `networkIdentity` 等于当前网络的节点及两端都在其中的链路，其他网络全部隐藏。视角按网络
  分别保存。

## 与中心托管网络的关系

中心网络仍然保留：在中心模式下，网络页多一个“创建中心托管网络”按钮，原中心详情页仍在
`networks/:networkId`。中心网络发布到设备上的实例同样按 `network_name` 被识别，列表里
带“中心托管”标记，详情页提供“中心管理”入口。

中心发布本身就以 revision 保护本地文件（见 `central_network/local_delivery.rs`）：在这里对
中心托管网络做批量修改会直接写入设备文件，下一次中心发布会因 revision 冲突而保留本地修改
并报告冲突，不会静默覆盖。详情页对此有提示。

外部 Console（webhook）模式下中心路由不可用，但识别、批量修改、批量创建都只依赖设备 API，
因此网络页在两种模式下都可用；只有中心详情页会被重定向。

## 验证

- `pnpm --dir easytier-web/frontend test:unit`：`tests/network-discovery.spec.ts` 覆盖分组、
  与拓扑身份一致、中心网络保留、按字段合并（含清空即重置、`peers`/`peer_urls` 成对复制、
  安全模式密钥不复制）以及差异字段。
- `vue-tsc -b` 类型检查。
