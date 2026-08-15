# SimuXchange · 模拟撮合网关

跨平台模拟撮合软件：模拟交易所交易网关（TGW），接受柜台（OMS）的 Binary 协议连接，
按配置策略回送确认回报 / 成交回报，无真实订单簿与撮合。

支持三类网关（新建网关时选择分类）：
- **深圳统一网关**：深交所 Binary 协议；
- **上海竞价网关**：上交所竞价平台 Binary 0.54 协议；
- **上海新债券网关**：上交所新债券平台 Binary 1.90 协议。

## 项目结构

```
SimuXchange/
├─ crates/
│  ├─ simx-core/        # 核心库：深/沪三套 Binary 协议编解码、会话管理、模拟回报引擎、统计
│  ├─ simx-server/      # 独立后端：WebSocket 控制接口（Linux/Windows 远程部署用）
│  └─ simx-update-server/  # 升级服务器：网页上传安装包 + 自动生成 version.json
├─ src-tauri/           # Tauri 桌面壳（内嵌 simx-core 引擎）
├─ src/                 # 前端 Vue 3 + TypeScript（changelog.ts 版本说明 / updater.ts 更新检查）
├─ docs/                # 文档资料：协议 PDF、界面参考图、PDF 提取文本
├─ scripts/             # 辅助脚本（build-all.ps1 打包前准备、图标生成等）
├─ simx.config.json     # 应用配置：本地引擎 / 远程后端
└─ Cargo.toml           # Cargo workspace
```

本项目遵循 GPL 开源协议。联系邮箱：chenzhe1986@126.com

## 环境要求（Windows 开发机）

| 工具 | 说明 | 检查命令 |
| --- | --- | --- |
| Rust（MSVC 工具链） | `winget install Rustlang.Rustup`（需已装 VS C++ 生成工具） | `cargo --version` |
| Node.js ≥ 18 | 前端构建 | `node --version` |
| WebView2 运行时 | Win10/11 一般自带 | — |

> 本机已完成安装并编译通过。新开终端若提示找不到 cargo，重开一个终端即可（安装器已写入 PATH）。

## 编译与运行

### 1. 开发模式（热更新调试）

```powershell
npm install          # 首次执行一次
npm run tauri dev    # 启动桌面端（首次编译较慢，约几分钟）
```

### 2. 构建发布版（桌面端）

```powershell
npm run tauri build
```

构建时会自动执行 `scripts/build-all.ps1`：确保 Linux 后端产物
`target\x86_64-unknown-linux-musl\release\simx-server` 存在（缺失时自动交叉编译），
并把该文件打进安装包，安装后位于安装目录的 `Linux\simx-server`
（供 Linux 服务器远程部署用，见第 3 节）。

产物：
- 免安装 exe：`target\release\simuxchange.exe`
- NSIS 安装包：`target\release\bundle\nsis\SimuXchange_1.0.0_x64-setup.exe`

部署时把 `simx.config.json` 与 exe 放同一目录；网关配置保存在同目录 `gateways.json`。

### 3. 构建独立后端（远程部署）

```powershell
cargo build --release -p simx-server
```

产物 `target\release\simx-server.exe`，运行：

```powershell
.\simx-server.exe --listen 0.0.0.0:9800
```

网关配置与报文文件等数据保存在 **exe 所在目录**（`gateways.json` / `packets/`）。

加 `--auto-start` 参数后，后端启动完成会**自动恢复上次运行中的网关**（引擎把
网关启停状态记在 `gateways.json` 的 `wasRunning` 字段里，只启动上次退出时仍在运行的网关）；
不带该参数则所有网关都不会自动启动，需在界面手动开启。
桌面端（simuxchange.exe）同样支持 `--auto-start`。

加 `--update-url <地址>` 参数后，启动时向更新服务器检查一次版本并打印提示
（仅提示不自动替换，详见下文“自动检查升级”）。

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

### 5. 自动检查升级

**桌面端**：顶栏「ⓘ 关于」可查看当前版本号与更新说明（版本定义在 `src/changelog.ts`）。
「后端连接设置」弹窗里填「更新服务器地址」（如 `http://192.168.1.10:8080`，可选）后，
每次启动 3 秒后自动向该服务器拉取 `version.json` 检查新版本：发现新版弹窗提示，
点「下载并安装」自动把安装包下载到本机 Downloads 并启动安装程序。

**升级服务器**（二选一）：

1. **推荐：项目自带升级服务器程序 `simx-update-server`**（`cargo build --release -p simx-update-server`），
   浏览器打开即管理页面：网页上传安装包（文件名须含版本号）与更新说明，自动生成 `version.json`，
   无需登录服务器、无需手写清单（详见《构建调试手册》6.6 节）；
2. 任意 HTTP 静态目录：把发布文件放到目录即可（见下）。

服务器上的文件布局（静态目录方式，把发布文件放到任意 HTTP 可访问的目录即可）：

```
<updateUrl>/
├─ version.json          # 版本清单（见下）
├─ SimuXchange_1.1.0_x64-setup.exe   # Windows 安装包（版本号与清单一致）
└─ simx-server           # Linux 后端二进制（可选）
```

`version.json` 内容示例：

```json
{
    "version": "1.1.0",
    "notes": ["新增 XX 功能", "修复 XX 问题"],
    "winInstaller": "SimuXchange_1.1.0_x64-setup.exe",
    "linuxServer": "simx-server"
}
```

**独立后端**：启动时加 `--update-url <地址>` 参数，启动过程会拉取同一份
`version.json` 并打印“发现新版本/已是最新版本”提示（后端长时间驻留，不自动
替换运行中的二进制，请按提示手动部署新版）。

检查失败（服务器不可达、清单无效等）**静默忽略**，不影响正常启动。

## 使用流程

1. 启动桌面端 → 左侧「新建」交易网关，选择**网关分类**（深圳统一网关 / 上海竞价网关，创建后不可改）
2. 选中网关 →「添加平台」，配置监听端口与模拟回报策略（平台类型选项随网关分类变化）
3. 「启动网关」→ 平台开始监听柜台 TCP 连接
4. 柜台按对应交易所 Binary 协议连接并登录（Logon），网关回登录应答与平台信息/状态
5. 柜台发送现货新订单 → 网关按策略回确认 / 成交 / 拒单
6. 界面实时展示连接数、委托/确认/成交/拒单统计与运行日志
7. 顶栏 ◐ 按钮可切换五套界面配色（经典黑/现代蓝/深空灰/极光紫/优雅白），选择自动保存

## 模拟回报策略

| 模式 | 行为 |
| --- | --- |
| 全部成交（单笔） | 1 条确认 + 1 条全量成交 |
| 全部成交（多笔拆单） | 1 条确认 + N 条成交，数量随机拆分，价格按档位买单递增/卖单递减（不超过委托价） |
| 部分成交（单笔） | 1 条确认 + 1 条随机数量的部分成交 |
| 部分成交（多笔拆单） | 1 条确认 + N 条部分成交 |
| 自定义成交 | 1 条确认 + 按配置的数量/价格逐笔成交 |
| 不成交挂单 | 只回确认 |
| 拒单 | 回 200102(ExecType=8) 或业务拒绝消息(MsgType=4)，原因码/文本可配置 |

延迟配置：最小 = 最大为固定延迟；范围随机；全为 0 时在收单流程内同步回报，否则异步发送。

## 协议实现范围

### 深圳统一网关（深交所 Binary）

- 会话层：Logon(1)、Logout(2)、Heartbeat(3)、平台状态(6)、平台信息(9)，校验和校验、心跳超时检测
- 业务层：新订单 100101、确认回报 200102、成交回报 200115、业务拒绝(4)、撤单 190007 → 撤单失败 290008
- 编码规则：大端序、定长字符串补空格、Price N13(4)、Qty N15(2)；报文头 MsgType(4)+BodyLength(4)

### 上海竞价网关（上交所竞价平台 Binary 0.54）

- 会话层：Logon(40)、Logout(41)、Heartbeat(33)、平台状态(209)、执行报告信息(208)、分区序号同步(206/207)，校验和校验、心跳区间[5,60]
- 业务层：新订单 58、申报响应/执行报告 32、成交回报 103、订单拒绝 204、撤单 61 → 撤单失败 59
- 编码规则：大端序、报文头 MsgType(4)+MsgSeqNum(8)+MsgBodyLen(4) 共 16 字节（比深交所多了消息序号），Price N13(5) = 价格×100000、Qty N15(3) = 股数×1000；**登录后需先完成分区序号同步(206/207)才会推送执行报告**

## 测试

```powershell
cargo test -p simx-core        # 68 个单元测试（64 + 4 自动启动）+ 11 个端到端集成测试（深交所 3 + 上交所竞价 4 + 上交所新债券 4）
```

端到端测试覆盖：TCP 连接 → Logon 握手 →（上交所还含分区序号同步）→ 委托 → 回报字段/校验和验证 → 网关停止 Logout。
