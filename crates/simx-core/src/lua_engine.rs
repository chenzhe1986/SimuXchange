//! lua_engine —— "Lua 脚本回报"模式的脚本执行器（三套协议共用）。
//!
//! # 职责
//! 用户在平台配置里选择"Lua 脚本回报"模式并编写一段 Lua 脚本，收到新委托时
//! 各协议的 strategy 模块把委托字段装进 `ord` 表（协议字段不同，装表各自实现）、
//! 把环境信息装进 `ctx` 表（本模块统一实现），然后调用本模块执行脚本并把
//! 脚本 `return` 的"回报计划"解析成 [`ScriptPlan`] 列表，最后由协议侧
//! 转成 `PlannedReport`（复用既有的确认/成交/拒单报文构造器）。
//!
//! # 脚本形式（直接脚本体）
//! 用户写的脚本**就是函数体**：引擎把脚本源码原样编译成一个 Lua 函数，
//! 每笔委托调用一次，调用前把 `ord` / `ctx` 设为全局变量。这样脚本里
//! 直接写 `if ord.OrderQty > 1000 then return "reject" end`，无需定义函数，
//! 报错行号也与输入框里的行号一致。
//!
//! # 沙箱与安全
//! 脚本是用户随手写的测试逻辑，必须假设它可能出错甚至写坏：
//! - 只加载 table/string/math/base 标准库（无 io/os/require），
//!   并把 base 里的 `load`/`dofile`（运行时加载任意代码/读文件）、
//!   `print`（污染后端进程 stdout）、`collectgarbage` 置空；
//! - 内存上限（[`MEMORY_LIMIT`]）：Lua 状态分配超过即报错；
//! - 指令数上限（[`MAX_INSTRUCTIONS`]）：通过 debug hook 每隔一段指令检查
//!   计数器，超限报错——防止 `while true do end` 这类死循环挂死连接任务。
//!
//! # 缓存与热更新
//! [`ScriptCache`] 按脚本源码的 hash 缓存"已编译的 Lua 状态 + 函数"，
//! 策略热更新后下一笔委托发现 hash 变化即自动重新编译（编译失败会在
//! 保存阶段就被 check_syntax 拦下，运行时重建失败按脚本错误处理）。
//! 脚本顶层定义的局部变量/函数会在委托之间保留（同一份编译产物），
//! 可用来写计数器之类的小状态。
//!
//! # Rust 侧的注意事项（为什么这样写）
//! mlua 的 `Lua` 状态默认不跨线程（!Send），开启 `send` feature 后可以
//! 移动（类比 C++ 的 unique_ptr 但允许 move 到别的线程），于是能把整个
//! 运行时放进 `parking_lot::Mutex` 缓存里挂在 SessionCtx 上，被任意连接
//! 任务取用。整个执行过程是**纯同步**的（无 await），与回报计划生成
//! 处"读锁/非 Send 值不跨 await"的既有约定一致。

use mlua::{Function, HookTriggers, Lua, LuaOptions, StdLib, Value, VmState};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// 脚本编译后的 chunk 名（出现在 Lua 报错文本里，行号解析依赖 `:N:` 格式）。
/// '@' 前缀让 Lua 把它当文件名展示（不带引号包裹）
const CHUNK_NAME: &str = "@Lua回报脚本";
/// Lua 报错文本里展示的 chunk 名（'@' 前缀会被剥掉）
const CHUNK_DISPLAY: &str = "Lua回报脚本";
/// 单个脚本运行时的内存上限（32MB，对测试脚本远超够用，防内存炸弹）
const MEMORY_LIMIT: usize = 32 * 1024 * 1024;
/// 单笔委托脚本执行的指令数上限（约 0.1~0.2 秒量级，防死循环挂死连接任务）
const MAX_INSTRUCTIONS: u64 = 10_000_000;
/// 每隔多少条 Lua 指令检查一次指令计数（值越大 hook 开销越小，粒度越粗）
const INSTR_CHECK_INTERVAL: u64 = 1024;

/// 脚本返回的回报项种类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptKind {
    /// 确认回报
    Confirm,
    /// 成交回报
    Trade,
    /// 拒单（执行报告拒绝或业务拒绝）
    Reject,
}

/// 脚本返回的一条回报计划（还没落到具体协议报文上）。
/// 各字段为 None 时由协议侧用策略配置/委托值补默认值（缺省规则见文档）。
#[derive(Debug, Clone)]
pub struct ScriptPlan {
    pub kind: ScriptKind,
    /// 成交数量（股）。None = 剩余全部
    pub qty: Option<f64>,
    /// 成交价格（元）。None = 委托价
    pub price: Option<f64>,
    /// 拒单方式。Some(true) = 业务拒绝，Some(false) = 执行报告拒绝，None = 用策略配置
    pub via_business: Option<bool>,
    /// 拒单原因码。None = 用策略配置
    pub reason: Option<u32>,
    /// 拒单说明文本（仅业务拒绝消息携带）。None = 用策略配置
    pub text: Option<String>,
    /// 本条回报延迟毫秒。None = 用策略配置的确认/成交延迟
    pub delay_ms: Option<u64>,
}

/// 执行环境：会话层在每笔委托回报计划生成前组装（廉价，全是借用）。
#[derive(Clone, Copy)]
pub struct ScriptEnv<'a> {
    /// 平台的脚本编译缓存（挂在 SessionCtx 上，全连接共享）
    pub cache: &'a ScriptCache,
    /// 网关分类（"sz" / "shjj" / "shbond"），进 ctx.Category
    pub category: &'static str,
    /// 网关名，进 ctx.Gateway
    pub gateway: &'a str,
    /// 平台名，进 ctx.Platform
    pub platform: &'a str,
    /// 柜台地址（ip:port），进 ctx.PeerAddr
    pub peer: &'a str,
    /// 日志回调：脚本报错/异常行为要写到前端日志面板（level, message）
    pub log: &'a dyn Fn(&str, String),
}

impl ScriptEnv<'_> {
    /// 执行回报脚本并解析返回值。
    ///
    /// `build_ord` 由协议侧提供（把各自的 NewOrder 结构装成 Lua 表），
    /// `ctx` 表由本模块统一构建。整个函数是同步的（无 await），
    /// mlua 的错误统一转成带行号的可读字符串。
    pub fn run<F>(&self, script: &str, partition_no: i32, build_ord: F) -> Result<Vec<ScriptPlan>, String>
    where
        F: FnOnce(&Lua) -> Result<mlua::Table, mlua::Error>,
    {
        self.cache.with_runtime(script, |lua, func| {
            let g = lua.globals();
            g.set("ord", build_ord(lua)?)?;
            g.set("ctx", build_ctx_table(lua, self, partition_no)?)?;
            let ret: Value = func.call(())?;
            parse_return_value(ret)
        })
    }
}

/// 构建 ctx 表（运行环境信息，三套协议相同）
fn build_ctx_table(lua: &Lua, env: &ScriptEnv, partition_no: i32) -> Result<mlua::Table, mlua::Error> {
    let t = lua.create_table()?;
    t.set("Category", env.category)?;
    t.set("Gateway", env.gateway)?;
    t.set("Platform", env.platform)?;
    t.set("PartitionNo", partition_no)?;
    t.set("PeerAddr", env.peer)?;
    Ok(t)
}

/// 创建一个沙箱化的 Lua 状态：只加载安全的标准库 + 内存上限 +
/// 清理 base 库里的危险函数
fn new_sandboxed_lua() -> Result<Lua, mlua::Error> {
    // 只开 table/string/math（base 库由 mlua 恒定加载）：io（文件）、
    // os（系统命令/时间）、package/require（加载模块）、debug（内部窥探）一概不加载
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH,
        LuaOptions::new(),
    )?;
    lua.set_memory_limit(MEMORY_LIMIT)?;
    // base 库里这几个要么破坏沙箱要么有副作用，置空：
    // - load/dofile：运行时加载并执行任意代码/读取本地文件
    // - print：会把输出打到后端进程 stdout（桌面端无控制台、服务器无人看）
    // - collectgarbage：内存回收交给引擎自己管理
    for name in ["load", "dofile", "print", "collectgarbage"] {
        lua.globals().set(name, Value::Nil)?;
    }
    Ok(lua)
}

/// 一个已编译的脚本运行时：独立的 Lua 状态 + 编译好的脚本函数 + 指令计数器。
struct Runtime {
    lua: Lua,
    func: Function,
    /// 指令计数器（hook 闭包累加，每次运行前清零）。Arc 是为了把同一个
    /// 计数器同时借给 hook 闭包和 Runtime 本身
    budget: Arc<AtomicU64>,
    /// 死循环被 hook 强制中止的标志。mlua 的 hook 返回 Err 时错误文本会被
    /// 位置信息覆盖（只剩 "in main chunk"），拿不到原始消息，所以另立
    /// 标志位由 with_runtime 检测后补上可读的错误说明
    killed: Arc<AtomicBool>,
}

/// 死循环被中止时给用户的错误说明（配合 killed 标志使用）
const KILL_MESSAGE: &str = "脚本执行中断：指令数超过 10,000,000（疑似死循环）";

impl Runtime {
    /// 创建运行时：建沙箱状态 → 编译脚本（只编译不执行）→ 挂指令 hook
    fn new(script: &str) -> Result<Self, mlua::Error> {
        let lua = new_sandboxed_lua()?;
        // into_function 只编译不执行——语法错误在这里暴露，行号与源码一致
        let func = lua.load(script).set_name(CHUNK_NAME).into_function()?;
        let budget = Arc::new(AtomicU64::new(0));
        let killed = Arc::new(AtomicBool::new(false));
        let hook_budget = budget.clone();
        let hook_killed = killed.clone();
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(INSTR_CHECK_INTERVAL as u32),
            move |_lua, _debug| {
                // hook 每隔 INSTR_CHECK_INTERVAL 条指令触发一次，累加计数；
                // 返回 Err 会中止当前脚本的执行（类比 C++ 里抛异常打断）
                let used =
                    hook_budget.fetch_add(INSTR_CHECK_INTERVAL, Ordering::Relaxed) + INSTR_CHECK_INTERVAL;
                if used > MAX_INSTRUCTIONS {
                    hook_killed.store(true, Ordering::Relaxed);
                    Err(mlua::Error::runtime("instruction budget exceeded"))
                } else {
                    Ok(VmState::Continue)
                }
            },
        );
        Ok(Runtime {
            lua,
            func,
            budget,
            killed,
        })
    }

    /// 运行前把指令计数清零、中止标志复位
    fn reset_budget(&self) {
        self.budget.store(0, Ordering::Relaxed);
        self.killed.store(false, Ordering::Relaxed);
    }
}

/// 脚本编译缓存：按脚本源码 hash 缓存运行时，脚本变了自动重建。
/// 挂在 SessionCtx 上（Arc 共享），整个执行过程同步、由 Mutex 串行化，
/// 天然线程安全。
pub struct ScriptCache {
    inner: parking_lot::Mutex<Option<Cached>>,
}

struct Cached {
    hash: u64,
    rt: Runtime,
}

impl ScriptCache {
    pub fn new() -> Self {
        ScriptCache {
            inner: parking_lot::Mutex::new(None),
        }
    }

    /// 取（必要时编译）脚本对应的运行时，然后在闭包里使用它执行。
    /// 闭包拿到 Lua 状态与已编译函数，负责设置全局变量并调用；
    /// 返回的 Result 已统一转成带行号的可读错误文本。
    pub fn with_runtime<T>(
        &self,
        script: &str,
        f: impl FnOnce(&Lua, &Function) -> Result<T, mlua::Error>,
    ) -> Result<T, String> {
        let mut guard = self.inner.lock();
        let hash = script_hash(script);
        // 缓存未命中（首次/脚本热更新后）→ 重新编译
        if guard.as_ref().map_or(true, |c| c.hash != hash) {
            let rt = Runtime::new(script).map_err(|e| prettify_mlua_error(&e))?;
            *guard = Some(Cached { hash, rt });
        }
        let cached = guard.as_mut().expect("刚写入的缓存必然存在");
        cached.rt.reset_budget();
        match f(&cached.rt.lua, &cached.rt.func) {
            Ok(v) => Ok(v),
            Err(e) => {
                // 死循环被 hook 强制中止：mlua 会把错误文本吞成位置信息，
                // 用 killed 标志换回可读的说明
                if cached.rt.killed.load(Ordering::Relaxed) {
                    Err(KILL_MESSAGE.to_string())
                } else {
                    Err(prettify_mlua_error(&e))
                }
            }
        }
    }
}

impl Default for ScriptCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 语法检查（不执行脚本）：编译通过即返回 Ok。
/// 供保存网关/策略时校验，以及前端"检查脚本"按钮即时校验。
pub fn check_syntax(script: &str) -> Result<(), String> {
    // 每次检查临时建一个沙箱状态，成本毫秒级，无需缓存
    let lua = new_sandboxed_lua().map_err(|e| prettify_mlua_error(&e))?;
    lua.load(script)
        .set_name(CHUNK_NAME)
        .into_function()
        .map(|_| ())
        .map_err(|e| prettify_mlua_error(&e))
}

/// 脚本源码 hash（缓存失效判定用；DefaultHasher 足够，不追求抗碰撞）
fn script_hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// 把 mlua 错误转成用户可读的文本：优先提取 chunk 名后面的 `:N:` 行号，
/// 格式化为"第 N 行：..."（与输入框行号一致，因为脚本是原样编译的）。
fn prettify_mlua_error(e: &mlua::Error) -> String {
    let s = e.to_string();
    // 典型格式：
    //   语法错误：syntax error: Lua回报脚本:3: 'end' expected (to close 'if' at line 1) near <eof>
    //   运行时错：Lua回报脚本:5: attempt to index a nil value
    if let Some(pos) = s.find(CHUNK_DISPLAY) {
        let rest = &s[pos + CHUNK_DISPLAY.len()..];
        let rest = rest.trim_start_matches(|c| c == ']' || c == ':');
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            if let Some(msg) = rest[digits.len()..].strip_prefix(':') {
                return format!("第 {} 行：{}", digits, msg.trim());
            }
        }
    }
    s
}

/// 解析脚本返回值为回报计划列表。支持三种形式（详见编写说明文档）：
/// 1. 字符串快捷方式："confirm" / "trade"（确认+全部成交）/ "reject"
/// 2. 单项表：{ kind = "trade", qty = 300, ... }
/// 3. 多项数组：{ { kind = "confirm" }, { kind = "trade", qty = 300 } }
/// nil / 空表 = 不回任何回报
fn parse_return_value(ret: Value) -> Result<Vec<ScriptPlan>, mlua::Error> {
    match ret {
        // 没写 return 或 return nil：不回任何回报（订单仍登记进缓存）
        Value::Nil => Ok(Vec::new()),
        Value::String(s) => match s.to_string_lossy().as_ref() {
            "confirm" => Ok(vec![ScriptPlan {
                kind: ScriptKind::Confirm,
                qty: None,
                price: None,
                via_business: None,
                reason: None,
                text: None,
                delay_ms: None,
            }]),
            // 字符串"trade"是常用套路快捷方式：确认 + 全部成交一笔
            "trade" => Ok(vec![
                ScriptPlan {
                    kind: ScriptKind::Confirm,
                    qty: None,
                    price: None,
                    via_business: None,
                    reason: None,
                    text: None,
                    delay_ms: None,
                },
                ScriptPlan {
                    kind: ScriptKind::Trade,
                    qty: None,
                    price: None,
                    via_business: None,
                    reason: None,
                    text: None,
                    delay_ms: None,
                },
            ]),
            "reject" => Ok(vec![ScriptPlan {
                kind: ScriptKind::Reject,
                qty: None,
                price: None,
                via_business: None,
                reason: None,
                text: None,
                delay_ms: None,
            }]),
            other => Err(mlua::Error::runtime(format!(
                "未知返回字符串：{}（可选 confirm / trade / reject，或返回回报计划表）",
                other
            ))),
        },
        Value::Table(t) => parse_plan_table(&t),
        v => Err(mlua::Error::runtime(format!(
            "返回值类型非法：{}（应为字符串或表）",
            v.type_name()
        ))),
    }
}

/// 解析返回的表：有 kind 字段 = 单项；否则按数组（整数键 1..n）逐项解析
fn parse_plan_table(t: &mlua::Table) -> Result<Vec<ScriptPlan>, mlua::Error> {
    if matches!(t.get::<Value>("kind")?, Value::String(_)) {
        return Ok(vec![parse_item(t)?]);
    }
    let mut items = Vec::new();
    for pair in t.pairs::<Value, Value>() {
        let (k, v) = pair?;
        match (k, v) {
            // 数组元素：每项必须是一张回报计划表
            (Value::Integer(_), Value::Table(item)) => items.push(parse_item(&item)?),
            (Value::Integer(_), v) => {
                return Err(mlua::Error::runtime(format!(
                    "回报计划数组元素类型非法：{}（每项应为 {{ kind = ... }} 表）",
                    v.type_name()
                )))
            }
            // 带命名键却没有 kind 字段：大概率是漏写了 kind 或写错了结构
            (_, Value::Nil) => continue,
            (_, _) => {
                return Err(mlua::Error::runtime(
                    "回报计划表缺少 kind 字段（confirm/trade/reject），或数组中混入了命名键",
                ))
            }
        }
    }
    Ok(items)
}

/// 解析单项回报计划表
fn parse_item(t: &mlua::Table) -> Result<ScriptPlan, mlua::Error> {
    let kind = match t.get::<Value>("kind")? {
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Nil => {
            return Err(mlua::Error::runtime(
                "回报项缺少 kind 字段（confirm/trade/reject）",
            ))
        }
        v => {
            return Err(mlua::Error::runtime(format!(
                "kind 必须是字符串，收到 {}",
                v.type_name()
            )))
        }
    };
    let kind = match kind.as_str() {
        "confirm" => ScriptKind::Confirm,
        "trade" => ScriptKind::Trade,
        "reject" => ScriptKind::Reject,
        other => {
            return Err(mlua::Error::runtime(format!(
                "未知 kind：{}（可选 confirm / trade / reject）",
                other
            )))
        }
    };
    let qty = opt_num(t, "qty")?;
    if let Some(q) = qty {
        if q < 0.0 {
            return Err(mlua::Error::runtime(format!("qty 不能为负数：{}", q)));
        }
    }
    let price = opt_num(t, "price")?;
    if let Some(p) = price {
        if p <= 0.0 {
            return Err(mlua::Error::runtime(format!("price 必须大于 0：{}", p)));
        }
    }
    let delay_ms = match opt_num(t, "delayMs")? {
        Some(d) if d < 0.0 => {
            return Err(mlua::Error::runtime(format!("delayMs 不能为负数：{}", d)))
        }
        Some(d) => Some(d as u64),
        None => None,
    };
    let via_business = match opt_str(t, "via")? {
        Some(v) => match v.as_str() {
            "execution" => Some(false),
            "business" => Some(true),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "via 只能是 execution 或 business，收到 {}",
                    other
                )))
            }
        },
        None => None,
    };
    let reason = match opt_num(t, "reason")? {
        Some(r) if r < 0.0 => {
            return Err(mlua::Error::runtime(format!("reason 不能为负数：{}", r)))
        }
        // 原因码各协议字段宽度不同（深市 u16 / 沪市 u32），统一按 u32 收，
        // 由协议侧再截断
        Some(r) => Some((r as u64).min(u32::MAX as u64) as u32),
        None => None,
    };
    let text = opt_str(t, "text")?;
    Ok(ScriptPlan {
        kind,
        qty,
        price,
        via_business,
        reason,
        text,
        delay_ms,
    })
}

/// 读表里的数字字段（整数/浮点都接受），缺失返回 None，类型不对报错
fn opt_num(t: &mlua::Table, key: &str) -> Result<Option<f64>, mlua::Error> {
    Ok(match t.get::<Value>(key)? {
        Value::Nil => None,
        Value::Integer(i) => Some(i as f64),
        Value::Number(n) => Some(n),
        v => {
            return Err(mlua::Error::runtime(format!(
                "字段 {} 必须是数字，收到 {}",
                key,
                v.type_name()
            )))
        }
    })
}

/// 读表里的字符串字段，缺失返回 None，类型不对报错
fn opt_str(t: &mlua::Table, key: &str) -> Result<Option<String>, mlua::Error> {
    Ok(match t.get::<Value>(key)? {
        Value::Nil => None,
        Value::String(s) => Some(s.to_string_lossy().to_string()),
        v => {
            return Err(mlua::Error::runtime(format!(
                "字段 {} 必须是字符串，收到 {}",
                key,
                v.type_name()
            )))
        }
    })
}

// ---------------------------------------------------------------------------
// 单元测试：语法检查、返回值解析、沙箱限制
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// 无日志的测试环境
    fn test_env(cache: &ScriptCache) -> ScriptEnv<'_> {
        ScriptEnv {
            cache,
            category: "sz",
            gateway: "测试网关",
            platform: "测试平台",
            peer: "127.0.0.1:1111",
            log: &|_level, _msg| {},
        }
    }

    #[test]
    fn syntax_ok() {
        check_syntax("return \"confirm\"").unwrap();
        check_syntax("if ord.OrderQty > 1000 then return \"reject\" end").unwrap();
        // 空脚本也是合法 Lua
        check_syntax("").unwrap();
    }

    #[test]
    fn syntax_error_has_line_number() {
        // 缺 end：Lua 把错误报在文件末尾（<eof> 处）
        let err = check_syntax("if ord.OrderQty > 1000 then\n    return \"reject\"\n").unwrap_err();
        assert!(err.contains("第 3 行"), "错误应带行号: {}", err);
        let err2 = check_syntax("local a = {\n1,\n2,\n").unwrap_err();
        // 3 行内容 + 结尾换行 → eof 报在第 4 行
        assert!(err2.contains("第 4 行"), "错误应带行号: {}", err2);
        // 单行语法错误：报在出错那一行
        let err3 = check_syntax("local a = 1\nlocal b = = 2\n").unwrap_err();
        assert!(err3.contains("第 2 行"), "错误应带行号: {}", err3);
    }

    #[test]
    fn string_shortcuts() {
        let cache = ScriptCache::new();
        let env = test_env(&cache);
        let plans = env.run("return \"confirm\"", 1, |lua| lua.create_table()).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ScriptKind::Confirm);
        // "trade" = 确认 + 全部成交
        let plans = env.run("return \"trade\"", 1, |lua| lua.create_table()).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].kind, ScriptKind::Confirm);
        assert_eq!(plans[1].kind, ScriptKind::Trade);
        assert!(plans[1].qty.is_none());
        let plans = env.run("return \"reject\"", 1, |lua| lua.create_table()).unwrap();
        assert_eq!(plans[0].kind, ScriptKind::Reject);
        // nil / 空 = 不回报
        assert!(env.run("", 1, |lua| lua.create_table()).unwrap().is_empty());
    }

    #[test]
    fn table_forms_and_defaults() {
        let cache = ScriptCache::new();
        let env = test_env(&cache);
        // 单项表（自动视为一项）
        let plans = env
            .run("return { kind = \"trade\", qty = 300 }", 1, |lua| lua.create_table())
            .unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].qty, Some(300.0));
        // 数组形式
        let plans = env
            .run(
                "return { { kind = \"confirm\", delayMs = 5 }, { kind = \"reject\", via = \"business\", reason = 20009, text = \"x\" } }",
                1,
                |lua| lua.create_table(),
            )
            .unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].delay_ms, Some(5));
        assert_eq!(plans[1].via_business, Some(true));
        assert_eq!(plans[1].reason, Some(20009));
        assert_eq!(plans[1].text.as_deref(), Some("x"));
        // 空表 = 不回报
        assert!(env.run("return {}", 1, |lua| lua.create_table()).unwrap().is_empty());
    }

    #[test]
    fn invalid_returns() {
        let cache = ScriptCache::new();
        let env = test_env(&cache);
        // 未知字符串
        assert!(env.run("return \"foo\"", 1, |lua| lua.create_table()).is_err());
        // 未知 kind
        assert!(env.run("return { kind = \"fill\" }", 1, |lua| lua.create_table()).is_err());
        // 负数量
        assert!(env.run("return { kind = \"trade\", qty = -1 }", 1, |lua| lua.create_table()).is_err());
        // 数组里混命名键
        assert!(env
            .run("return { { kind = \"confirm\" }, foo = 1 }", 1, |lua| lua.create_table())
            .is_err());
        // 返回数字
        assert!(env.run("return 42", 1, |lua| lua.create_table()).is_err());
    }

    #[test]
    fn runtime_error_reported() {
        let cache = ScriptCache::new();
        let env = test_env(&cache);
        // 语法通过但执行报错：比较 nil
        let err = env
            .run("if ord.OrderQty > 1000 then return \"reject\" end", 1, |lua| {
                // 故意不设置 ord（模拟访问 nil 全局）
                let _ = lua;
                Err(mlua::Error::runtime("skip"))
            })
            .unwrap_err();
        // 这里 build_ord 报错也应转成字符串错误
        assert!(!err.is_empty());
    }

    #[test]
    fn ctx_table_exposed() {
        let cache = ScriptCache::new();
        let env = test_env(&cache);
        let plans = env
            .run(
                "assert(ctx.Category == \"sz\") assert(ctx.PartitionNo == 115) return \"confirm\"",
                115,
                |lua| lua.create_table(),
            )
            .unwrap();
        assert_eq!(plans[0].kind, ScriptKind::Confirm);
    }

    #[test]
    fn infinite_loop_killed() {
        let cache = ScriptCache::new();
        let env = test_env(&cache);
        let err = env
            .run("while true do end return \"confirm\"", 1, |lua| lua.create_table())
            .unwrap_err();
        assert!(err.contains("死循环"), "应报指令超限: {}", err);
    }
}
