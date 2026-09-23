# Lua 脚本回报编写说明

> 适用版本：SimuXchange 1.1.0 及以上（深交所统一网关 / 上海竞价网关 / 上海新债券网关均支持）。
> 配套界面：平台编辑弹窗 → 「报单自动回报模式」→ **Lua 脚本回报**。

---

## 1. 这是什么

「Lua 脚本回报」是第 8 种报单自动回报模式。与固定模式（全部成交 / 部分成交 / 自动拒单……）不同，
它把"收到新委托后回什么回报"完全交给一段你编写的 **Lua 脚本**：

- 平台每收到一笔**新委托**，就执行一次脚本；
- 脚本根据委托内容（数量、价格、证券代码、买卖方向等）**返回回报计划**；
- 引擎按计划逐条生成并发送确认回报 / 成交回报 / 拒单。

固定模式还能用的功能在脚本模式下全部照常：

| 功能 | 脚本模式下是否生效 |
| --- | --- |
| 撤单自动回报模式（含前台拒单、撤单延迟） | ✅ 照常，脚本**不**接管撤单 |
| 回报延迟（确认/成交延迟配置） | ❌ 不参与（延迟只能由脚本各项 `delayMs` 控制，界面不提供确认/成交延迟输入，保存时自动归零；撤单延迟除外，见上一行） |
| 订单缓存 / 订单列表 / 界面手动回复 | ✅ 照常 |
| 收发报文展示与持久化、回报同步(5/206)重发 | ✅ 照常 |
| 策略热更新（网关运行中修改脚本） | ✅ 保存后对下一笔委托即时生效 |

## 2. 快速上手

编辑平台 → 自动回报模式选 **Lua 脚本配置** → 在输入框里写脚本 → 点「检查脚本」即时校验语法 → 保存。

只有一个输入框（原“AI 生成脚本”输入框已合并）：默认放一段**需求示例文字**，保持默认
点「AI 生成脚本」即可让大模型把它变成真正的 Lua 脚本（您的原话会自动转为脚本头部
注释；模型判定需求不支持时会说明原因，描述不清或逻辑矛盾时会追问并指出问题点）；
生成后在框内继续手动调整，想换逻辑直接改写框内文字再点「AI 生成脚本」。保存时若框内
还是默认示例提示词会被拦截，提示先生成或直接写好脚本。

点「AI 生成脚本」前会先做语法校验：框内**已经是合法 Lua 脚本**时（粘贴的或刚生成的）
直接提示"已是合法 Lua 脚本"，**不再调用大模型**——只有输入需求描述文字才会触发生成，
因此后续手动修改过的脚本也请直接在框内编辑，不要把它当需求再交回 AI。

点「脚本手册」可随时查看本文档全文（随程序内嵌）。

下面这个"按委托数量分档"的脚本逻辑就是默认提示词对应的完整示例（深圳平台）：

```lua
if ord.OrderQty > 1000 then
    return "reject"                       -- 数量超 1000：拒单
elseif ord.OrderQty > 800 then
    return "confirm"                      -- 800~1000：只回确认
elseif ord.OrderQty > 500 then
    -- 500~800：确认 + 按一半数量部分成交
    return { { kind = "confirm" }, { kind = "trade", qty = math.floor(ord.OrderQty / 2) } }
else
    return "trade"                        -- 500 以内：确认 + 全部成交
end
```

三行规则总结：

1. **输入**：脚本里直接用全局变量 `ord`（委托字段）和 `ctx`（环境信息）；
2. **输出**：`return` 一个字符串（快捷方式）或一张表（精确控制），见第 5 节；
3. **脚本就是函数体**：平台自动包装执行，不需要自己定义函数，报错行号与编辑器行号一致。

## 3. 输入变量 `ord`（委托字段）

字段名与报文解析弹窗里显示的**交易所规范字段名一致**。价格/数量已换算成自然单位：
**价格 = 元（可带小数），数量 = 股**（协议里的放大整数无需关心）。

### 3.1 深交所统一网关（sz）

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `ord.MsgType` | number | 新订单消息类型（如 100101 现货竞价、100401 期权） |
| `ord.ApplID` | string | 应用标识（`"010"` 现货竞价等） |
| `ord.ClOrdID` | string | 客户订单编号（订单唯一键） |
| `ord.SecurityID` | string | 证券代码 |
| `ord.SecurityIDSource` | string | 证券代码源（深市 `"102"`） |
| `ord.PBU` | string | 申报交易单元 |
| `ord.Side` | string | 买卖方向：`"1"` 买、`"2"` 卖（G 借入 / F 出借 / D 申购 / E 赎回） |
| `ord.OrdType` | string | 订单类别：`"1"` 市价、`"2"` 限价、`"U"` 本方最优 |
| `ord.OrderQty` | number | 委托数量（**股**） |
| `ord.Price` | number | 委托价格（**元**） |
| `ord.AccountID` | string | 证券账户 |
| `ord.BranchID` | string | 营业部代码 |
| `ord.OwnerType` | number | 订单所有者类型（1 个人 / 102 会员 / 103 机构…） |
| `ord.ClearingFirm` | string | 结算机构代码 |
| `ord.TransactTime` | number | 委托时间 YYYYMMDDHHMMSSsss |
| `ord.UserInfo` | string | 用户私有信息 |
| `ord.OrderRestrictions` | string | 订单限定（如 `"1"` 程序化交易） |
| `ord.StopPx` | number | 止损价（元，未填为 0） |
| `ord.MinQty` | number | 最低成交数量（股，未填为 0） |
| `ord.MaxPriceLevels` | number | 最多成交价位数 |
| `ord.TimeInForce` | number | 有效时间类型（0 当日 / 3 IOC） |
| `ord.CashMargin` | number | 信用标识 |
| `ord.LowLimitPrice` / `ord.HighLimitPrice` | number | 价格下限 / 上限（元，仅部分业务下发） |
| `ord.CashOrderQty` | number | 申购金额（元，仅 LOF 申赎业务） |
| `ord.LotType` | string | 订单数量类型（仅港股通） |

### 3.2 上海竞价网关（shjj）与上海新债券网关（shbond）

两套协议字段完全一致（新债券无扩展字段三列）：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `ord.BizID` | number | 业务标识（竞价如 100010 现货竞价；债券 **1** 现券竞价 / **2** 质押式回购） |
| `ord.ClOrdID` | string | 客户订单编号 |
| `ord.SecurityID` | string | 证券代码 |
| `ord.Account` | string | 证券账户 |
| `ord.PBU` | string | 交易单元 |
| `ord.Side` | string | 买卖方向：`"1"` 买、`"2"` 卖 |
| `ord.OrdType` | string | 订单类别：竞价 `"1"` 市转撤 / `"2"` 限价 / `"3"` 市转限 / `"4"` 本方最优 / `"5"` 对手方最优；债券平台仅 `"2"` 限价 |
| `ord.OrderQty` | number | 委托数量（**股**） |
| `ord.Price` | number | 委托价格（**元**） |
| `ord.OwnerType` | number | 订单所有者类型 |
| `ord.TimeInForce` | number | 有效时间类型 |
| `ord.TransactTime` | number | 委托时间 HHMMSSsssnnnn |
| `ord.CreditTag` | string | 信用标签 |
| `ord.ClearingFirm` | string | 结算机构 |
| `ord.BranchID` | string | 营业部代码 |
| `ord.UserInfo` | string | 用户私有信息 |
| `ord.Custodian` / `ord.DividendSelect` / `ord.DestSecurity` | string | 扩展字段（仅转托管/分红/转换类业务） |

> 深市 `Side` 判断示例：`if ord.Side == "1" then`（买）。

## 4. 输入变量 `ctx`（环境信息）

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `ctx.Category` | string | 网关分类：`"sz"` / `"shjj"` / `"shbond"` |
| `ctx.Gateway` | string | 网关名称 |
| `ctx.Platform` | string | 平台名称 |
| `ctx.PartitionNo` | number | 本笔委托命中的分区号（按证券代码哈希分配） |
| `ctx.PeerAddr` | string | 柜台地址（`ip:port`） |

## 5. 返回值（回报计划）

### 5.1 字符串快捷方式（常用剧本一行搞定）

| 返回值 | 含义 |
| --- | --- |
| `return "confirm"` | 只回 **1 条确认回报**，订单挂在"已报"状态 |
| `return "trade"` | **确认 + 全部成交**（等价固定模式"全部成交"） |
| `return "reject"` | **拒单**（方式/原因码/文本取平台"拒单参数"配置） |
| `return`（或不写 return） | **不回任何回报**（订单仍进订单缓存，可手动回复） |

### 5.2 表形式（精确逐条控制）

`return` 一张回报计划表；数组里每个元素是一条回报，**按顺序逐条发出**。
单张回报表（带 `kind` 字段）会自动被视为只有一项。

```lua
-- 确认 + 两笔成交（拆单）：三连发
return {
    { kind = "confirm" },
    { kind = "trade", qty = 300, price = 10.50, delayMs = 100 },
    { kind = "trade" },   -- 缺省 qty = 剩余全部
}
```

**每条回报的字段**（`kind` 决定是哪种回报，其余字段均有缺省）：

| 字段 | 适用 kind | 类型 | 缺省行为 |
| --- | --- | --- | --- |
| `kind` | 全部 | string | 必填：`"confirm"` / `"trade"` / `"reject"` |
| `delayMs` | 全部 | number | 距上一条回报的等待毫秒；缺省 0 同步发出（界面"回报延迟"配置不参与 Lua 模式，延迟只能由本字段控制） |
| `qty` | trade | number | 本笔成交数量（**股**，向下取整）；缺省 = 剩余全部数量 |
| `price` | trade | number | 成交价格（**元**）；缺省 = 委托价格 |
| `via` | reject | string | `"execution"` 执行报告拒绝（深 200102 ExecType=8 / 沪 32 ExecType=8，缺省）；`"business"` 业务拒绝（深 MsgType=4 / 沪 204） |
| `reason` | reject | number | 拒单原因码；缺省取"拒单原因码"配置 |
| `text` | reject | string | 拒单说明文本（仅深市业务拒绝携带）；缺省取"拒单说明文本"配置 |

**缺省规则速查**：

- `return "reject"`（不带参数）= 界面「自动拒单」模式的完整行为（方式 + 原因码 + 文本）；
- 成交数量**不会自动截断**：脚本给多少就报多少，多笔成交按顺序累计（`cum`）推进，
  最后一笔累计 ≥ 委托量时订单转"全部成交"，不足则保持"部分成交"在途——
  可以故意构造 cum > 委托量等异常场景测试柜台容错；
- 无成交回报的业务（深市表 3-4 除现货竞价外的多数业务、沪市除 100010/300020/300021 外），
  脚本返回的 `trade` 项会被**忽略并写一条警告日志**；
- `return nil`、`return {}` 或不写 return = 不回报（等同"关闭自动回报"但订单照常入缓存）。

### 5.3 校验与报错

- **保存时**：后端强制校验（脚本非空 + 语法编译通过），失败则保存/热更新整体报错并弹窗提示行号；
- **编辑中**：点「检查脚本」按钮即时校验语法（不保存）；
- **运行时**：语法正确但执行出错（如对 nil 取索引、死循环被强制终止）时，
  **本笔委托不自动回报**，错误详情写入日志面板（前缀 "Lua 回报脚本执行失败"）。

## 6. 沙箱与限制

脚本是随手写的测试逻辑，引擎做了沙箱化处理，**写坏也不会影响模拟器本身**：

- 可用标准库：`table`、`string`、`math`（`math.random` / `math.floor` / `math.max`…），
  以及部分基础函数（`pairs` / `ipairs` / `tostring` / `tonumber` / `pcall` / `select` 等）；
- 不可用：`io`（文件）、`os`（系统命令/时间）、`require`（模块加载）、`load` / `dofile`、`print`；
- 脚本顶层定义的局部变量/函数会在**同一平台的委托之间保留**（可用来做计数器、状态机），
  修改脚本保存后重新编译、状态清零；
- 单笔委托脚本执行有**指令数上限**（约千万条指令量级，正常脚本远用不到），
  超限按运行时错误处理（本笔不回报 + 日志提示"疑似死循环"）；
- 有内存上限（32MB），超限同样按运行时错误处理。

## 7. 完整示例

### 例 1：按委托数量分档（默认模板）

见第 2 节。

### 例 2：随机拆多笔成交（自己控制拆单节奏）

```lua
-- 任意数量：确认后随机拆 2~4 笔成交完
local total = math.floor(ord.OrderQty)
local n = math.random(2, 4)
local plan = { { kind = "confirm" } }
local filled = 0
for i = 1, n - 1 do
    local q = math.floor(total / n)
    filled = filled + q
    table.insert(plan, { kind = "trade", qty = q, delayMs = math.random(50, 200) })
end
table.insert(plan, { kind = "trade" })  -- 最后一笔吃掉剩余
return plan
```

### 例 3：按证券代码拒绝（黑名单）

```lua
local blacklist = { ["000001"] = true, ["600519"] = true }
if blacklist[ord.SecurityID] then
    return { kind = "reject", via = "business", reason = 20009, text = "证券在测试黑名单中" }
end
return "trade"
```

### 例 4：价格超限拒单（用委托里下发的涨跌幅上下限）

```lua
-- 仅当委托携带了限价上下限时检查（深市部分业务下发）
if ord.HighLimitPrice > 0 and ord.Price > ord.HighLimitPrice then
    return { kind = "reject", reason = 20009 }
end
if ord.LowLimitPrice > 0 and ord.Price < ord.LowLimitPrice then
    return { kind = "reject", reason = 20010 }
end
return "trade"
```

### 例 5：市价单只确认、限价单成交

```lua
if ord.OrdType == "1" then
    return "confirm"          -- 市价单只挂单
elseif ord.Side == "1" then
    return "trade"            -- 买单全部成交
else
    return { { kind = "confirm" }, { kind = "trade", qty = math.floor(ord.OrderQty / 2) } }
end
```

### 例 6：部分成交挂单（剩余在途，供撤单/手动回复测试）

```lua
-- 固定成交 1/3，剩余保持"部分成交"在途
return {
    { kind = "confirm" },
    { kind = "trade", qty = math.floor(ord.OrderQty / 3) },
}
```

### 例 7：模拟真实节奏（确认快、成交慢）

```lua
return {
    { kind = "confirm", delayMs = 20 },      -- 确认 20ms
    { kind = "trade",   delayMs = 500 },     -- 500ms 后全部成交
}
```

### 例 8：每 5 笔拒 1 笔（顶层计数器在委托间保留）

```lua
counter = (counter or 0) + 1
if counter % 5 == 0 then
    return { kind = "reject", reason = 20011 }
end
return "trade"
```

### 例 9：按柜台来源区分行为

```lua
-- 多台柜台连不同平台时，可用柜台 IP/平台信息区分剧本
if string.find(ctx.PeerAddr, "^192%.168%.1%.", 1) then
    return "trade"
end
return "confirm"
```

## 8. 常见问题

**Q：脚本会接管撤单吗？**
不会。撤单仍按界面「撤单自动回报模式」配置执行；脚本只在收到**新委托**时运行。

**Q：为什么我的成交回报没有发出来？**
该业务无成交回报（如深市 ETF 申赎、沪市多数非竞价业务），脚本返回的 `trade` 项被忽略并写警告日志；
另外成交回报发出前会检查订单状态——订单已在延迟期间到达终态（如已拒单）的回报会被丢弃。

**Q：回报延迟怎么算？**
每条回报的 `delayMs` 是**相对上一条回报**的等待时间；未写该字段的项同步发出
（0 延迟）。界面"回报延迟"配置里的**确认/成交延迟在 Lua 模式下不生效**——
延迟只能由脚本的 `delayMs` 控制（Lua 模式界面隐藏这两个输入框、保存时自动归零）；
撤单延迟不受影响（脚本不接管撤单）。全部为 0 时严格保序同步发送。

**Q：脚本报错第 N 行，但我数着不对？**
错误行号与编辑器中的脚本正文一致（平台不做任何包装/拼接）。若对空值比较，注意
Lua 中 `nil` 与数字比较会直接报错，可先判空：`if ord.Price and ord.Price > 10 then`。

**Q：保存时报"脚本内容为空"？**
Lua 脚本模式要求脚本非空；新建/切换到该模式时会自动预填示例模板。

**Q：怎么验证脚本逻辑？**
修改脚本（网关运行中也可保存，即时生效）→ 柜台发一笔测试委托 → 看平台日志面板
的"发送 确认回报(...)（Lua）"字样与订单列表状态；报文弹窗可逐字段核对。

---

*本文档对应源码：`crates/simx-core/src/lua_engine.rs`（执行器）与三套协议
`strategy.rs` 的 `plan_reports_lua`（回报计划落地）；发现文档与实现不符请提 issue。*
