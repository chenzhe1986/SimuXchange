# AGENTS.md — SimuXchange 项目协作指南

> 本文件面向参与本项目的 AI 编码助手与开发者，说明项目背景、架构约定、
> 代码规范和常用命令。**任何修改都应遵守本文约定。**
>
> **维护原则**：本文件只保留**每次协作都需要的通用约定**（架构、业务规则、
> 代码规范、命令），宁精勿多——环境踩坑、一次性排查过程等低频细节一律记入
> `docs/构建调试手册.md`，不写入本文件（占用上下文、降低专注度）。

---

## 1. 项目背景

SimuXchange 是一款**模拟交易所交易网关（TGW）**的跨平台测试工具：
假扮真实交易所网关：TCP 网关接受柜台系统（OMS）按交易所 Binary 协议发来的
连接与委托，上海综合网关则通过 SQL Server 接口表与柜台交换 STEP 消息；
按用户配置的策略回送确认回报 / 成交回报 / 拒单。
**没有真实订单簿与撮合**，所有回报均为策略模拟生成，用于柜台系统联调测试。

支持四类网关（由 `GatewayConfig.category` 区分，新建网关时选择）：
**深圳统一网关**（深交所 Binary，sz）、**上海竞价网关**（上交所竞价平台 Binary 0.54，shjj）、
**上海新债券网关**（上交所债券平台 Binary 1.90，shbond）与
**上海综合网关**（上交所综合业务平台 EzSTEP 数据库接口，shzh）。
前三类协议代码按目录分放（`sz/` / `shjj/` / `shbond/`，目录内为 protocol.rs / session.rs / strategy.rs）；
shzh 目录多一个 `db.rs`（tiberius 异步访问 SQL Server），session.rs 是数据库轮询任务而非 TCP 会话。
新增网关类型 = 新增一个同构目录，并在 engine.rs 分类分派处登记；
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
│  │   ├─ src/shzh/     # 上海综合网关（EzSTEP 数据库接口）：protocol.rs / db.rs / session.rs / strategy.rs
│  │   │               # （上交所各网关共用 sz/session.rs 的 SessionCtx；shzh 无 TCP，session 为轮询任务）
│  │   ├─ src/lua_engine.rs  # "Lua 脚本回报"模式执行器（沙箱编译运行用户脚本）
│  │   ├─ src/pcf.rs      # ETF 申赎清单解析：深交所 pcf_*.xml + 上交所 ssepcf_*.xml（执行报告明细数据源）
│  │   ├─ src/positions.rs  # ETF 申购持仓：Oracle 查询（spawn_blocking）+ 全局唯一一份持仓 + 按替代标志扣减/现金替代
│  │   ├─ src/capture.rs  # 连接收发报文捕获（16 进制 + 按字段名解析 + 可选持久化到文件）
│  │   ├─ src/orderbook.rs  # 订单缓存（界面订单列表、撤单按状态判断）
│  │   ├─ src/oplog.rs    # 前端操作日志（带前端 IP，按日期存 log/ 目录）
│  │   ├─ src/engine.rs   # 引擎：按网关分类分派会话、网关/平台生命周期管理
│  │   ├─ src/api.rs      # 统一控制接口（供 Tauri 与 server 复用）
│  │   └─ tests/e2e_sz.rs / e2e_shjj.rs / e2e_shbond.rs / e2e_shzh_db.rs / shzh_config.rs  # 端到端测试（前三套真 TCP，端口 18101~ / 18201~ / 18301~；shzh 需真实 SQL Server，未配置环境变量时跳过；shzh_config 只测配置校验）
│  ├─ simx-server/      # 独立后端：axum WebSocket 控制接口（远程部署）
│  ├─ simx-update-server/  # 升级服务器：自动扫描发布目录 + 生成清单 + 文件下载
│  └─ simx-cli/         # 命令行控制工具（自动测试用）：纯 WebSocket 客户端连 simx-server，复用 api 协议，引擎零改动
├─ src-tauri/           # Tauri 2 桌面壳（包名 simuxchange，内嵌 simx-core）
├─ src/                 # 前端 Vue 3 + TypeScript
│  ├─ App.vue           # 主页面（全部界面状态与交互）
│  ├─ components/       # PlatformCard / PlatformEditor / LogPanel / PacketViewer（收发报文弹窗）/ OrderViewer（订单弹窗）/ LuaEditor（Lua 脚本编辑器，CodeMirror 6）/ LuaDocViewer（脚本手册弹窗，marked 渲染）/ PositionSyncDialog（同步持仓数据弹窗）
│  ├─ llmPrompt.ts      # "AI 生成脚本"的系统提示词（编写说明的模型友好版提炼）
│  ├─ llm.ts            # "AI 生成脚本"的大模型调用（接口配置读 simx.config.json 的 llm 字段，浏览器模式回退内置默认；纯前端 + Tauri 壳透传，引擎不参与）
│  ├─ types.ts          # 前后端共享数据结构（与 Rust 结构体手工对应）
│  ├─ backend.ts        # 后端抽象层：LocalBackend(Tauri invoke) / RemoteBackend(WebSocket)
│  ├─ changelog.ts      # APP_VERSION / CHANGELOG（“ⓘ 关于”弹窗的版本号与更新说明）
│  └─ updater.ts        # 更新检查：version.json 清单协议、版本比较（checkForUpdate）
├─ docs/                # 构建调试手册、产品使用手册、协议 PDF、PCF示例/（ETF 清单内容定义表 + 真实示例清单）
├─ scripts/             # build-all.ps1（tauri build 前准备 Linux 产物+Windows CLI+前端）、pack-all.ps1（一键打包并生成 dist\SimuXchange_V<版本号>.7z）、build-glibc-server.ps1（按需交叉编译 glibc 版后端）、musl-cc/ 与 gnu-cc/（zig cc 参数翻译 shim）等
├─ .cargo/config.toml   # 项目级镜像配置（aliyun sparse，覆盖全局 tuna）
├─ simx.config.json     # 运行时配置：backend = local / remote + 可选 updateUrl + llm（AI 生成脚本的模型接口）
└─ Cargo.toml           # Cargo workspace 根
```

数据流：前端每秒轮询 `get_snapshot` 整体替换快照（轮询快照模式，最多 1 秒延迟）；
网关配置持久化在数据目录 `gateways.json`。

**报文捕获**：平台配置 `showPackets`（展示收发报文，单一开关，默认 true；持久化已与
展示合并，保存时 `persistPackets = showPackets` 自动归一化）。勾选后每条报文同时做
三件事：界面弹窗展示、按交易所字段名解析（`ParsedField`，由各协议 protocol.rs 的
`describe_fields` 生成）、持久化到 `<data_dir>/packets/<日期>/<网关名>_<平台名>/` 下的
日志文件（逐行 flush，内容与界面一致）。连接断开时记录器**不删除**、仅 `mark_dead()`
标记离线（平台级报文弹窗按连接分组回看历史）；前端每 0.7s 以 `after_seq` 游标增量拉取
（连接级 `get_conn_packets` / 平台级 `get_platform_packets`）。

## 3. 关键业务逻辑

- **网关分类**：`GatewayConfig.category` = `sz`（深圳统一网关）/ `shjj`（上海竞价网关）/ `shbond`（上海新债券网关）/
  `shzh`（上海综合网关），决定网关下所有平台说哪套协议；`engine.rs` 按 category 分派到对应 session
  （shzh 不分派 TCP，走 `start_db_gateway` 的数据库轮询任务）。新建时选定，创建后不可修改
  （前端重命名弹窗中分类下拉框置灰）。
- **网关 → 平台**两级结构：网关是平台集合，整体启动/停止；
  TCP 网关的一个平台 = 一个监听端口，上海综合网关的一个平台 = 一套数据库连接与接口表。
  **网关运行中禁止增删平台/删除网关**；
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
  仅业务参数不同：PlatformID=2（竞价=0）、BizID=1 债券现券竞价 / 2 债券质押式回购（竞价=100010）、
  协议版本最低 1.90（竞价用 "0.50"）、OrdType 仅支持 2=限价。
- **上海综合网关（EzSTEP 数据库接口，shzh）**：**不走 TCP**，柜台与网关通过 SQL Server
  两张接口表交换 STEP 消息。`PlatformConfig.db`（`DbConfig`）保存主机/端口/库/账号密码/
  委托表名/成交表名/轮询间隔；旧配置没有该字段时按 None 加载。
  连接固定信任 SQL Server 证书（`trust_cert()`，不做证书链校验）——本工具只用于
  测试环境，避免使用者额外理解/配置证书；生产环境不应使用本工具直连数据库。
  - **委托表 reqresp**：柜台写入新委托行（`ordstatus='R'`、`localrespnum=0`、`reqtext=STEP D/F`）；
    网关轮询读取后**回写同一行**的 `ordstatus/resptext/localrespnum`（确认/拒单/撤单响应都在 resptext），
    **没有独立确认表**；成交另插 `execreport`（`execnum/bcasttype/setid/seqnum/recordtimestamp/execreporttext`）。
  - **STEP 格式**：`9=<bodylen><SOH>35=<MsgType><SOH><tag>=<value><SOH>...`，SOH=0x01，
    空串写 `tag= <SOH>`；MsgType D=新委托、F=撤单、8=执行报告、9=撤单拒绝；标签编号沿用 FIX
    （11=ClOrdID、48=SecurityID、38=OrderQty、44=Price、453/448/452=参与方重复组…）。
  - **业务映射**：`protocol::biz_mapping(reqid)` 按规范 1.51 给出 `bcasttype/setid` 与“是否有成交回报”；
    无成交回报的业务（报价/约定购回、股票质押、债券转股/回售、投票等）自动降级为只确认。
  - **策略**：复用同一套 `StrategyConfig`（8 种报单 + 3 种撤单 + Lua）；综合平台无独立业务拒绝消息，
    `RejectVia::BusinessReject` / `cancel_front_reject` 降级为执行报告拒绝（ExecType=8）/撤单拒绝(9)。
  - **并发模型**：`DbHandle` 用 `tokio::sync::Mutex` 串行化一条 SQL Server 连接上的轮询与写库；
    tiberius 是纯 Rust 异步 TDS（固定 0.12.x，rustls+ring，避免 aws-lc 交叉编译问题），
    **不得**在异步任务里改用同步 ODBC/阻塞调用——数据库慢只会拖慢本平台，不能阻塞其他 TCP 网关。
  - **伪连接**：数据库平台在 `connections/recorders` 里登记 `conn_id=1`（peer=数据库地址），
    读到的 reqtext 记为收、写出的 resptext/execreporttext 记为发，报文弹窗/持久化照常可用；
    快照 `PlatformSnapshot.db_connected` 表示数据库连接状态。
  - **启动/停止**：`engine::start_db_gateway` 不绑端口，每个平台 spawn 一个 `shzh::session::run_platform`；
    连接失败每 2 秒重试，平台保持运行（dbConnected=false）；停止时通过 watch 通道取消未到期的回报延迟。
  - **手动回复**：`engine::send_report` 对 shzh 走 `session_shzh::manual_report`（写 reqresp 或 execreport），
    注意不能持有全局 running 锁跨数据库 await（先克隆 Arc 并释放锁）。
- **报单自动回报模式**（四类网关一致，八种）：全部成交 / 部分成交 / 全部成交（随机拆多笔）/
  部分成交（随机拆多笔）/ 只回确认回报 / 自动拒单（执行报告拒绝或业务拒绝二选一；
  旧配置的 custom 自定义成交已废弃，加载时降级为只回确认；综合平台无业务拒绝消息，
  统一按执行报告拒绝）/ Lua 脚本回报（见下条）/
  关闭自动回报（订单进缓存，界面订单弹窗手动回确认/成交/拒单/撤单）。
- **Lua 脚本回报**（`StrategyMode::LuaScript`，配置字段 `lua_script`）：收到新委托时
  `lua_engine.rs` 沙箱中**同步**执行脚本（仅 table/string/math 库，指令数 + 内存上限
  防死循环；整段执行无 await，符合"非 Send 值不跨 await"约定）。输入全局变量 `ord`
  （委托字段，键名=交易所规范名，价格元/数量股）与 `ctx`（网关/平台/分区/柜台地址），
  返回回报计划（`"confirm"`/`"trade"`/`"reject"` 快捷方式，或 `{kind, qty, price, via,
  reason, text, delayMs}` 表数组），由各网关 strategy.rs 的 `plan_reports_lua` 复用既有
  报文构造器落地。只接管新委托（撤单仍走 CancelMode）；运行时出错 → 本笔不回报 + 日志；
  无成交回报业务的 trade 项忽略并告警。**延迟规则**：回报延迟只由脚本各项
  `delayMs`（毫秒，未写 = 0 同步）控制，界面"回报延迟"的确认/成交配置**不参与**
  Lua 模式（前端 Lua 模式隐藏这两个输入框、保存时自动归零）；撤单延迟不受影响
  （撤单仍走 CancelMode 与撤单延迟配置）。保存时 `update_strategy` / `save_gateway`
  强制语法校验（`lua_engine::check_syntax`），api 命令 `validate_lua_script` 供前端
  即时校验；编译缓存挂 `SessionCtx.lua_cache`（mlua 须开 `send` feature 才能放进
  SessionCtx）。编写文档：docs/Lua脚本回报编写说明.md。
  前端另有「AI 生成脚本」（llm.ts 调大模型、系统提示词在 llmPrompt.ts；模型接口
  配置在 simx.config.json 的 `llm` 字段（AppConfig.llm，baseUrl/apiKey/model/
  temperature/maxTokens，桌面端经 get_app_config 读取、浏览器模式回退 llm.ts
  内置默认；保存后端设置时前端原样回传、不会覆盖）；模型接口
  无 CORS 头，桌面壳 llm_chat command 透传 HTTPS，引擎不参与）与「脚本手册」
  （LuaDocViewer 内嵌渲染编写说明文档）。
- **撤单自动回报模式**：默认（原单在途→撤单成功，否则撤单拒单）/ 全部撤单成功（一律成功，
  无订单缓存也成功）/ 全部撤单拒单（一律拒单，原因码 cancel_reject_reason 可配置）；
  勾选 cancel_front_reject（前台拒单）时撤单拒单
  改发业务拒绝消息（深市 MsgType=4 / 沪市 204）。撤单回报延迟 cancel_delay 对撤单成功与拒单均生效。
- **ETF 申赎成份股明细**（深圳非交易处理平台，`platformType=3`）：ETF 实时申购赎回
  新订单（101201）的执行报告 201202 的"成份股明细"不在委托报文里，引擎按平台配置的
  `strategy.etf_pcf_dir`（界面"ETF清单文件路径"，随策略配置保存/热更新）从该目录按
  证券代码查找**日期最新**的清单文件 `pcf_<代码>_<YYYYMMDD>.xml`（PCF XML，解析器
  `pcf.rs`；`find_latest_pcf` 用长度预检 + 字节级大小写不敏感匹配，零拷贝，日期直接
  折数值比较）填充：篮子数 = 委托份额 ÷ CreationRedemptionUnit；每条成份券
  DeliveryQty = ComponentShare × 篮子数（协议 ×100 放大），SubstCash 按方向取
  CreationCashSubstitute（申购）/ RedemptionCashSubstitute（赎回）× 篮子数
  （协议 ×10000 放大）；目录留空、目录不可用或找不到该代码的清单文件时按 0 只成份股
  回复并推送告警日志。**查找/解析带缓存**（`SessionCtx.pcf_cache`，`pcf::PcfCache`，
  按 (目录, 代码, 市场) 记忆）：目录 mtime 未变且未超 30 秒兜底 → 零扫描直接复用（负结果
  同样缓存）；目录变化 → 重扫一次目录，若文件 mtime/大小未变仍复用已解析结果；
  同名文件盘中更新会被文件 stat 发现并重新解析——清单目录可能非常大，避免逐笔
  委托全量遍历目录。报文弹窗对成份股重复组按
  `Security[i].UnderlyingSecurityID/SecurityIDSource/DeliveryQty/SubstCash` 结构化
  解析展示（`describe_ack_extend`，无需额外改动）。**申购（Side=D）且已同步持仓时**，
  上面两项按「ETF 申购持仓扣减」规则调整（见后文），禁止现金替代不足时整笔拒单。
- **上海 ETF 申赎清单**（上海综合网关 EEC/EER/IEC/IER）：同一 `strategy.etf_pcf_dir`
  在 shzh 平台也生效，查找 `ssepcf_<代码>_<YYYYMMDD>.xml`（`pcf::parse_sse_pcf` /
  `PcfCache::load_sse`；缓存 key 增加市场标志，避免与深交所同代码串用）。
  - **EEC/EER 现金申赎**：执行报告 = 类型1（ETF 成交）+ 类型3（资金替代，
    `MarginAmtType=9`）；现金替代按替代标志 1/2/3/0 处理：标志 1 乘 1±溢价/折价，
    标志 2（必须）/3（退补）直接取金额，标志 0 不计；单只/汇总先四舍五入 3 位，
    乘篮子数后最终 2 位。
  - **IEC/IER 实物申赎**：类型1 + 每只可过户成份券一条类型2（`LastQty=Quantity×篮子数`，
    方向与 ETF 相反）+ 类型3（有现金替代时；按市场 ID 101→1/102→2/103→3/其他→9 分组）。
  - ETF 不支持部分成交/拆单，统一按整笔成交；Lua 脚本模式不参与 PCF。
  - 无清单/目录未配置/解析失败：仍回结构完整的“类型1 + 类型3（0）”，日志告警。
  - 相关：`shzh/strategy.rs::plan_etf_records`、`shzh/session.rs::load_etf_pcf`、
    `shzh/protocol.rs::build_etf_trade`；界面字段与深圳共用（shzh 也显示）。
- **ETF 申购持仓扣减（同步持仓数据）**：深圳非交易处理平台（平台类型 3）与上海综合
  平台的「ETF清单文件路径」下方有「同步持仓数据」按钮（问号说明“用于作为成分股进行ETF申购”），
  弹窗填写 Oracle 连接信息（环境名/连接串/用户名/密码）与资金账号，点「查询持仓」执行
  `TRA_SEC_POSITION` + `EX_TRADING_SECURITY` 联查（账号用绑定变量）；结果六列
  （证券代码/名称/持有/可用/可申赎/冻结）可编辑，保存后**整份替换**服务端内存中
  **全局唯一一份**持仓（引擎 `Inner.positions: Arc<PositionStore>`，所有网关/平台共享；
  下次打开弹窗回填上次连接信息与账号，密码只存进程内存）。
  - **线程模型**：rust-oracle 为阻塞驱动，`Engine::query_positions` 用
    `tokio::task::spawn_blocking` + 20 秒超时，不阻塞异步工作线程；运行时需安装
    Oracle Instant Client（未安装只影响查询，不影响网关）。Linux musl 静态版
    无法加载 glibc 版 Instant Client，该功能建议 Windows 桌面端使用；Linux 需用
    glibc 目标构建 simx-server，并在 `start.sh` 的 `ORACLE_CLIENT_DIR` 指定客户端目录。
  - **扣减规则**（仅申购：深圳 Side=D、上海 **IEC 实物申购**；上海 EEC 现金申购与
    全部赎回不扣）：按 PCF 每篮子数量 × 篮子数
    得到要求数量；替代标志 **0 禁止**：可申赎数量不足 → 整笔拒单（深圳按 `reject_via`、
    上海统一执行报告拒绝），足够则扣减；**1 允许**：优先扣减，覆盖多少过户多少，未覆盖
    部分按比例转现金替代；**2 必须 / 3 退补**：不扣持仓，全部现金替代。
  - 实现：`positions::PositionStore::adjust_creation_pcf` 返回调整后的 `PcfFile`
    克隆（component_share 缩小、替代金额按未覆盖比例保留），深圳 `pcf_delivery_details`、
    上海 `plan_etf_records` 的既有报文构造逻辑无需感知持仓；未保存过持仓
    （`rows == None`）时不启用。关闭自动回报/Lua/自动拒单（上海还包括只回确认）
    不参与扣减。日志不记录密码。
  - 测试：`positions` 单元测试 6 个 + 上海调整后回报 1 个 + 深圳 e2e 17
    （含持仓扣减/连续两笔/拒单）。
- **多分区号**：`PlatformConfig.partition_nos`（逗号分隔字符串）支持一个平台配置多个分区号，
  回报按**证券代码哈希**（`partition_for(&security_id)`）分配到其中一个分区——同一证券恒落
  同一分区，保证该证券的确认/成交/撤单回报都在同一分区；为空时回退单分区 `partition_no`。
  登录时下发的平台信息（深 9 / 沪 208）携带全部分区列表。
- **策略热更新**（网关不停机实时生效）：`SessionCtx.strategy` 为 `Arc<std::sync::RwLock<StrategyConfig>>`
  运行时共享区，与持久化配置（gateways.json）分离；每次生成回报计划时在块作用域内加读锁
  读取最新值（`RwLockReadGuard` 不是 Send，**不能跨 await 存活**）；`engine.update_strategy`
  同时写运行时共享区与持久化配置（api 命令 `update_strategy`），前端在网关运行中保存平台时走此命令。
  `plan_reports` 签名为 `(st: &StrategyConfig, partition_no: i32, order, stats, env, [pbu])`——策略与分区号分离，
  `env: &lua_engine::ScriptEnv` 是 Lua 脚本模式的执行环境（缓存/网关平台信息/日志回调，非 Lua 模式不用但统一携带）。
- **自动启动**：simx-server / 桌面端均支持 `--auto-start` 参数，引擎创建后调用
  `engine.auto_start_previous()` 只恢复**上次运行中**的网关（启停状态并入
  `data_dir/gateways.json` 的 `was_running` 字段：start_gateway 置 true、
  stop_gateway 置 false、删除随配置消失；旧版无该字段视为 false）。
  不带参数则不自动启动任何网关。
- **Linux 后端两个版本**：发布包 `Linux/simx-server/` 同时提供 `simx-server-musl`
  （默认，musl 静态，任意 x86_64 Linux）与 `simx-server-glibc`（Oracle 持仓查询用，
  需 glibc ≥ 2.17）；`start.sh [musl|glibc]` 选择（默认 musl），`stop.sh` 两个都停。
  glibc 版由 `scripts/build-glibc-server.ps1` 用 zig 交叉编译（glibc 2.17 基线），
  pack-all.ps1 会一并生成并放进 7z。
- **自动检查升级**（目录发布，无安装包模式）：桌面端在「后端连接设置」里配置
  `updateUrl`（AppConfig.update_url，simx.config.json 的 camelCase 字段），启动
  3 秒后检查 `<updateUrl>/version.json`（数字点分比较 `compareVersions`）。桌面端
  （Tauri）的检查/下载/应用更新逻辑在 **src-tauri/src/updater.rs**（ureq 拉清单
  避开 WebView CORS；`src/updater.ts` 只服务浏览器调试）：① `check_update_server`
  拉清单比版本；② 发现新版后前端自动转**后台下载**（不弹窗，顶栏「ⓘ 关于」显示
  「⇣ 下载中」），`download_update_files` 把清单 `files` 全部下载到
  `%LOCALAPPDATA%\SimuXchange\updates\<版本>`，逐文件校验 size/sha256，
  **主 exe（simuxchange.exe）恒最后下载**，完成写 pending.json；③ 点顶栏红色
  「↻ 重启以更新」（`apply_update` command，restart=true）或关闭窗口
  （`on_window_event` 的 CloseRequested，restart=false）时，以 `--apply-update`
  参数启动下载目录里的**新版 exe 充当更新器**：等主进程（父 PID，Windows
  WaitForSingleObject）退出 → 把下载目录文件覆盖到 exe 所在目录（同样主 exe
  最后）→ 按需重启 → 删 pending.json。只覆盖清单内文件，用户数据（gateways.json
  等）不在清单就不被碰。有 pending.json 时启动界面显示红色按钮
  （`get_pending_update`）。检查失败静默忽略。

  升级服务器 `simx-update-server`（crates/simx-update-server/src/main.rs）：唯一部署
  方式是 `release.txt`（纯文本一行版本号）与程序同目录，**发布文件放同名版本目录**
  （如 `2.1.0/`，按 release.txt 动态解析，换版本无需重启）；服务默认监听
  `0.0.0.0:9860`，`GET /update/version.json` 实时扫描版本目录（含子目录）生成
  `files` 清单（name/size/sha256；隐藏文件不进清单），`GET /update/*rest` 提供下载
  （axum 0.7 通配路由，支持子目录，`..` 拒绝），`GET /api/files` + `DELETE /api/file/:name` 供
  管理页清理。无 release.txt 或发布文件为空时返回 404（客户端静默）。客户端更新
  地址填 `http://<IP>:9860/update`，发布流程见 README「自动检查升级」/
  《产品使用手册》5.5 节；发布包 `Linux/simx-update-server/` 内含
  `start-update.sh` / `stop-update.sh`（nohup 后台启动，日志追加到
  `update-server.log`）。simx-server 不带自升级功能。
- **延迟规则**：min=max=0 同步回报；否则异步，区间内随机抽样。
- **报文捕获**：见第 2 节“报文捕获”段（各网关共用 `SessionCtx::start_capture(conn_id, peer)`；
  连接断开时记录器保留仅 `mark_dead()` 标记离线，**不删除**（平台级报文弹窗按连接分组回看）；
  读侧在 read_frame 后记录，写侧在 writer 任务写出前记录——上交所需在 `finalize_seq` 补完
  MsgSeqNum 之后捕获，保证文件里是真实上线字节；shzh 用 `start_capture_peer` 以数据库地址
  建伪连接，记录 reqtext/resptext/execreporttext 的 STEP 文本）。

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

- **版本号**：除非用户明确要求升版本，否则**不要自动修改任何版本号**
  （`package.json`、`src-tauri/tauri.conf.json`、各 crate 的 `Cargo.toml`、
  `src/changelog.ts` 等一律保持原值）。
- 前端遵循既有模式：状态集中在 `App.vue`，子组件只通过 props 接收、emit 上报；
  所有后端调用走 `backend.ts` 抽象层，**不得**在组件里直接 `invoke`；
- 修改协议/策略逻辑后必须跑 `cargo test -p simx-core`（含 30 个 TCP e2e 测试：深交所 17 + 上交所竞价 7 + 上交所新债券 6；
  上海综合网关的 `e2e_shzh_db` 需真实 SQL Server，未设置环境变量时自动跳过）；
- 右键菜单、弹窗等 UI 交互遵循现有实现风格（贴边修正、运行态置灰、Esc 关闭）；
- 端口约定：Vite 开发端口 **14200**（`vite.config.ts` 与 `tauri.conf.json` 的
  devUrl 两处必须一致）；e2e 测试占用 18101~18116（深交所）、18201~18206（上交所竞价）与 18301~18305（上交所新债券）；平台默认端口从 10001 起。

## 5. 常用命令（PowerShell）

> 本机 shell 为 PowerShell：**不支持 `&&`，用 `;` 分隔命令**。
> 找不到 cargo 时先执行 `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`。
> cargo 的 ExitCode 非 0 **不代表失败**——以输出末尾 `Finished` / `test result: ok` 为准
> （PowerShell 把进度输出当 stderr；详见构建手册坑⑥）。

| 目的 | 命令 |
| --- | --- |
| 安装前端依赖（首次） | `npm install` |
| 开发调试（桌面端热更新） | `npm run tauri dev` |
| 只跑前端（浏览器） | `npm run dev` |
| Rust 快速检查 | `cargo check -p simx-core -p simx-server` |
| Tauri 主程序检查 | `cargo check --manifest-path src-tauri/Cargo.toml` |
| 全部测试 | `cargo test -p simx-core` |
| 前端构建验证 | `npx vite build`（类型检查：`node .\node_modules\typescript\bin\tsc --noEmit`） |
| 打包桌面发布版 | `npm run tauri build`（只出 exe，不生成安装包） |
| 构建独立后端 | `cargo build --release -p simx-server` |
| 构建 CLI 工具 | `cargo build --release -p simx-cli`（用法见 docs/CLI工具使用手册.md，速查见产品手册第 8 章） |
| 一键打包全部产物 | `.\scripts\pack-all.ps1`（生成 dist\pkg\SimuXchange 与 dist\SimuXchange_V<版本号>.7z，见构建手册 6.1） |

## 6. 环境与踩坑记录（不在本文件维护）

环境准备与踩坑排查统一记录在 [docs/构建调试手册.md](docs/构建调试手册.md)：
环境准备（含 zig 交叉编译器安装）见第 2 节，踩坑记录见第 8 节坑①~⑪
（端口占用、custom-protocol、ExitCode 误判、os error 5、依赖镜像、ps1 编码、
zig 交叉编译、目录搬迁缓存等）。

**新踩到的坑一律记入手册第 8 节，不写入本文件**——本文件只保留每次协作
都需要的通用约定（见文件头"维护原则"）。

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
- [ ] 涉及协议/策略/会话时：`cargo test -p simx-core` 全绿（148 单元 + 4 自动启动 + 3 综合配置集成 + 30 TCP e2e；DB e2e 2 个需环境变量）
- [ ] 涉及前端时：`npx vite build` 通过 + `tsc --noEmit` 无错误
- [ ] 前后端类型已同步（types.ts ↔ Rust 结构体）
- [ ] 新增逻辑已按 4.1 规范补注释
- [ ] 已按第 7 节检查两份手册与 README 是否需要更新
