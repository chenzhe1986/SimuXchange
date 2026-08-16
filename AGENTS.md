# AGENTS.md — SimuXchange 项目协作指南

> 本文件面向参与本项目的 AI 编码助手与开发者，说明项目背景、架构约定、
> 代码规范和常用命令。**任何修改都应遵守本文约定。**

---

## 1. 项目背景

SimuXchange 是一款**模拟交易所交易网关（TGW）**的跨平台测试工具：
假扮真实交易所网关，接受柜台系统（OMS）按交易所 Binary 协议发来的 TCP
连接与委托，按用户配置的策略回送确认回报 / 成交回报 / 拒单。
**没有真实订单簿与撮合**，所有回报均为策略模拟生成，用于柜台系统联调测试。

支持三类网关（由 `GatewayConfig.category` 区分，新建网关时选择）：
**深圳统一网关**（深交所 Binary，sz）、**上海竞价网关**（上交所竞价平台 Binary 0.54，shjj）
与**上海新债券网关**（上交所债券平台 Binary 1.90，shbond）。
三套协议代码按目录分放（`sz/` / `shjj/` / `shbond/`，目录内为 protocol.rs / session.rs / strategy.rs）。
新增网关类型 = 新增一个同构目录（protocol/session/strategy 三件套），并在 engine.rs 分类分派处登记；
上交所系可复用 `sz::session::SessionCtx`（见 3 节）。

维护者精通 C++，但**对 Rust 与 Vue.js 不够熟悉**：注释请用中文，重点解释 Rust/Vue 特有的
概念（所有权与借用、trait、async/await、响应式等，可类比 C++ 的 shared_ptr/接口/协程），
通用编程逻辑不必赘述，措辞保持专业，不使用“给非专业开发者”类科普标签。

## 2. 架构与目录

```
SimuXchange/
├─ crates/
│  ├─ simx-core/        # 核心库：协议编解码、会话、回报策略、统计
│  │   ├─ src/sz/       # 深圳统一网关：protocol.rs / session.rs / strategy.rs
│  │   ├─ src/shjj/     # 上海竞价网关：protocol.rs / session.rs / strategy.rs
│  │   ├─ src/shbond/   # 上海新债券网关：protocol.rs / session.rs / strategy.rs
│  │   │               # （上交所两套会话共用 sz/session.rs 的 SessionCtx）
│  │   ├─ src/capture.rs  # 连接收发报文捕获（16 进制 + 按字段名解析 + 可选持久化到文件）
│  │   ├─ src/orderbook.rs  # 订单缓存（界面订单列表、撤单按状态判断）
│  │   ├─ src/oplog.rs    # 前端操作日志（带前端 IP，按日期存 log/ 目录）
│  │   ├─ src/engine.rs   # 引擎：按网关分类分派会话、网关/平台生命周期管理
│  │   ├─ src/api.rs      # 统一控制接口（供 Tauri 与 server 复用）
│  │   └─ tests/e2e_sz.rs / e2e_shjj.rs / e2e_shbond.rs  # 端到端测试（真 TCP，深交所 18101~ / 上交所竞价 18201~ / 上交所新债券 18301~）
│  ├─ simx-server/      # 独立后端：axum WebSocket 控制接口（远程部署）
│  └─ simx-update-server/  # 升级服务器：axum HTTP，网页上传安装包+自动生成 version.json（crates/ 下）
├─ src-tauri/           # Tauri 2 桌面壳（包名 simuxchange，内嵌 simx-core）
├─ src/                 # 前端 Vue 3 + TypeScript
│  ├─ App.vue           # 主页面（全部界面状态与交互）
│  ├─ components/       # PlatformCard / PlatformEditor / LogPanel / PacketViewer（收发报文弹窗）
│  ├─ types.ts          # 前后端共享数据结构（与 Rust 结构体手工对应）
│  ├─ backend.ts        # 后端抽象层：LocalBackend(Tauri invoke) / RemoteBackend(WebSocket)
│  ├─ changelog.ts      # APP_VERSION / CHANGELOG（“ⓘ 关于”弹窗的版本号与更新说明）
│  └─ updater.ts        # 更新检查：version.json 清单协议、版本比较（checkForUpdate）
├─ docs/                # 构建调试手册、产品使用手册、协议 PDF
├─ scripts/             # build-all.ps1（tauri build 前准备 Linux 产物+前端）、smoke 测试等
├─ .cargo/config.toml   # 项目级镜像配置（aliyun sparse，覆盖全局 tuna）
├─ simx.config.json     # 运行时配置：backend = local / remote + 可选 updateUrl
└─ Cargo.toml           # Cargo workspace 根
```

数据流：前端每秒轮询 `get_snapshot` 整体替换快照（轮询快照模式，最多 1 秒延迟）；
网关配置持久化在数据目录 `gateways.json`。

**报文捕获**：平台配置 `showPackets`（展示收发报文）为**单一开关（默认 true）**，
`persistPackets`（持久化到文件）**已与展示合并**：保存网关时 `persistPackets = showPackets`
自动归一化（前端默认勾选、字段保留仅为向后兼容旧配置）；勾选后每条报文同时做三件事——
界面弹窗展示、按交易所字段名解析（`ParsedField`，由各协议 protocol.rs 的
`describe_fields(mt, body)` 生成，展示时与原始报文分行、字体区分）、持久化到文件
（`<data_dir>/packets/<YYYYMMDD>/<网关名>_<平台名>/pkg_<HHMMSS>_ip-<IP点转下划线>_port-<端口>.log`，
逐行 flush；原始报文行下方跟解析字段缩进续行，内容与界面一致）。
连接断开时记录器**不删除**，仅 `mark_dead()` 标记离线——平台级报文弹窗要能按连接分组回看历史报文。
前端报文弹窗每 0.7s 以 `after_seq` 游标增量拉取（连接级 `get_conn_packets` / 平台级 `get_platform_packets`，
后者附 `conns` 连接摘要供分组标题展示）；持久化开启时回看从连接建立起全部报文。

## 3. 关键业务逻辑

- **网关分类**：`GatewayConfig.category` = `sz`（深圳统一网关）/ `shjj`（上海竞价网关）/ `shbond`（上海新债券网关），
  决定网关下所有平台说哪套协议；`engine.rs` 按 category 分派到 `sz::session` / `shjj::session` / `shbond::session`。
  新建时选定，创建后不可修改（前端重命名弹窗中分类下拉框置灰）。
- **网关 → 平台**两级结构：网关是平台集合，整体启动/停止；
  一个平台 = 一个 TCP 监听端口。**网关运行中禁止增删平台/删除网关**；
  **编辑平台允许**（仅"模拟回报策略与回报延迟"热更新实时生效，其余字段运行中保存无效、需停止后修改）。
- **单连接限制**：一个平台同一时刻只服务一个柜台连接（accept 循环检查存活连接表，已有连接时直接断开新连接拒绝接入）；
  手动回复（send_report）的回报统一从**当前活动连接**发出——历史连接发来的订单同样走当前连接，
  因此 send_report 不记订单所属连接，而是取 `conn_tx` 中连接号最大者（最近接入）。
- **深交所协议编码**：MsgType(4)+BodyLength(4)+Body+Checksum(4)，大端序，
  定长字符串右补空格；Price N13(4) = 价格×10000，Qty N15(2) = 股数×100。
  会话：Logon(1) → 回 Logon + 平台信息(9) + 平台状态(6) → 心跳(3)；
  新订单 100101 → 按策略回 200102 确认 / 200115 成交 / 拒单；撤单 190007 固定回撤单失败 290008；
  回报同步(5)→按 begin 重发历史回报（有捕获缓存时；按实测真实交易所行为，
  发完历史回报后不回“回报结束”等结束标记，柜台按记录号自行对账）。
- **上交所竞价协议编码**：报文头 MsgType(4)+MsgSeqNum(8)+MsgBodyLen(4)（共 16 字节）+Body+Checksum(4)，
  大端序，校验和 = 全字节 uint8 自然溢出累加；Price N13(5) = 价格×100000，Qty N15(3) = 股数×1000。
  消息序号由 writer 任务发送时递增赋值（finalize_seq 补写）。会话：Logon(40，无密码字段) → 回 Logon + 平台状态(209) + 执行报告信息(208)；
  **登录后需先完成分区序号同步(206→207)才推送执行报告**（同步前回报缓存，同步后补发；同步时按各分区 begin 重发历史回报，有捕获缓存时）；
  新订单 58 → 申报响应 32 / 成交 103 / 拒单 204；撤单 61 固定回撤单失败 59；心跳 33，区间[5,60]。
- **上交所新债券协议编码**：报文结构与竞价平台完全一致（报文头、校验和、字段放大、会话流程、消息类型均同竞价），
  仅业务参数不同：PlatformID=2（竞价=0）、分区号默认 101,102,103,104（新建平台界面；协议常量 SetID 竞价=1、其余业务 991/992）、BizID=1 债券现券竞价 / 2 债券质押式回购（竞价=100010）、
  协议版本最低 1.90（竞价用 "0.50"）、OrdType 仅支持 2=限价。
- **七种回报策略**（三套协议一致）：全成单笔 / 全成拆单 / 部成单笔 / 部成拆单 /
  不自动回复（挂单手动回复，订单进缓存等界面手动回确认/成交/拒单/撤单）/ 只回确认 / 拒单
  （执行报告拒绝或业务拒绝二选一；旧配置的 custom 自定义成交已废弃，加载时降级为只回确认）。
- **多分区号**：`PlatformConfig.partition_nos`（逗号分隔字符串）支持一个平台配置多个分区号，
  回报按**证券代码哈希**（`partition_for(&security_id)`）分配到其中一个分区——同一证券恒落
  同一分区，保证该证券的确认/成交/撤单回报都在同一分区；为空时回退单分区 `partition_no`。
  登录时下发的平台信息（深 9 / 沪 208）携带全部分区列表。
- **策略热更新**（网关不停机实时生效）：`SessionCtx.strategy` 为 `Arc<std::sync::RwLock<StrategyConfig>>`
  运行时共享区，与持久化配置（gateways.json）分离；每次生成回报计划时在块作用域内加读锁
  读取最新值（`RwLockReadGuard` 不是 Send，**不能跨 await 存活**）；`engine.update_strategy`
  同时写运行时共享区与持久化配置（api 命令 `update_strategy`），前端在网关运行中保存平台时走此命令。
  `plan_reports` 签名为 `(st: &StrategyConfig, partition_no: i32, order, stats, [pbu])`——策略与分区号分离。
- **自动启动**：simx-server / 桌面端均支持 `--auto-start` 参数，引擎创建后调用
  `engine.auto_start_previous()` 只恢复**上次运行中**的网关（启停状态并入
  `data_dir/gateways.json` 的 `was_running` 字段：start_gateway 置 true、
  stop_gateway 置 false、删除随配置消失；旧版无该字段视为 false）。
  不带参数则不自动启动任何网关。
- **自动检查升级**：桌面端在「后端连接设置」里配置 `updateUrl`（AppConfig.update_url，
  simx.config.json 的 camelCase 字段），启动 3 秒后 `checkForUpdate`（src/updater.ts）
  拉取 `<updateUrl>/version.json`（`{version, notes, winInstaller, linuxServer}`，
  数字点分比较 `compareVersions`），有新版弹窗，点「下载并安装」调 Tauri command
  `download_update`（src-tauri/main.rs，ureq 下载到 Downloads + 启动安装程序）；
  检查失败静默忽略。simx-server 侧 `--update-url` 参数启动时 `check_update`
  （crates/simx-server/src/main.rs，ureq 无 TLS、仅 http，`version_cmp` 同规则）
  只打印提示不自动替换。升级服务器 `simx-update-server`（crates/simx-update-server/src/main.rs）
  提供 `/version.json`（实时扫描目录生成，无安装包时 404）、`/files/:name` 下载、
  `POST /api/upload`（multipart：file 安装包 + linux 可选 + notes 说明）、`DELETE /api/file/:name`；
  版本号从安装包文件名解析（`SimuXchange_x.y.z_x64-setup.exe`，至少三段数字点分，
  取版本号最大者进清单），更新说明按版本存 `notes.json`；路由参数用 axum 0.7 的
  **`:name` 语法**（`{name}` 是 axum 0.8 的写法，勿混用）。安装包内嵌 Linux 后端：
  tauri.conf.json `bundle.resources`
  用 **Map 形式** `{"../target/x86_64-unknown-linux-musl/release/simx-server": "Linux/simx-server"}`
  （Tauri 2 不支持 v1 的 source/target 对象数组），安装后位于安装目录 `Linux/`；
  beforeBuildCommand 执行 scripts/build-all.ps1（确保产物存在 + vite build）。
- **延迟规则**：min=max=0 同步回报；否则异步，区间内随机抽样。
- **报文捕获**：见第 2 节“报文捕获”段（三套会话共用 `SessionCtx::start_capture(conn_id, peer)`；
  连接断开时记录器保留仅 `mark_dead()` 标记离线，**不删除**（平台级报文弹窗按连接分组回看）；
  读侧在 read_frame 后记录，写侧在 writer 任务写出前记录——上交所需在 `finalize_seq` 补完
  MsgSeqNum 之后捕获，保证文件里是真实上线字节）。

## 4. 代码规范

### 4.1 注释规范（本项目最重要的约定）

- 全部注释使用**中文**，面向“精通 C++、不熟 Rust/Vue”的读者；
- 注释重点解释 **Rust/Vue 特有概念**：所有权与借用、trait、async/await、通道、
  响应式 ref/reactive 等，可用 C++ 类比（Arc ≈ shared_ptr、trait ≈ 接口、
  async/await ≈ 协程）；通用编程逻辑不必赘述；
- 文件头注释交代：模块职责、设计动机（“为什么这么做”）、关键机制；
- 函数级用 `///`（Rust）/ JSDoc（TS）；行内注释解释原理而非复述代码；
- **新增/修改代码必须补齐同风格注释**，不得留无注释的新逻辑；
- 不使用“给非专业开发者”等科普标签，注释措辞保持专业。

### 4.2 前后端类型同步

Rust 结构体（serde 序列化为 **camelCase**）与 `src/types.ts` 的 interface
是**手工对应**的。修改任何一边的字段，必须同步修改另一边，
并检查 `App.vue` / 组件中的使用处。新增报文解析字段时注意同步
`capture.rs` 的 `ParsedField` 与 `types.ts` 的 `ParsedField`。

### 4.3 其他约定

- 前端遵循既有模式：状态集中在 `App.vue`，子组件只通过 props 接收、emit 上报；
  所有后端调用走 `backend.ts` 抽象层，**不得**在组件里直接 `invoke`；
- 修改协议/策略逻辑后必须跑 `cargo test -p simx-core`（含 21 个 e2e 测试：深交所 11 + 上交所竞价 5 + 上交所新债券 5）；
- 右键菜单、弹窗等 UI 交互遵循现有实现风格（贴边修正、运行态置灰、Esc 关闭）；
- 端口约定：Vite 开发端口 **14200**（`vite.config.ts` 与 `tauri.conf.json` 的
  devUrl 两处必须一致）；e2e 测试占用 18101~18110（深交所）、18201~18205（上交所竞价）与 18301~18305（上交所新债券）；平台默认端口从 10001 起。

## 5. 常用命令（PowerShell）

> 本机 shell 为 PowerShell：**不支持 `&&`，用 `;` 分隔命令**。
> 找不到 cargo 时先执行 `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`。

| 目的 | 命令 |
| --- | --- |
| 安装前端依赖（首次） | `npm install` |
| 开发调试（桌面端热更新） | `npm run tauri dev` |
| 只跑前端（浏览器） | `npm run dev` |
| Rust 快速检查 | `cargo check -p simx-core -p simx-server` |
| Tauri 主程序检查 | `cargo check --manifest-path src-tauri/Cargo.toml` |
| 全部测试 | `cargo test -p simx-core` |
| 前端构建验证 | `npx vite build`（类型检查：`node .\node_modules\typescript\bin\tsc --noEmit`） |
| 打包桌面发布版 | `npm run tauri build` |
| 构建独立后端 | `cargo build --release -p simx-server` |

## 6. 环境注意事项（踩过的坑）

1. **不要用 1381–1480 段端口**（Windows Hyper-V/WSL 保留段，报 EACCES）；
2. `src-tauri/Cargo.toml` 必须保留 `default = ["custom-protocol"]`，
   否则发布版 exe 加载 devUrl 白屏（"localhost 拒绝连接"）；
3. cargo 显示 ExitCode 非 0 不代表失败——**以输出末尾 `Finished` /
   `test result: ok` 判断成功**（PowerShell 把进度输出当 stderr）；
4. cargo 报「拒绝访问 (os error 5)」多为杀毒拦截 build-script，重跑即可；
5. 避免 `cargo check --workspace`（易触发上一条），按包分开检查；
6. cargo 拉依赖 404 时用项目级 `.cargo/config.toml` 切 aliyun sparse 镜像（见构建手册坑⑨）；
7. 新建含中文的 .ps1 必须存 **UTF-8 with BOM**，否则 PowerShell 5.1 乱码报解析错误（坑⑩）。

详细说明见 [docs/构建调试手册.md](docs/构建调试手册.md) 第 8 节。

## 7. 文档同步义务（每次改动必查）

**每次修改功能后，必须检查并更新以下文档**（如有涉及）：

| 文档 | 何时更新 |
| --- | --- |
| `docs/构建调试手册.md` | 构建/调试流程、命令、依赖、端口、部署方式变化，或踩到新坑 |
| `docs/产品使用手册.md` | 界面、操作流程、配置项、回报策略、协议范围、FAQ 相关的功能变化 |
| `README.md` | 项目结构、编译命令、功能概述层面的变化 |

判断标准：**用户按旧文档操作会不会遇到与实际不符的地方**——会，就必须更新。
纯内部重构（不影响命令、界面、操作）可不更新，但需在回复中说明已确认无需更新。

## 8. 验证清单（提交改动前）

- [ ] `cargo check -p simx-core -p simx-server` 通过（Finished）
- [ ] 涉及 Tauri 时：`cargo check --manifest-path src-tauri/Cargo.toml` 通过
- [ ] 涉及协议/策略/会话时：`cargo test -p simx-core` 全绿（94 单元 + 4 自动启动 + 21 e2e）
- [ ] 涉及前端时：`npx vite build` 通过 + `tsc --noEmit` 无错误
- [ ] 前后端类型已同步（types.ts ↔ Rust 结构体）
- [ ] 新增逻辑已按 4.1 规范补注释
- [ ] 已按第 7 节检查两份手册与 README 是否需要更新
