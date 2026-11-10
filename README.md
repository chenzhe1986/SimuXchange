# SimuXchange · 模拟撮合网关

跨平台模拟撮合软件：模拟交易所交易网关（TGW），接受柜台（OMS）的 Binary 协议连接，
按配置策略回送确认回报 / 成交回报，无真实订单簿与撮合。

项目介绍、部署形态（前后端一体 / 分离）、性能与线程模型等概览见
[docs/项目简介.md](docs/项目简介.md)。

**开源主页**（本项目遵循 GPL 开源协议）：[GitHub](https://github.com/chenzhe1986/SimuXchange) · [Gitee](https://gitee.com/chenzhe1986/SimuXchange)

支持四类网关（新建网关时选择分类）：
- **深圳统一网关**：深交所 Binary 协议；
- **上海竞价网关**：上交所竞价平台 Binary 0.54 协议；
- **上海新债券网关**：上交所新债券平台 Binary 1.90 协议；
- **上海综合网关**：上交所综合业务平台 EzSTEP **数据库接口**——柜台与网关不直连 TCP，
  而是通过 SQL Server 的委托表 `reqresp` / 成交表 `execreport` 交换 STEP 消息。

界面功能：网关可按**目录**分组（最多两层嵌套，目录数据存后端、折叠状态存前端），
侧栏与主区之间的分界线可拖动调整侧栏宽度；
网关支持**标签/备注**（存后端，侧栏可按标签筛选）、**创建/修改时间**
（右键「详情」弹窗查看，网关标签与备注可在弹窗内编辑）。

## 项目结构

```
SimuXchange/
├─ crates/
│  ├─ simx-core/        # 核心库：三类 Binary 协议 + 上海综合 EzSTEP 数据库接口、模拟回报引擎、统计
│  ├─ simx-server/      # 独立后端：WebSocket 控制接口（Linux/Windows 远程部署用）
│  ├─ simx-cli/         # 命令行控制工具（自动测试用）
│  └─ simx-update-server/  # 升级服务器：发布目录自动扫描 + 生成 version.json + 文件下载
├─ src-tauri/           # Tauri 桌面壳（内嵌 simx-core 引擎）
├─ src/                 # 前端 Vue 3 + TypeScript（changelog.ts 版本说明 / updater.ts 更新检查）
├─ docs/                # 文档资料：协议 PDF、界面参考图、PDF 提取文本
├─ scripts/             # 辅助脚本（build-all.ps1 打包前准备、图标生成等）
├─ simx.config.json     # 应用配置：本地引擎 / 远程后端、更新服务器、AI 生成脚本的模型接口
└─ Cargo.toml           # Cargo workspace
```

本项目遵循 GPL 开源协议。联系邮箱：chenzhe1986@126.com

## 环境要求（Windows 开发机）

| 工具 | 说明 | 检查命令 |
| --- | --- | --- |
| Rust（MSVC 工具链） | `winget install Rustlang.Rustup`（需已装 VS C++ 生成工具） | `cargo --version` |
| Node.js ≥ 18 | 前端构建 | `node --version` |
| WebView2 运行时 | Win10/11 一般自带 | — |
| musl 交叉 C 编译器 | 仅**打包发布版**需要（mlua 的 Lua C 源码要交叉编译到 musl）：安装 [zig](https://ziglang.org/download/)（Windows x86_64 版，解压后把目录加入 PATH），build 脚本自动用 `zig cc` 充当交叉编译器；详见构建调试手册第 2 节 | `zig version` |
| Oracle Instant Client | 仅使用「同步持仓数据」（Oracle 持仓查询）功能时需要，运行期动态加载；Linux 需把 Instant Client 目录加入 `LD_LIBRARY_PATH`（`start.sh` 已预留 `ORACLE_CLIENT_DIR`），且**只有 glibc 版 simx-server 能加载**（musl 静态版不支持） | 查询持仓按钮能连上数据库 |

> 本机已完成安装并编译通过。新开终端若提示找不到 cargo，重开一个终端即可（安装器已写入 PATH）。

## 编译与运行

### 1. 开发模式（热更新调试）

```powershell
npm install          # 首次执行一次
npm run tauri dev    # 启动桌面端（首次编译较慢，约几分钟）
```

### 2. 构建发布版（桌面端）

```powershell
npm run tauri build     # 只编译桌面端 exe（不生成安装包）
.\scripts\pack-all.ps1  # 推荐：编译全部产物并生成发布压缩包（见第 7 节）
```

构建时会自动执行 `scripts/build-all.ps1`：**每次打包按源码指纹增量编译** Linux
三件套与 Windows 版 CLI、构建前端；Tauri 配置 `bundle.active=false`，
**不再生成 NSIS 安装包**。
注意：mlua（Lua 脚本回报）的 vendored Lua C 源码需要 **musl 交叉 C 编译器**
（装好 zig 即可，本机无则打包中止并提示安装方法，见环境要求表）。

产物：
- 免安装 exe：`target\release\simuxchange.exe`
- Linux 三件套：`target\x86_64-unknown-linux-musl\release\{simx-server,simx-update-server,simx-cli}`

Linux 服务端如需使用「同步持仓数据」（Oracle），musl 静态版无法加载 Oracle 客户端，
用 `.\scripts\build-glibc-server.ps1` 额外生成 glibc 版 `dist\linux-glibc\simx-server-glibc`
（详见构建调试手册第 2 节；一键打包时也会自动生成并放进 7z）。

完整绿色版发布包（程序 + 配置 + 脚本 + 文档）用 `.\scripts\pack-all.ps1` 生成。

部署时把 `simx.config.json` 与 exe 放同一目录；网关配置保存在同目录 `gateways.json`。

### 3. 构建独立后端（远程部署）

```powershell
cargo build --release -p simx-server
```

产物 `target\release\simx-server.exe`，运行：

```powershell
.\simx-server.exe --listen 0.0.0.0:9800
```

网关配置与报文文件等数据保存在 **exe 所在目录**（`gateways.json` / `packets/` / `log/`）。

> Linux 发布包同时提供 `simx-server-musl`（默认，静态无依赖）和
> `simx-server-glibc`（Oracle 查询用，需 glibc ≥ 2.17）两个版本，
> 启动脚本用 `./start.sh`（默认 musl）或 `./start.sh glibc` 选择；详见产品使用手册 1.2 节。

加 `--auto-start` 参数后，后端启动完成会**自动恢复上次运行中的网关**（引擎把
网关启停状态记在 `gateways.json` 的 `wasRunning` 字段里，只启动上次退出时仍在运行的网关）；
不带该参数则所有网关都不会自动启动，需在界面手动开启。
桌面端（simuxchange.exe）同样支持 `--auto-start`。

查看版本号：`simx-server --version`（正常启动时横幅也会打印版本，
如 `SimuXchange 后端已启动 v2.0.0`）。

Linux 下在装好 Rust 的机器上同样执行 `cargo build --release -p simx-server` 即可
（simx-server 不依赖任何 GUI 库，可直接在服务器上编译运行）；
跨发行版分发（如 Ubuntu 编译 → CentOS 7 运行）需用 **musl 静态编译**
（`--target x86_64-unknown-linux-musl`，详见构建调试手册 6.2 节）。

### 4. 前端连接远程后端

编辑桌面端目录下的 `simx.config.json`：

```json
{
    "backend": "remote",
    "remoteUrl": "ws://<服务器IP>:9800/ws"
}
```

`backend` 为 `local` 时使用内嵌引擎（合并目录部署），为 `remote` 时连接远程 simx-server。

同一文件还可配置「AI 生成脚本」使用的大模型接口（缺省时用内置默认值）：

```json
{
    "llm": {
        "baseUrl": "https://developer.amd.com.cn/radeon/v1",
        "apiKey": "rc-xxxxxxxx",
        "model": "MiMo-V2.6-Flash",
        "temperature": 0.2,
        "maxTokens": 8192
    }
}
```

`baseUrl` 写到版本号为止（程序自动拼 `/chat/completions`），需兼容 OpenAI 协议；
换服务商/模型只改这段配置，重启程序生效（纯浏览器调试模式无配置文件，使用前端内置默认）。

### 5. 自动检查升级

**桌面端**：顶栏「ⓘ 关于」可查看当前版本号与更新说明（版本定义在 `src/changelog.ts`）。
「后端连接设置」弹窗里填「更新服务器地址」（如 `http://192.168.1.10:9860/update`，
可选）后，
每次启动 3 秒后自动向该服务器拉取 `version.json` 检查新版本：发现新版后在**后台
自动下载整个发布目录**（不弹窗、不打断操作，主 exe 最后下载、逐文件校验 sha256），
顶栏「ⓘ 关于」显示「⇣ 下载中」；下载完成后变红色「↻ 重启以更新」——点击则覆盖旧文件并自动
重启，或直接关闭程序，下次启动即为新版本。覆盖由新版 exe 以 `--apply-update` 参数
执行，保证"其它文件先覆盖、exe 最后覆盖"，且只替换服务器清单里的文件，本机配置
（`simx.config.json`、`gateways.json` 等）不受影响。检查失败静默忽略，不影响启动。

**服务器端发布新版**：程序目录布局如下（发布目录名 = `release.txt` 里的版本号）：

```
simx-update-server
├─ release.txt      ← 版本号文件，与程序同级
└─ 2.1.0/           ← 以版本号命名的发布目录
```

1. **发布内容**：把压缩包解压后的完整文件夹（simuxchange.exe 与其它文件/子目录）
   拷进与版本号同名的目录（如 `2.1.0/`）；
2. **版本号文件**：在**程序同目录**放 `release.txt`，内容仅一行版本号（如 `2.1.0`）。

启动 `simx-update-server`（默认监听 9860 端口），客户端「更新服务器地址」填
`http://<服务器IP>:9860/update`。服务器按 `release.txt` 找到同名版本目录，自动
扫描生成 `version.json`（文件清单含子目录、size、sha256，用户不用手写），并从
`/update/<相对路径>` 提供下载。换版本只需改 `release.txt` 并放好对应目录，无需重启。

### 6. CLI 命令行控制工具（simx-cli，自动测试用）

`simx-cli` 是不依赖界面的命令行控制工具：连到已启动的 simx-server，用一条条
命令完成"查状态 / 改策略 / 手动回报"等自动化测试常用操作（每次调用一条命令、
退出即返回，适配 shell 脚本与 CI）。引擎与 simx-server 本身零改动，CLI 只是
现有 WebSocket 控制协议的一个客户端。

构建（Windows，产物 `target\release\simx-cli.exe`）：

```powershell
cargo build --release -p simx-cli
```

先启动后端，登录一次后即可免地址调用（CLI 默认连 `ws://127.0.0.1:9800/ws`，
单条命令可用 `--url` 临时覆盖）：

```powershell
.\simx-cli.exe login --url ws://<服务器IP>:9800/ws   # 登录：探活+保存地址（写用户主目录 .simx-cli.json），此后免 --url
.\simx-cli.exe snapshot
.\simx-cli.exe set-strategy --gateway <网关名或id> --platform <平台名或id> --mode ackOnly --ack-delay 100,300
.\simx-cli.exe orders --gateway <网关名或id> --platform <平台名或id>        # 查委托编号
.\simx-cli.exe send --gateway <网关名或id> --platform <平台名或id> --order <委托编号> trade --qty 100
.\simx-cli.exe logout                # 清除保存的地址，恢复默认本机
```

退出码：0 = 成功；1 = 后端错误/连接失败；2 = 参数错误。加 `--json` 输出后端
原始 JSON 响应，便于脚本解析。完整命令说明与用例见
[docs/CLI工具使用手册.md](docs/CLI工具使用手册.md)（速查见《产品使用手册》第 8 章）。

### 7. 一键打包（全部产物）

```powershell
.\scripts\pack-all.ps1
```

一条命令完成全部编译并生成绿色版发布包（不再生成 NSIS 安装包；`dist\` 不进仓库）：

- `dist\pkg\SimuXchange\`：解压即用的发布目录（便于检查）；
- `dist\SimuXchange_V<版本号>.7z`：最终发布包，内部布局固定为：

```
SimuXchange\
├─ simuxchange.exe
├─ simx-cli.exe
├─ simx.config.json
├─ docs\   （CLI工具使用手册 / Lua脚本回报编写说明 / 产品使用手册 / 项目简介）
└─ Linux\
   ├─ simx-cli\simx-cli
   ├─ simx-server\
   │  ├─ simx-server-musl    默认，静态链接，任意 x86_64 Linux 可用
   │  ├─ simx-server-glibc   需要 Oracle 查询时用，要求 glibc >= 2.17
   │  └─ start.sh / stop.sh  （./start.sh [musl|glibc] 选择，默认 musl）
   └─ simx-update-server\{simx-update-server,start-update.sh,stop-update.sh}
```

版本号取自 `src-tauri/tauri.conf.json`，脚本只读取、不自动升版本。

## 使用流程

1. 启动桌面端 → 左侧「新建」交易网关，选择**网关分类**（深圳统一网关 / 上海竞价网关 / 上海新债券网关 / 上海综合网关，创建后不可改）
2. 选中网关 →「添加平台」，配置监听端口与模拟回报策略（平台类型选项随网关分类变化）；
   上海综合网关改为配置 SQL Server 地址、账号、委托表/成交表名与轮询间隔
3. 「启动网关」→ TCP 网关开始监听柜台连接；上海综合网关开始轮询数据库委托表
4. 柜台按对应交易所协议连接登录 / 写入委托表，网关回登录应答或回写确认/成交
5. 柜台发送现货新订单（或综合平台写入 STEP 委托）→ 网关按策略回确认 / 成交 / 拒单
6. 界面实时展示连接数、委托/确认/成交/拒单统计与运行日志（综合平台显示数据库连接状态）
7. 顶栏 ◐ 按钮可切换五套界面配色（经典黑/现代蓝/深空灰/极光紫/优雅白），选择自动保存

## 模拟回报策略

界面里报单自动回报模式先选**大类**（固定回报模式 / Lua 脚本配置 / 回放行情撮合（待开发，暂按关闭自动回报处理）/ 关闭自动回报），选“固定回报模式”后右侧再选下表的具体模式：

| 模式 | 行为 |
| --- | --- |
| 全部成交（单笔） | 1 条确认 + 1 条全量成交 |
| 全部成交（多笔拆单） | 1 条确认 + N 条成交，数量随机拆分，价格按档位买单递增/卖单递减（不超过委托价） |
| 部分成交（单笔） | 1 条确认 + 1 条随机数量的部分成交 |
| 部分成交（多笔拆单） | 1 条确认 + N 条部分成交 |
| 不自动回复（挂单手动回复） | 不自动回任何回报，委托登记进订单缓存，由订单界面手动回复确认/成交/拒单/撤单 |
| 不成交挂单 | 只回确认 |
| 拒单 | 回 200102(ExecType=8) 或业务拒绝消息(MsgType=4)，原因码/文本可配置 |
| Lua 脚本回报 | 执行自定义 Lua 脚本，按脚本返回值生成任意组合的确认/成交/拒单（可以委托的任意字段作为条件，沙箱化执行、保存时校验语法），详见 [docs/Lua脚本回报编写说明.md](docs/Lua脚本回报编写说明.md)；编辑框默认放需求示例，支持 AI 自然语言生成（生成结果回填同一编辑框）与内置脚本手册 |

延迟配置：最小 = 最大为固定延迟；范围随机；全为 0 时在收单流程内同步回报，否则异步发送。

## 协议实现范围

### 深圳统一网关（深交所 Binary）

- 会话层：Logon(1)、Logout(2)、Heartbeat(3)、平台状态(6)、平台信息(9)，校验和校验、心跳超时检测
- 业务层：新订单 100101、确认回报 200102、成交回报 200115、业务拒绝(4)、撤单 190007 → 撤单失败 290008、ETF 实时申购赎回 101201 → 执行报告 201202（成份股明细按「ETF清单文件路径」配置的 PCF 清单文件回复；同步持仓后申购按可申赎数量扣减、不足部分转现金替代、禁止现金替代不足时拒单，见产品手册 4.4.1/4.4.3）
- 编码规则：大端序、定长字符串补空格、Price N13(4)、Qty N15(2)；报文头 MsgType(4)+BodyLength(4)

### 上海竞价网关（上交所竞价平台 Binary 0.54）

- 会话层：Logon(40)、Logout(41)、Heartbeat(33)、平台状态(209)、执行报告信息(208)、分区序号同步(206/207)，校验和校验、心跳区间[5,60]
- 业务层：新订单 58、申报响应/执行报告 32、成交回报 103、订单拒绝 204、撤单 61 → 撤单失败 59
- 编码规则：大端序、报文头 MsgType(4)+MsgSeqNum(8)+MsgBodyLen(4) 共 16 字节（比深交所多了消息序号），Price N13(5) = 价格×100000、Qty N15(3) = 股数×1000；**登录后需先完成分区序号同步(206/207)才会推送执行报告**

### 上海综合网关（上交所综合业务平台 EzSTEP 数据库接口）

- **不走 TCP**：柜台把委托写入 SQL Server 的 `reqresp`（请求及响应接口表），网关按配置的轮询间隔读取 `ordstatus='R'` 的行
- 委托解析：STEP 消息 `9=<长度><SOH>35=D<F>...`，支持新委托(D)/撤单(F)；业务类型取 `reqresp.reqid`（EEC/PFP/CBT 等，任意 3 位业务码均可）
- 回报写库：确认/拒单/撤单响应**回写同一行** `reqresp` 的 `resptext + localrespnum + ordstatus`；成交插入 `execreport`（`bcasttype/setid` 按 reqid 映射规范业务表）
- 策略范围：与 TCP 网关相同的 8 种报单模式与 3 种撤单模式（含 Lua 脚本）；规范上无成交回报的业务自动降级为只回确认；综合平台没有独立的“业务拒绝消息”，拒单统一用执行报告 `ExecType=8`
- **ETF 申赎清单（PCF）**：配置「ETF清单文件路径」后，EEC/EER 现金申赎按 `ssepcf_<代码>_<日期>.xml` 计算现金替代总额并回“类型1 + 类型3”；IEC/IER 实物申赎再按清单逐只生成“类型2 成份券成交”；替代标志 0/1/2/3 与申购溢价/赎回折价按 IS118 规范计算；「同步持仓数据」可从 Oracle 同步成份股库存，**IEC 实物申购**按持仓扣减/现金替代（EEC 现金申购不扣减，详见产品手册 4.4.2/4.4.3）
- 访问驱动：tiberius（纯 Rust 异步 TDS），不依赖 ODBC；数据库断开自动重连，不阻塞其他 TCP 网关
- 配置项：数据库主机/端口/库名/账号/密码、委托表名、成交表名、轮询间隔（见产品手册 4.7）

## 测试

```powershell
cargo test -p simx-core        # 148 个单元测试 + 4 个自动启动 + 3 个综合配置集成测试 + 30 个 TCP 端到端测试（深交所 17 + 上交所竞价 7 + 上交所新债券 6）+ 2 个上海综合数据库 e2e（基础流程 + ETF PCF，需 SQL Server 环境变量，未设置时跳过）
```

端到端测试覆盖：TCP 连接 → Logon 握手 →（上交所还含分区序号同步）→ 委托 → 回报字段/校验和验证 → 网关停止 Logout（含 Lua 脚本回报模式的数量分档脚本全流程、ETF 申赎按 PCF 清单文件回成份股明细、ETF 申购按同步持仓扣减/现金替代/拒单）。
