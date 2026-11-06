# CLI 命令行工具使用手册（simx-cli）

> 本文是 `simx-cli` 的独立使用文档：工具定位、命令用法、输出格式、
> 自动化测试示例。与《产品使用手册》第 8 章的关系：那里是速查，
> 这里讲全。

---

## 1. 工具定位

`simx-cli` 是模拟交易所后端的**命令行控制工具**，专为自动化测试
（回归、CI、定时批量跑用例）设计：把界面上"查状态 / 改策略 / 手动回报"
等手工操作做成一条条子命令，一次调用只做一件事，用退出码和 JSON 反馈
结果。

| 界面操作 | CLI 等价命令 |
| --- | --- |
| 看网关/平台状态与统计 | `simx-cli snapshot` |
| 编辑平台 → 回报策略（热更新） | `simx-cli set-strategy --mode ackOnly ...` |
| 订单弹窗手动回一笔成交 | `simx-cli send --order CL0001234 trade --qty 500 --price 10.0` |
| 手动撤单成功 | `simx-cli send --order CL0001234 cancel` |
| 启动 / 停止网关 | `simx-cli start --gateway G` / `simx-cli stop --gateway G` |

**它不监听任何端口**：只作为客户端连到已启动的 simx-server（WebSocket
控制口），发一条命令、收一个结果、按退出码结束。**引擎、simx-server、
前端均无改动**——CLI 只是现行控制协议的又一个客户端，界面能做的控制
操作它都能做。

### 支持与不支持

- **支持**：查询（快照 / 订单 / 探活）、回报策略热更新（含 Lua 脚本）、
  手动回报（确认 / 成交 / 拒单 / 撤单）、网关启动 / 停止、统计清零、
  Lua 语法校验；
- **不支持**：新建 / 编辑网关、增删平台等配置结构类操作——控制协议里
  没有对应命令（这类操作仍用界面做，做完后 CLI 直接复用配置继续测试）。

> 构建 / 安装方式见根目录 README 的「CLI 命令行控制工具」一节；
> 本文只讨论工具本身的说明与用法。

---

## 2. 连接服务器：登录一次，之后免地址

CLI 每次调用是新进程，本身不持有状态。所以引入"登录"概念：**登录 =
指定本次连哪台服务器并把地址保存下来**，之后所有命令都按保存的地址
连接，无需再带 `--url`，直到重新登录（换服务器）或登出（清除）。

```text
simx-cli login --url ws://192.168.1.10:9800/ws    # 连一次并保存地址
simx-cli health                                    # 之后全部免 --url
```

地址生效顺序（每次命令都按这个找）：

1. 命令行显式写了 `--url <地址>` → **仅本次生效**（不改变保存值）；
2. 否则用 `login` 保存的地址；
3. 从未登录过 → 用默认 `ws://127.0.0.1:9800/ws`（本机）。

```text
simx-cli logout                                     # 清除保存值，回默认本机
```

地址保存在**用户主目录**：

| 平台 | 文件 |
| --- | --- |
| Windows | `%USERPROFILE%\.simx-cli.json` |
| Linux / macOS | `~/.simx-cli.json` |

内容是一行 JSON：`{"url": "ws://192.168.1.10:9800/ws"}`，可以手动查看、
编辑或删除（等价 logout）。

特别注意：

- `login` 会**先探活再保存**：地址连不上（服务器未启动 / 端口错 / 被
  防火墙拦截）时直接报错、退出码 1、**不写配置文件**——避免把坏地址
  固化成默认；
- simx-server **无鉴权**；CLI 默认连本机 `127.0.0.1`，远程使用务必自行
  控制网络暴露（防火墙只放行测试网段）；
- `login` 的 `--url` 是必填参数。

---

## 3. 全局参数与退出码

```text
simx-cli [全局参数] <子命令> [子命令参数]
```

| 全局参数 | 说明 |
| --- | --- |
| `--url <地址>` | 本次连接的服务器地址（覆盖 login 保存值，仅本次有效） |
| `--json` | 原样输出后端 JSON 响应（脚本解析用），默认人类可读 |
| `--timeout <毫秒>` | 单次请求超时，默认 10000 |

三个全局参数**放在子命令前面或后面都合法**（clap 的 global 约定），
以下两句等价：

```text
simx-cli --json snapshot
simx-cli snapshot --json
```

另有 clap 内置的 `-h/--help` 与 `-V/--version`：`simx-cli --version` 打印
版本号（如 `simx-cli 2.0.0`），`simx-cli --help` 打印全部子命令与参数说明；
两者不连服务器、直接退出。

退出码（脚本据此判断成败）：

| 退出码 | 含义 |
| --- | --- |
| 0 | 命令成功（后端 `ok:true`） |
| 1 | 后端返回错误 / 连接失败 / 超时 |
| 2 | 命令行参数错误（未知子命令、非法模式名、缺参数等） |

失败时错误文本写到 stderr；**`--json` 模式失败也保证 stdout 上有一行
`{"ok":false,"error":"..."}`**，脚本总能从 stdout 解析到 JSON。

---

## 4. 子命令详解

所有子命令里的平台参数（`--gateway` / `--platform`）都可填 **id 或
名称**（CLI 自动解析成后端认的 id——后端命令只认 id，名称只是给人看的）。

**关于重名**：网关名、平台名在界面上允许重复（并不强制唯一）。CLI 的
匹配规则是 **id 优先**——id（快照输出的第一列括号内）全局唯一，恒可
精确定位；按名称匹配时若命中多个，CLI **报错并列出全部候选 id**，
提示改用 id，绝不会默默随机取第一个。

### 4.1 health —— 探活

```text
simx-cli health
```

只验证"CLI → server → 引擎"链路通（实际内部发一条快照请求）；成功时
打印 `ok（已连接 ws://...）`，失败时打印连接错误、退出码 1。

### 4.2 snapshot —— 查网关/平台状态

```text
simx-cli snapshot                        # 全部网关
simx-cli snapshot --gateway 竞价网关A     # 只看某个网关
```

人类可读输出：网关名称/状态（运行中 / 已停止）、platform 的 id、监听
端口、当前连接数、订单统计（委托 / 确认 / 成交 / 拒单）；`--json` 输出
全量结构化数据（`dirs` + `gateways[]`，字段与 `types.ts` 对应）。

### 4.3 orders —— 查订单明细

```text
simx-cli orders --gateway G --platform P [--status 状态[,状态...]] [--limit N]
```

- `--status`：可选 `new`（已报）/ `partial`（部分成交）/ `filled`（已成交）/
  `cancelled`（已撤单）/ `rejected`（已拒单），可逗号分隔多个；缺省全部；
- `--limit`：最多显示行数（0 = 不限制）；
- 输出列：委托编号（clOrdId）、交易所单号、方向、价格、数量/剩余、状态、
  时间、连接 id。

手动回报前，先在这里查委托编号（第一列）。

### 4.4 set-strategy —— 修改自动回报策略（核心）

**等价界面"编辑平台 → 策略"**：走同一热更新通道，**运行中实时生效并
持久化**（写入 gateways.json）。**未传的参数保持服务器当前值**——实现上
是"先取快照做基底 → 命令行参数覆盖 → 整体回传"，所以只改一个字段也
安全，不会被默认值冲掉。

```text
simx-cli set-strategy --gateway G --platform P [参数覆盖...]
```

可用参数：

| 参数 | 取值/格式 | 说明 |
| --- | --- | --- |
| `--mode` | `fullSingle` / `fullSplit` / `partialSingle` / `partialSplit` / `noAutoReply` / `ackOnly` / `reject` / `luaScript` | 8 种模式，与界面一致：全部成交（单/多笔）、部分成交（单/多笔）、关闭自动回报、只回确认、拒单、Lua 脚本回报 |
| `--split-min` / `--split-max` | 整数 | 拆单成交的笔数范围（拆单模式下生效） |
| `--price-tick` | 小数 | 拆单时相邻两笔的递价（买递增/卖递减，不穿委托价） |
| `--ack-delay` / `--trade-delay` / `--cancel-delay` | `N` 或 `min,max` | 对应回报的延迟毫秒：单值=固定，两值=区间随机；0 同步 |
| `--reject-via` | `executionReport` / `businessReject` | 拒单形式：执行报告拒绝 / 业务拒绝 |
| `--reject-reason` / `--reject-text` | 数字 / 文本 | 拒单原因码与说明 |
| `--cancel-mode` | `default` / `alwaysSuccess` / `alwaysReject` | 撤单回报策略 |
| `--cancel-reject-reason` | 数字 | 撤单拒单原因码 |
| `--cancel-front-reject` | 开关 | 撤单拒单改发业务拒绝消息（深 4 / 沪 204） |
| `--lua-script` | 脚本文本或 `@文件` | Lua 脚本内容（长脚本放文件，`@` 前缀从文件读） |
| `--strategy-json` | JSON 文本或 `@文件` | 用整个策略对象整体替换（字段 camelCase、`mode` 必填），随后可再叠加命令行覆盖 |

**Lua 模式**：`--mode luaScript` 时必须给脚本；CLI 保存前先做
`validate_lua_script` 语法校验（后端保存时同样会校验，CLI 提前报更清晰），
不过则直接失败不落盘。Lua 模式下的回报延迟只由脚本里 `delayMs` 控制，
`--ack-delay` / `--trade-delay` 自动归零（与界面行为一致）。

示例：

```text
# 只改一个字段：切到"只回确认"
simx-cli set-strategy --gateway 竞价网关A --platform 平台1 --mode ackOnly

# 拆单成交（2~5 笔随机）+ 确认固定 10ms / 成交 50~150ms 区间随机
simx-cli set-strategy --gateway G --platform P --mode fullSplit --split-min 2 --split-max 5 --ack-delay 10 --trade-delay 50,150

# 关闭自动回报（之后全部用 send 手动发）
simx-cli set-strategy --gateway G --platform P --mode noAutoReply

# Lua 脚本从文件装载
simx-cli set-strategy --gateway G --platform P --mode luaScript --lua-script @trade.lua
```

`--json` 时输出后端完整响应（含新策略对象，方便脚本断言字段）。

### 4.5 send —— 手动回报一笔订单

**等价界面"订单弹窗 → 回复"**：委托必须在订单缓存、处于在途（orders
里能看到），回报从**当前活动柜台连接**发出。

```text
simx-cli send --gateway G --platform P --order <委托编号> <kind> [--qty N] [--price X] [--reason N] [--front-reject]
```

- `<kind>` 是位置参数（命令末尾）：`ack` 确认 / `trade` 成交 / `reject`
  拒单 / `cancel` 撤单成功；
- `trade` 不传 `--qty` / `--price` → 按剩余量全成、按委托价成交（与界面
  一致）；
- `reject` 不传 `--reason` → 按网关分类取默认原因码（深圳 20009 /
  上海 1025）；`--front-reject` 改发业务拒绝消息；
- 订单已终态（成交/拒单/撤单后）再回报会被后端拒绝——这正是自动化测试
  要验证的行为之一，CLI 报错、退出码 1。

```bash
# 手动回 500 股@ 10.0 的成交
simx-cli send --gateway G --platform P --order CL0001234 trade --qty 500 --price 10.0

# 全部成交（不传 qty/price，按剩余量全成）
simx-cli send --gateway G --platform P --order CL0001234 trade

# 手动拒单（默认原因码）
simx-cli send --gateway G --platform P --order CL0001234 reject
```

### 4.6 start / stop / reset-stats —— 网关管理

```text
simx-cli start        --gateway <G>       # 启动网关
simx-cli stop         --gateway <G>       # 停止网关
simx-cli reset-stats  --gateway <G>       # 统计清零（等价界面"重置统计"）
```

三者返回人类可读的成功文案；网关不存在 / 状态与操作冲突时报错、退出码 1。
CLI 不提供创建/删除网关——保持界面唯一入口，避免误操作。

### 4.7 validate-lua —— 只校验 Lua 语法

```text
simx-cli validate-lua "return {...}"      # 内联脚本
simx-cli validate-lua @trade.lua          # 从文件读
```

用 lua_engine 编译一次（不执行、不保存），通过打印"脚本语法正确"，失败
打印具体错误并退出码 1。适合把脚本入库前自检。

### 4.8 login / logout —— 地址持久化

见第 2 节。`login --url` 必填、先探活再保存（连不上不写文件）；`logout`
清除保存值回默认本机。

---

## 5. 输出与脚本解析

成功时默认人类可读（中文）；`--json` 时原样输出后端 JSON。用 jq 解析：

```bash
# 找某网关是否在运行：
simx-cli --json snapshot | jq -e '.data.gateways[] | select(.config.name=="竞价网关A") | .state' >/dev/null && echo RUNNING

# 取某委托状态：
simx-cli --json orders --gateway 1 --platform 1 | jq -r '.data[] | select(.cl_ord_id=="1001234") | .status'
```

失败时人类模式错误到 stderr、JSON 模式仍打 stdout 一行
`{"ok":false,"error":"..."}`。

---

## 6. 自动化测试场景示例

### 6.1 PowerShell 完整用例（现场测试）

```powershell
$cli = ".\simx-cli.exe"
& $cli login --url ws://192.168.1.10:9800/ws
if ($LASTEXITCODE -ne 0) { exit 1 }

# 1. 探活 + 确认网关已启动
& $cli health
& $cli snapshot --gateway 竞价网关A

# 2. 切换策略：只回确认 + 80ms 固定延迟（热更新即时生效）
& $cli set-strategy --gateway 竞价网关A --platform 平台1 --mode ackOnly --ack-delay 80
if ($LASTEXITCODE -ne 0) { exit 1 }

# 3. （柜台侧）下单后取委托编号
Start-Sleep -Seconds 2
$order = (& $cli orders --gateway 竞价网关A --platform 平台1 --status new) -split '\s+' | Select-Object -First 1

# 4. 手动回一笔 500 股 @ 10.0 的成交
& $cli send --gateway 竞价网关A --platform 平台1 --order $order trade --qty 500 --price 10.0
if ($LASTEXITCODE -ne 0) { exit 1 }

# 5. 断言订单已成交
& $cli orders --gateway 竞价网关A --platform 平台1 --status filled | Select-String $order
if ($LASTEXITCODE -ne 0) { Write-Error "用例失败"; exit 1 }
Write-Host "用例通过"
```

### 6.2 bash + jq（CI 流水线）

```bash
#!/usr/bin/env bash
CLI=./simx-cli
set -e

$CLI login --url ws://192.168.1.10:9800/ws

$CLI set-strategy --gateway 竞价网关A --platform 平台1 \
    --mode fullSplit --split-min 2 --split-max 5 --trade-delay 100,200

# （柜台下单后）
oid=$($CLI --json orders --gateway 竞价网关A --platform 平台1 --status new \
    | jq -er '.data[0].cl_ord_id')
$CLI send --gateway 竞价网关A --platform 平台1 --order "$oid" trade

$CLI orders --gateway 竞价网关A --platform 平台1 --status filled | grep -q "$oid"
echo "PASS"
```

### 6.3 常用组合速查

| 目标 | 命令片段 |
| --- | --- |
| 全部成交（单笔） | `--mode fullSingle` |
| 全部成交（随机 2~5 笔） | `--mode fullSplit --split-min 2 --split-max 5` |
| 只回确认不成交 | `--mode ackOnly` |
| 拒单 | `--mode reject --reject-via executionReport --reject-reason 默认` |
| 关闭自动回报 | `--mode noAutoReply` |
| Lua 回报 | `--mode luaScript --lua-script @my.lua` |
| 撤单一律拒绝 | `--cancel-mode alwaysReject --cancel-reject-reason 1000` |

---

## 7. 常见问题（FAQ）

**Q1：连不上，报 Connection refused？**
看 server 是否启动、端口对不对（默认 9800）、防火墙是否放行；地址末尾
的 `/ws` 不能丢；`health` 先在本机自验。

**Q2：报"未找到网关 xxx"？**
snapshot 里的网关名称与你输入不一致（大小写 / 空格）。用 `--json` 看
真实 `config.name`，或直接用快照显示的 id。

**Q3：send 报"order not found"？**
- 下单后最多 1 秒才进缓存：先 `orders` 查；
- 订单已终态不可重复回报；
- 当前没有活动柜台连接时回报发出被拒。

**Q4：命令变慢/超时？**
加大 `--timeout 30000`（放全局位置）。

**Q5：为什么 --json 字段是 camelCase（小驼峰，如 clOrdId）？**
后端 serde 序列化约定 camelCase，字段与 `src/types.ts` 对应；脚本照此解析。

**Q6：怎么把默认地址改回本机？**
`logout`，或直接删 `%USERPROFILE%\.simx-cli.json`。

**Q7：网关 / 平台名称重复了怎么办？**
网关名、平台名在界面上允许重复。CLI 按名称匹配到多个时会**报错并列出
候选的 id**（绝不默默取第一个），你改用 id 精确定位即可——id 在
`snapshot` 输出第一列的括号里，全局唯一。

---

## 8. 与内部模块的对应（维护者速查）

| CLI 子命令 | 后端命令（api.rs cmd） | 说明 |
| --- | --- | --- |
| health / snapshot | `get_snapshot` | 探活=快照 |
| orders | `get_orders` | |
| set-strategy | `update_strategy`（先取基底） | 热更新+持久化 |
| send | `send_report` | 手动回报 |
| start | `start_gateway` | |
| stop | `stop_gateway` | |
| reset-stats | `reset_stats` | |
| validate-lua | `validate_lua_script` | |
| login / logout | （无后端命令） | 本地持久化地址 |

后端 `dispatch` 加新命令后，CLI 只需加一个转发子命令即可接入。