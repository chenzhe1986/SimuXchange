//! simx-cli：SimuXchange 模拟撮合后端的命令行控制工具（自动化测试用）
//!
//! # 本工具解决什么问题
//!
//! 模拟交易所的界面只能手动操作：选网关、点“启动”、在订单弹窗里回一笔
//! 成交……自动化测试（柜台系统联调回归）需要的是可执行的一条条命令：
//! 启动网关、把策略切到“只回确认”、盯住订单列表、给某笔订单手动补报。
//! 本工具把这些能力做成命令行子命令，一次调用只做一件事，方便脚本拼装。
//!
//! # 架构约定：纯客户端，不重复实现任何业务逻辑
//!
//! 引擎的所有控制命令统一收口在 `simx_core::api::dispatch`
//! （crates/simx-core/src/api.rs），simx-server 的 /ws 服务原样转发
//! （消息格式见 crates/simx-server/src/main.rs 头部注释）。本程序只做
//! 三件事：解析命令行参数 → 组装 JSON 命令 → WebSocket 发出并格式化
//! 输出。命令名、参数、返回结构与 api.rs 完全一致，引擎侧零改动——
//! 今后后端加新命令，CLI 只需加一个转发子命令。
//!
//! # 使用约定
//!
//! - 默认连本机 ws://127.0.0.1:9800/ws（simx-server 默认监听地址）；
//!   用 `login --url <地址>` 登录一次后，地址会保存到用户主目录的
//!   .simx-cli.json，之后所有命令免 --url，直到重新 login 或 logout；
//!   单次命令也可用 --url 临时覆盖（不改变保存值）；
//! - 数量/价格单位与界面一致：股、元（协议放大倍数由后端换算）。
//! - --json 模式把后端响应原样打到 stdout 供脚本解析；连接失败时也
//!   保证 stdout 上有一行 JSON（{"ok":false,"error":...}）。
//!
//! # 退出码
//!
//! - 0：命令成功（后端 ok=true）；
//! - 1：后端返回错误、连接失败或超时；
//! - 2：命令行参数错误（clap 解析失败，如非法 mode 值）。
//!
//! # 参数里的 “@” 前缀
//!
//! 凡是 “脚本文本|@文件” 形态的参数，@ 前缀表示从文件读取内容
//! （Lua 脚本较长时可放文件里，避免命令行转义烦恼）。

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

// clap 的 derive 特性：Parser trait（提供 parse()）/ Subcommand / Args / ValueEnum
// 都要在作用域内，派生的方法调用才能解析
use clap::Parser;

// ============================ 命令行参数定义 ============================

/// 顶层入口：全局选项 + 子命令
#[derive(clap::Parser)]
#[command(name = "simx-cli", version, about = "控制 SimuXchange 模拟交易所后端（需先启动 simx-server）")]
struct Cli {
    /// 本次命令临时指定的控制接口地址（覆盖 login 保存的地址；
    /// 不传则用 login 保存的地址，从未登录过时默认 ws://127.0.0.1:9800/ws。
    /// 可放在子命令之前或之后）
    #[arg(long, global = true)]
    url: Option<String>,
    /// 实际生效的地址：main() 里按“--url > login 保存值 > 默认值”解析后填入
    #[arg(skip)]
    resolved_url: String,
    /// 原样输出后端 JSON 响应（便于脚本用 jq 等解析）；默认人类可读文本
    #[arg(long, global = true)]
    json: bool,
    /// 单次请求超时（毫秒）
    #[arg(long, default_value_t = 10000, global = true)]
    timeout: u64,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// 列出全部网关/平台：运行状态、端口与统计计数
    Snapshot(SnapshotCmd),
    /// 列出指定平台的订单明细（手动回报前先来这里查委托编号）
    Orders(OrdersCmd),
    /// 热更新某平台的模拟回报策略（运行中实时生效并持久化）
    SetStrategy(SetStrategyCmd),
    /// 手动回报一笔在途订单（确认/成交/拒单/撤单成功）
    Send(SendCmd),
    /// 启动网关
    Start(SingleGw),
    /// 停止网关
    Stop(SingleGw),
    /// 重置网关统计计数（仅运行中的网关）
    ResetStats(SingleGw),
    /// 校验 Lua 回报脚本语法（只编译不执行，不保存）
    ValidateLua(ValidateLuaCmd),
    /// 探活：连接控制接口并发一次快照请求
    Health,
    /// 登录：连一次指定地址，成功后保存为默认地址（后续命令免 --url）
    Login(LoginCmd),
    /// 登出：清除已保存的默认地址（恢复 ws://127.0.0.1:9800/ws）
    Logout,
}

#[derive(clap::Args)]
struct LoginCmd {
    /// 控制接口地址，如 ws://192.168.1.10:9800/ws
    #[arg(long)]
    url: String,
}

#[derive(clap::Args)]
struct SnapshotCmd {
    /// 只列指定网关（id 或名称）
    #[arg(long)]
    gateway: Option<String>,
}

#[derive(clap::Args)]
struct OrdersCmd {
    /// 网关 id 或名称
    #[arg(long)]
    gateway: String,
    /// 平台 id 或名称
    #[arg(long)]
    platform: String,
    /// 只列指定状态的订单：new=已报 partial=部分成交 filled=全部成交
    /// cancelled=已撤单 rejected=已拒单（缺省列出全部状态）
    #[arg(long)]
    status: Option<String>,
    /// 最多显示行数（0 = 不限制，防刷屏）
    #[arg(long, default_value_t = 0)]
    limit: usize,
}

#[derive(clap::Args)]
struct SetStrategyCmd {
    /// 网关 id 或名称
    #[arg(long)]
    gateway: String,
    /// 平台 id 或名称
    #[arg(long)]
    platform: String,
    /// 报单自动回报模式（不传则保持当前值）
    #[arg(long, value_enum)]
    mode: Option<StrategyModeArg>,
    /// 拆单笔数下限（多笔拆单模式用）
    #[arg(long, value_name = "N")]
    split_min: Option<u32>,
    /// 拆单笔数上限（多笔拆单模式用）
    #[arg(long, value_name = "N")]
    split_max: Option<u32>,
    /// 价格档位（元），拆单成交的逐笔价差
    #[arg(long, value_name = "X")]
    price_tick: Option<f64>,
    /// 拒单方式：executionReport=执行报告 / businessReject=业务拒绝消息
    #[arg(long, value_enum)]
    reject_via: Option<RejectViaArg>,
    /// 拒单原因代码
    #[arg(long, value_name = "N")]
    reject_reason: Option<u16>,
    /// 拒单原因文字说明
    #[arg(long)]
    reject_text: Option<String>,
    /// 确认回报延迟：N（固定延迟）或 min,max（区间随机），单位毫秒
    #[arg(long, value_name = "毫秒[,毫秒]")]
    ack_delay: Option<String>,
    /// 成交回报延迟：N（固定）或 min,max（区间随机），单位毫秒
    #[arg(long, value_name = "毫秒[,毫秒]")]
    trade_delay: Option<String>,
    /// 撤单回报延迟：N（固定）或 min,max（区间随机），单位毫秒
    #[arg(long, value_name = "毫秒[,毫秒]")]
    cancel_delay: Option<String>,
    /// 撤单自动回报模式：default=按原单状态 / alwaysSuccess=一律成功 /
    /// alwaysReject=一律拒单
    #[arg(long, value_enum)]
    cancel_mode: Option<CancelModeArg>,
    /// 撤单拒单原因代码
    #[arg(long, value_name = "N")]
    cancel_reject_reason: Option<u16>,
    /// 撤单拒单时改发业务拒绝消息（前台拒单）
    #[arg(long)]
    cancel_front_reject: bool,
    /// Lua 回报脚本（“@”前缀 = 从文件读，如 --lua-script @./trade.lua）
    #[arg(long, value_name = "脚本文本|@文件")]
    lua_script: Option<String>,
    /// 用 JSON 文件整体替换策略（内容即策略对象，字段名以 camelCase，
    /// mode 必填；先加载文件，命令行其它参数再覆盖其上）
    #[arg(long, value_name = "策略 JSON 文件")]
    strategy_json: Option<PathBuf>,
}

#[derive(clap::Args)]
struct SendCmd {
    /// 网关 id 或名称
    #[arg(long)]
    gateway: String,
    /// 平台 id 或名称
    #[arg(long)]
    platform: String,
    /// 委托编号（orders 命令查到的委托编号列）
    #[arg(long)]
    order: String,
    /// 回报种类：ack=确认 / trade=成交 / reject=拒单 / cancel=撤单成功
    #[arg(value_enum)]
    kind: SendKind,
    /// 成交数量（股，仅 trade 用；默认按剩余量全成）
    #[arg(long)]
    qty: Option<f64>,
    /// 成交价格（元，仅 trade 用；默认取委托价）
    #[arg(long)]
    price: Option<f64>,
    /// 拒单原因代码（仅 reject 用；不传时按网关分类默认：
    /// 深交所 20009，上交所 1025）
    #[arg(long, value_name = "N")]
    reason: Option<i32>,
    /// kind=reject 时改发业务拒绝消息（Business Reject，默认发执行报告）
    #[arg(long)]
    front_reject: bool,
}

#[derive(clap::Args)]
struct SingleGw {
    /// 网关 id 或名称
    #[arg(long)]
    gateway: String,
}

#[derive(clap::Args)]
struct ValidateLuaCmd {
    /// Lua 脚本文本；“@”前缀 = 从文件读取
    #[arg(value_name = "脚本文本|@文件")]
    script: String,
}

// ---- 枚举参数：值名与后端 serde 枚举名严格一致（camelCase） ----

#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
enum StrategyModeArg {
    /// 全部成交（单笔）：1 条确认 + 1 条全量成交
    #[value(name = "fullSingle")]
    FullSingle,
    /// 全部成交（多笔拆单）：1 条确认 + N 条成交，数量随机拆分
    #[value(name = "fullSplit")]
    FullSplit,
    /// 部分成交（单笔）：1 条确认 + 1 条部分成交，剩余挂单
    #[value(name = "partialSingle")]
    PartialSingle,
    /// 部分成交（多笔拆单）
    #[value(name = "partialSplit")]
    PartialSplit,
    /// 不自动回复：只登记订单缓存，等待手动回报
    #[value(name = "noAutoReply")]
    NoAutoReply,
    /// 只回确认不成交
    #[value(name = "ackOnly")]
    AckOnly,
    /// 拒单：回拒绝回报（方式见 --reject-via）
    #[value(name = "reject")]
    Reject,
    /// Lua 脚本回报：按 --lua-script 内容生成回报
    #[value(name = "luaScript")]
    LuaScript,
}

impl StrategyModeArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::FullSingle => "fullSingle",
            Self::FullSplit => "fullSplit",
            Self::PartialSingle => "partialSingle",
            Self::PartialSplit => "partialSplit",
            Self::NoAutoReply => "noAutoReply",
            Self::AckOnly => "ackOnly",
            Self::Reject => "reject",
            Self::LuaScript => "luaScript",
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
enum RejectViaArg {
    /// 执行报告消息
    #[value(name = "executionReport")]
    ExecutionReport,
    /// 业务拒绝消息（深市 MsgType=4 / 沪市 204）
    #[value(name = "businessReject")]
    BusinessReject,
}

#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
enum CancelModeArg {
    /// 默认：原单在途→撤单成功，否则撤单拒单
    #[value(name = "default")]
    Default,
    /// 一律撤单成功
    #[value(name = "alwaysSuccess")]
    AlwaysSuccess,
    /// 一律撤单拒单
    #[value(name = "alwaysReject")]
    AlwaysReject,
}

#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
enum SendKind {
    /// 确认回报（执行报告，ExecType=New）
    Ack,
    /// 成交回报
    Trade,
    /// 拒单（执行报告，或业务拒绝见 --front-reject）
    Reject,
    /// 撤单成功（订单转终态）
    Cancel,
}

// ====================== 后端数据的缩略结构（只建模用到的字段） ======================
// 字段名与后端 serde 序列化（camelCase）一致；只声明展示/查找需要的，
// 多余的字段 serde 自动忽略。--json 模式下打印完整 JSON，不受这里限制。

/// get_snapshot 响应的缩略版
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    gateways: Vec<GatewaySnapshot>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GatewaySnapshot {
    config: GatewayConfig,
    running: bool,
    platforms: Vec<PlatformSnapshot>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GatewayConfig {
    id: String,
    name: String,
    category: String,
    platforms: Vec<PlatformConfig>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PlatformConfig {
    id: String,
    name: String,
    port: u16,
    #[serde(default)]
    db: Option<CliDbConfig>,
    strategy: StrategyConfig,
}

/// 上海综合网关的数据库配置（CLI 只用于展示地址，不修改）
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CliDbConfig {
    host: String,
    port: u16,
    database: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PlatformSnapshot {
    platform_id: String,
    listening: bool,
    stats: StatsSnapshot,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct StatsSnapshot {
    #[serde(default)]
    orders: u64,
    #[serde(default)]
    acks: u64,
    #[serde(default)]
    trades: u64,
    #[serde(default)]
    order_rejects: u64,
    #[serde(default)]
    business_rejects: u64,
    #[serde(default)]
    cancels: u64,
    #[serde(default)]
    total_connections: u64,
}

/// 回报策略（与 config.rs 的 StrategyConfig 对齐）：set-strategy 用它做
/// “读当前值 → 覆盖指定字段 → 整对象回传”的基底
#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
struct StrategyConfig {
    mode: String,
    split_count_min: u32,
    split_count_max: u32,
    price_tick: f64,
    reject_via: String,
    reject_reason: u16,
    reject_text: String,
    ack_delay: DelayConfig,
    trade_delay: DelayConfig,
    cancel_mode: String,
    cancel_reject_reason: u16,
    cancel_front_reject: bool,
    cancel_delay: DelayConfig,
    lua_script: String,
}

/// 毫秒延迟区间（min=max=0 即同步回报）
#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
struct DelayConfig {
    min_ms: u64,
    max_ms: u64,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            mode: "fullSingle".into(),
            split_count_min: 2,
            split_count_max: 5,
            price_tick: 0.01,
            reject_via: "executionReport".into(),
            reject_reason: 1,
            reject_text: "模拟拒单".into(),
            ack_delay: DelayConfig::default(),
            trade_delay: DelayConfig::default(),
            cancel_mode: "default".into(),
            cancel_reject_reason: 1,
            cancel_front_reject: false,
            cancel_delay: DelayConfig::default(),
            lua_script: String::new(),
        }
    }
}

impl Default for DelayConfig {
    fn default() -> Self {
        Self { min_ms: 0, max_ms: 0 }
    }
}

/// 订单明细（orderbook.rs 的 OrderEntry 缩略）
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderEntry {
    cl_ord_id: String,
    order_id: String,
    security_id: String,
    side: String,
    price: f64,
    qty: f64,
    cum_qty: f64,
    leaves_qty: f64,
    status: String,
    ts: String,
    conn_id: u64,
}

// ============================ 入口 ============================

#[tokio::main]
async fn main() {
    let mut cli = Cli::parse();
    cli.resolved_url = resolve_url(&cli);
    let code = match &cli.cmd {
        Cmd::Snapshot(ref a) => cmd_snapshot(&cli, a).await,
        Cmd::Orders(ref a) => cmd_orders(&cli, a).await,
        Cmd::SetStrategy(ref a) => cmd_set_strategy(&cli, a).await,
        Cmd::Send(ref a) => cmd_send(&cli, a).await,
        Cmd::Start(ref a) => cmd_simple(&cli, "start_gateway", &a.gateway, "启动").await,
        Cmd::Stop(ref a) => cmd_simple(&cli, "stop_gateway", &a.gateway, "停止").await,
        Cmd::ResetStats(ref a) => cmd_simple(&cli, "reset_stats", &a.gateway, "重置统计").await,
        Cmd::ValidateLua(ref a) => cmd_validate_lua(&cli, a).await,
        Cmd::Health => cmd_health(&cli).await,
        Cmd::Login(ref a) => cmd_login(&cli, a).await,
        Cmd::Logout => cmd_logout(&cli).await,
    };
    std::process::exit(code);
}

// ============================ WebSocket 收发 ============================

/// 发一条命令并等它对应的响应（忽略后端推送的 event 消息）。
///
/// 每次调用都重建连接、用完即断：无状态、不占服务器资源，脚本想隔
/// 多久调用一次都行。id 恒为 1——单连接单请求，无需多路复用。
async fn call(url: &str, payload: Value, timeout_ms: u64) -> Result<Value, String> {
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| format!("连接 {} 失败: {}", url, e))?;
    let req = json!({ "id": 1, "payload": payload }).to_string();
    ws.send(tokio_tungstenite::tungstenite::Message::Text(req.into()))
        .await
        .map_err(|e| format!("发送请求失败: {}", e))?;
    // 用绝对期限而非“等 N 毫秒再收一条”：服务器可能先推来若干条 event，
    // timeout_at 保证“总等待时间”不超时
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue, // 非 JSON 文本，跳过
                };
                // 响应带与请求一致的 id；事件推送没有 id 字段，跳过
                if v.get("id").is_some() {
                    return Ok(v.get("resp").cloned().unwrap_or(Value::Null));
                }
            }
            Ok(Some(Ok(_))) => {} // 二进制帧等，跳过
            Ok(Some(Err(e))) => return Err(format!("连接中断: {}", e)),
            Ok(None) => return Err("服务器关闭了连接".into()),
            Err(_) => return Err(format!("等待响应超时（{} 毫秒）", timeout_ms)),
        }
    }
}

/// 把后端响应“拆包”成 data；失败返回错误文本，不做任何打印
fn to_result(resp: Value) -> Result<Value, String> {
    match resp.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => Ok(resp.get("data").cloned().unwrap_or(Value::Null)),
        _ => Err(resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("后端返回未知错误")
            .to_string()),
    }
}

/// 发起一条“用户点名要看结果”的请求，并按统一规则输出：
/// - 成功：json 模式打印后端响应原文，否则把 data 交给子命令渲染
/// - 失败：json 模式打印响应原文（脚本照常解析 error），人类模式打印
///   错误到 stderr；两种情况都返回 Err(())
async fn run_cmd(cli: &Cli, payload: Value) -> Result<Value, ()> {
    let resp = match call(&cli.resolved_url, payload, cli.timeout).await {
        Ok(r) => r,
        Err(e) => {
            // 连接层失败没有后端响应可打：JSON 模式下也拼一条标准错误
            // 响应，保证脚本 stdout 上一定有 JSON 可解析
            if cli.json {
                println!("{}", pretty(&json!({ "ok": false, "error": e })));
            } else {
                eprintln!("错误: {}", e);
            }
            return Err(());
        }
    };
    match to_result(resp.clone()) {
        Ok(data) => {
            if cli.json {
                println!("{}", pretty(&resp));
            }
            Ok(data)
        }
        Err(e) => {
            if cli.json {
                println!("{}", pretty(&resp));
            } else {
                eprintln!("错误: {}", e);
            }
            Err(())
        }
    }
}

/// JSON 美化输出（脚本解析 stdout 用）
fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string())
}

// ============================ 登录（服务器地址持久化） ============================
//
// “登录”的实质：把本次用的 WebSocket 地址保存到用户主目录的
// .simx-cli.json（Windows 为 %USERPROFILE%\.simx-cli.json，其它平台为
// ~/.simx-cli.json）。此后每次命令按“--url > 保存值 > 本机默认值”确定
// 地址，无需再带 --url；重新 login（换地址）或 logout（清除）才改变。

/// 配置文件路径：Windows 取 USERPROFILE，其余平台取 HOME（交叉编译的
/// Linux 产物运行时同样有 HOME 环境变量）
fn config_path() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .expect("无法确定用户主目录（缺少 USERPROFILE/HOME 环境变量）");
    PathBuf::from(home).join(".simx-cli.json")
}

/// 读出 login 保存的地址；未登录过 / 文件损坏时返回 None（读失败静默，
/// 不阻塞命令执行，顶多回到默认地址）
fn load_saved_url() -> Option<String> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("url").and_then(|u| u.as_str()).map(|s| s.to_string())
}

/// 保存登录地址（login 命令探活成功后才调用）
fn save_url(url: &str) -> Result<(), String> {
    let p = config_path();
    std::fs::write(&p, json!({ "url": url }).to_string())
        .map_err(|e| format!("保存地址到 {} 失败: {}", p.display(), e))
}

/// 清除已保存的地址（logout）：文件不存在不算错（从未登录过）
fn remove_saved_url() -> Result<(), String> {
    let p = config_path();
    match std::fs::remove_file(&p) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("删除 {} 失败: {}", p.display(), e)),
    }
}

/// 确定本条命令实际连的地址：显式 --url → login 保存值 → 本机默认值
fn resolve_url(cli: &Cli) -> String {
    if let Some(u) = &cli.url {
        u.clone()
    } else if let Some(saved) = load_saved_url() {
        saved
    } else {
        "ws://127.0.0.1:9800/ws".to_string()
    }
}

/// login：先探活再保存——地址不可达就不写配置文件，避免把坏地址
/// 固化成之后每次命令的默认连接
async fn cmd_login(cli: &Cli, a: &LoginCmd) -> i32 {
    let ok = json!({ "ok": true, "data": json!({ "saved": true, "url": a.url }) });
    let err = |msg: String| json!({ "ok": false, "error": msg });
    // 探活请求直接发到目标地址（不用 resolved_url——login 的 --url 是必填的）
    match call(&a.url, json!({ "cmd": "get_snapshot" }), cli.timeout).await {
        Ok(_) => match save_url(&a.url) {
            Ok(_) => {
                if cli.json {
                    println!("{}", pretty(&ok));
                } else {
                    println!("已登录 {}（后续命令无需再带 --url）", a.url);
                }
                0
            }
            Err(e) => finish_err(cli, &err(e), "保存地址失败"),
        },
        Err(e) => finish_err(cli, &err(e), "连接失败"),
    }
}

fn finish_err(cli: &Cli, err_json: &Value, tag: &str) -> i32 {
    if cli.json {
        println!("{}", pretty(err_json));
    } else {
        eprintln!("{}: {}", tag, err_json.get("error").and_then(|v| v.as_str()).unwrap_or(""));
    }
    1
}

/// logout：清除已保存的地址，之后所有命令回到默认本机地址
async fn cmd_logout(cli: &Cli) -> i32 {
    match remove_saved_url() {
        Ok(_) => {
            if cli.json {
                println!("{}", pretty(&json!({ "ok": true, "data": null })));
            } else {
                println!("已登出：恢复默认地址 ws://127.0.0.1:9800/ws");
            }
            0
        }
        Err(e) => finish_err(cli, &json!({ "ok": false, "error": e }), "登出失败"),
    }
}

// ============================ 工具函数 ============================

/// 拉一次快照并解析“网关 id 或名称 + 平台”成后端只认的 id。
/// 后端命令一律用配置里的 id 定位（get_orders / send_report 的 gatewayId/
/// platformId 都是 id），名称只是给人看的，这里统一翻译。
async fn fetch_platform(cli: &Cli, gw_key: &str, pf_key: &str) -> Result<(String, String), String> {
    let resp = call(&cli.resolved_url, json!({ "cmd": "get_snapshot" }), cli.timeout).await?;
    let data = to_result(resp)?;
    let snap: Snapshot =
        serde_json::from_value(data).map_err(|e| format!("解析快照失败: {}", e))?;
    let (gw, pf) = find_platform(&snap, gw_key, Some(pf_key))?;
    let pf = pf.ok_or_else(|| "平台未找到".to_string())?;
    Ok((gw.config.id.clone(), pf.id.clone()))
}

/// 按 id 优先、名称次之的规则在快照里定位网关。
///
/// id（GatewayConfig.id）全工程唯一、恒可精确定位；名称只是给人看的，
/// **不保证唯一**——多个网关重名时若仍按名称取第一个，自动化脚本会在
/// 不知情时连到错误的对象。所以名称命中多个时直接报错并列出候选 id，
/// 请用户改用 id 指定。
fn find_gateway<'a>(snap: &'a Snapshot, gw_key: &str) -> Result<&'a GatewaySnapshot, String> {
    // 先精确按 id 找：即使某个网关的名称恰好等于另一网关的 id，也以 id 为准
    if let Some(g) = snap.gateways.iter().find(|g| g.config.id == gw_key) {
        return Ok(g);
    }
    let hits: Vec<&'a GatewaySnapshot> = snap
        .gateways
        .iter()
        .filter(|g| g.config.name == gw_key)
        .collect();
    match hits.len() {
        0 => Err(format!("未找到网关 {}（用 snapshot 命令查看全部网关）", gw_key)),
        1 => Ok(hits[0]),
        _ => Err(format!(
            "网关名称 {} 重复（共 {} 个），名称无法精确定位，请改用 id：{}",
            gw_key,
            hits.len(),
            hits.iter()
                .map(|g| g.config.id.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        )),
    }
}

/// 在指定网关下按“id 优先、名称次之、重复即报错”定位平台（规则同网关，
/// 名称同样允许重复，重复时必须用平台 id）
fn find_gateway_platform<'a>(
    gw: &'a GatewaySnapshot,
    pf_key: &str,
) -> Result<&'a PlatformConfig, String> {
    if let Some(p) = gw.config.platforms.iter().find(|p| p.id == pf_key) {
        return Ok(p);
    }
    let hits: Vec<&'a PlatformConfig> = gw
        .config
        .platforms
        .iter()
        .filter(|p| p.name == pf_key)
        .collect();
    match hits.len() {
        0 => Err(format!(
            "未找到平台 {}（网关 {} 下的平台 id 见 snapshot）",
            pf_key, gw.config.name
        )),
        1 => Ok(hits[0]),
        _ => Err(format!(
            "平台名称 {} 在网关 {} 下重复（共 {} 个），名称无法精确定位，请改用 id：{}",
            pf_key,
            gw.config.name,
            hits.len(),
            hits.iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        )),
    }
}

/// 在快照里按 id 或名称定位目标：pf_key 为 Some 时同时定位平台，
/// 否则只定位网关。返回（网关快照, 平台配置可选）。
///
/// 后端命令只认 id（如 start_gateway 的 "id" 字段），名称是给人看的：
/// 这里把用户可能输入的名称翻译成配置里的真实 id，避免“后端报不存在”。
/// 名称允许重复（界面新建时并不强制唯一），重复时按上面的规则报错
/// 而不是默默取第一个。
fn find_platform<'a>(
    snap: &'a Snapshot,
    gw_key: &str,
    pf_key: Option<&str>,
) -> Result<(&'a GatewaySnapshot, Option<&'a PlatformConfig>), String> {
    let gw = find_gateway(snap, gw_key)?;
    let pf = match pf_key {
        Some(key) => Some(find_gateway_platform(gw, key)?),
        None => None,
    };
    Ok((gw, pf))
}

/// 把值写入 JSON 对象的字段（对象形态才有意义）
fn set_str(v: &mut Value, key: &str, val: Value) {
    if let Some(o) = v.as_object_mut() {
        o.insert(key.to_string(), val);
    }
}

/// 解析 --xxx-delay 参数："0"（同步）/ "500"（固定）/ "100,500"（区间随机）
fn parse_delay(s: &str) -> Result<DelayConfig, String> {
    let parts: Vec<u64> = s
        .split(',')
        .map(|t| t.trim().parse::<u64>().map_err(|_| format!("延迟参数无效: {}", s)))
        .collect::<Result<_, _>>()?;
    match parts.len() {
        1 => Ok(DelayConfig { min_ms: parts[0], max_ms: parts[0] }),
        2 => Ok(DelayConfig { min_ms: parts[0], max_ms: parts[1] }),
        _ => Err(format!("延迟参数无效: {}（应为毫秒数或 delimit 用逗号的两个毫秒数）", s)),
    }
}

/// "脚本文本" 或 "@文件路径" → 脚本字符串
fn read_input(s: &str) -> Result<String, String> {
    if let Some(path) = s.strip_prefix('@') {
        std::fs::read_to_string(path).map_err(|e| format!("读取文件 {} 失败: {}", path, e))
    } else {
        Ok(s.to_string())
    }
}

/// 表格列宽：中文按两个西文字符宽度占位，保证对齐
fn pad(s: &str, w: usize) -> String {
    let wide: usize = s
        .chars()
        .map(|c| if c as u32 > 0x2E80 { 2 } else { 1 })
        .sum();
    if wide >= w {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(w - wide))
    }
}

/// 数量/价格展示：整数不带小数位，其余最多 5 位小数并去尾 0
fn fmt_num(n: f64) -> String {
    let s = format!("{:.5}", n);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

// ============================ 各子命令实现 ============================

/// snapshot：拉全量快照，人类模式渲染成“网关/平台/统计”清单
async fn cmd_snapshot(cli: &Cli, a: &SnapshotCmd) -> i32 {
    let data = match run_cmd(cli, json!({ "cmd": "get_snapshot" })).await {
        Ok(d) => d,
        Err(()) => return 1,
    };
    if cli.json {
        return 0; // 响应已打印
    }
    let snap: Snapshot = match serde_json::from_value(data) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误: 解析快照失败: {}", e);
            return 1;
        }
    };
    let mut found = false;
    for gw in &snap.gateways {
        // --gateway 过滤：id 或名称精确匹配
        if let Some(f) = &a.gateway {
            if gw.config.id != *f && gw.config.name != *f {
                continue;
            }
        }
        found = true;
        let cat = match gw.config.category.as_str() {
            "sz" => "深圳",
            "shjj" => "上海竞价",
            "shbond" => "上海新债券",
            "shzh" => "上海综合",
            _ => "未知",
        };
        let state = if gw.running { "运行中" } else { "已停止" };
        println!("网关: {} [{}] id={} {}", gw.config.name, cat, gw.config.id, state);
        for p in &gw.platforms {
            // 平台名称与地址要从 config 里找（PlatformSnapshot 只有 id/状态/统计）
            let (pname, addr, is_db) = gw
                .config
                .platforms
                .iter()
                .find(|c| c.id == p.platform_id)
                .map(|c| {
                    let addr = match &c.db {
                        Some(db) => format!("{}:{}/{}", db.host, db.port, db.database),
                        None => format!("端口:{}", c.port),
                    };
                    (c.name.clone(), addr, c.db.is_some())
                })
                .unwrap_or_else(|| ("?".to_string(), "?".to_string(), false));
            let st = if p.listening {
                if is_db { "运行中" } else { "监听中" }
            } else if is_db {
                "未运行"
            } else {
                "未监听"
            };
            println!(
                "  平台 {} [id={}] {} {} | 委托 {} 确认 {} 成交 {} 拒单 {} 撤单 {} 连接 {}",
                pname,
                p.platform_id,
                addr,
                st,
                p.stats.orders,
                p.stats.acks,
                p.stats.trades,
                p.stats.order_rejects + p.stats.business_rejects,
                p.stats.cancels,
                p.stats.total_connections
            );
        }
    }
    if !found {
        if let Some(f) = &a.gateway {
            eprintln!("错误: 未找到网关 {}（id 或名称）", f);
        } else {
            eprintln!("错误: 尚未配置任何网关");
        }
        return 1;
    }
    0
}

/// orders：拉平台订单缓存，人类模式渲染表格
async fn cmd_orders(cli: &Cli, a: &OrdersCmd) -> i32 {
    // get_orders 只认 id，先把用户给的“id 或名称”翻译成 id
    let (gw_id, pf_id) = match fetch_platform(cli, &a.gateway, &a.platform).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("错误: {}", e);
            return 1;
        }
    };
    let data = match run_cmd(
        cli,
        json!({ "cmd": "get_orders", "gatewayId": gw_id, "platformId": pf_id }),
    )
    .await
    {
        Ok(d) => d,
        Err(()) => return 1,
    };
    if cli.json {
        return 0;
    }
    let list: Vec<OrderEntry> = match serde_json::from_value(data) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("错误: 解析订单列表失败: {}", e);
            return 1;
        }
    };
    let mut rows: Vec<&OrderEntry> = list.iter().collect();
    if let Some(s) = &a.status {
        let valid = ["new", "partial", "filled", "cancelled", "rejected"];
        if !valid.contains(&s.as_str()) {
            eprintln!("错误: 无效状态 {}（可选：{}）", s, valid.join("/"));
            return 1;
        }
        rows.retain(|o| &o.status == s);
    }
    if a.limit > 0 && rows.len() > a.limit {
        rows.truncate(a.limit);
    }
    if rows.is_empty() {
        println!("（无订单）");
        return 0;
    }
    let headers = ["委托编号", "交易所单号", "证券代码", "方向", "价格", "数量", "剩余", "累计成交", "状态", "时间", "连接"];
    let widths = [16usize, 16, 10, 4, 12, 12, 12, 12, 12, 10, 8];
    let mut head = String::new();
    for (h, w) in headers.iter().zip(widths.iter()) {
        head.push_str(&pad(h, *w));
        head.push(' ');
    }
    println!("{}", head.trim_end());
    for o in &rows {
        let status = match o.status.as_str() {
            "new" => "已报",
            "partial" => "部分成交",
            "filled" => "全部成交",
            "cancelled" => "已撤单",
            "rejected" => "已拒单",
            _ => o.status.as_str(),
        };
        let fields = [
            o.cl_ord_id.clone(),
            o.order_id.clone(),
            o.security_id.clone(),
            o.side.clone(),
            fmt_num(o.price),
            fmt_num(o.qty),
            fmt_num(o.leaves_qty),
            fmt_num(o.cum_qty),
            status.to_string(),
            o.ts.clone(),
            o.conn_id.to_string(),
        ];
        let mut line = String::new();
        for (f, w) in fields.iter().zip(widths.iter()) {
            line.push_str(&pad(f, *w));
            line.push(' ');
        }
        println!("{}", line.trim_end());
    }
    println!(
        "提示：手动回报用 send 子命令（--order 填上面委托编号），如：\n  simx-cli --gateway {} --platform {} send --order {} trade",
        a.gateway, a.platform, rows[0].cl_ord_id
    );
    0
}

/// set-strategy：快照取当前策略做基底 → 命令行参数覆盖 → 整对象回传。
/// 只改一个字段也安全：其余字段用服务器当前值，不会被默认值冲掉。
async fn cmd_set_strategy(cli: &Cli, a: &SetStrategyCmd) -> i32 {
    // 先拉快照定位网关/平台，同时拿到当前策略
    let resp = match call(&cli.resolved_url, json!({ "cmd": "get_snapshot" }), cli.timeout).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("错误: {}", e);
            return 1;
        }
    };
    let data = match to_result(resp) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("错误: 读取当前配置失败: {}", e);
            return 1;
        }
    };
    let snap: Snapshot = match serde_json::from_value(data) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误: 解析快照失败: {}", e);
            return 1;
        }
    };
    let (gw, pf_opt) = match find_platform(&snap, &a.gateway, Some(&a.platform)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("错误: {}", e);
            return 1;
        }
    };
    let pf = match pf_opt {
        Some(p) => p,
        None => {
            eprintln!("错误: 平台未找到");
            return 1;
        }
    };
    // 基底：--strategy-json 给定则整体替换，否则用服务器当前策略
    let mut st: Value = if let Some(path) = &a.strategy_json {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("错误: 读取策略文件失败: {}", e);
                return 1;
            }
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => v,
            _ => {
                eprintln!("错误: 策略文件必须是 JSON 对象（字段 camelCase，mode 必填）");
                return 1;
            }
        }
    } else {
        serde_json::to_value(pf.strategy.clone()).unwrap_or(Value::Null)
    };
    // 命令行参数逐个覆盖（未传的保持现值）
    if let Some(m) = a.mode {
        set_str(&mut st, "mode", json!(m.as_str()));
    }
    if let Some(v) = a.split_min {
        set_str(&mut st, "splitCountMin", json!(v));
    }
    if let Some(v) = a.split_max {
        set_str(&mut st, "splitCountMax", json!(v));
    }
    if let Some(v) = a.price_tick {
        set_str(&mut st, "priceTick", json!(v));
    }
    if let Some(v) = a.reject_via {
        let s = match v {
            RejectViaArg::ExecutionReport => "executionReport",
            RejectViaArg::BusinessReject => "businessReject",
        };
        set_str(&mut st, "rejectVia", json!(s));
    }
    if let Some(v) = a.reject_reason {
        set_str(&mut st, "rejectReason", json!(v));
    }
    if let Some(v) = &a.reject_text {
        set_str(&mut st, "rejectText", json!(v));
    }
    // 三个延迟参数解析失败即提前退出（不用 ? 是因为函数签名返回 i32）
    if let Some(v) = &a.ack_delay {
        match parse_delay(v) {
            Ok(d) => set_str(&mut st, "ackDelay", json!({ "minMs": d.min_ms, "maxMs": d.max_ms })),
            Err(e) => {
                eprintln!("错误: {}", e);
                return 1;
            }
        }
    }
    if let Some(v) = &a.trade_delay {
        match parse_delay(v) {
            Ok(d) => {
                set_str(&mut st, "tradeDelay", json!({ "minMs": d.min_ms, "maxMs": d.max_ms }))
            }
            Err(e) => {
                eprintln!("错误: {}", e);
                return 1;
            }
        }
    }
    if let Some(v) = &a.cancel_delay {
        match parse_delay(v) {
            Ok(d) => {
                set_str(&mut st, "cancelDelay", json!({ "minMs": d.min_ms, "maxMs": d.max_ms }))
            }
            Err(e) => {
                eprintln!("错误: {}", e);
                return 1;
            }
        }
    }
    if let Some(v) = a.cancel_mode {
        let s = match v {
            CancelModeArg::Default => "default",
            CancelModeArg::AlwaysSuccess => "alwaysSuccess",
            CancelModeArg::AlwaysReject => "alwaysReject",
        };
        set_str(&mut st, "cancelMode", json!(s));
    }
    if let Some(v) = a.cancel_reject_reason {
        set_str(&mut st, "cancelRejectReason", json!(v));
    }
    if a.cancel_front_reject {
        set_str(&mut st, "cancelFrontReject", json!(true));
    }
    if let Some(v) = &a.lua_script {
        let script = match read_input(v) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("错误: {}", e);
                return 1;
            }
        };
        set_str(&mut st, "luaScript", json!(script));
    }
    // Lua 模式先做语法校验（后端 update_strategy 也会校验，这里提前
    // 报出更清晰的错误，免得连带“整个更新被拒绝”）
    if st.get("mode").and_then(|m| m.as_str()) == Some("luaScript") {
        let script = st.get("luaScript").and_then(|s| s.as_str()).unwrap_or("");
        if !script.trim().is_empty() {
            let resp = match call(
                &cli.resolved_url,
                json!({ "cmd": "validate_lua_script", "script": script }),
                cli.timeout,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("错误: {}", e);
                    return 1;
                }
            };
            if let Err(e) = to_result(resp) {
                eprintln!("错误: Lua 脚本语法校验失败: {}", e);
                return 1;
            }
        }
    }
    // 回传整套策略（update_strategy：运行中热更新 + 同步持久化配置）；
    // 用解析出来的 id（后端只认 id）
    match run_cmd(
        cli,
        json!({
            "cmd": "update_strategy",
            "gatewayId": gw.config.id,
            "platformId": pf.id,
            "strategy": st,
        }),
    )
    .await
    {
        Ok(_) => {}
        Err(()) => return 1,
    }
    if !cli.json {
        println!(
            "已更新策略: 网关 {} 平台 {}（当前 mode={}）",
            gw.config.name,
            pf.name,
            st.get("mode").and_then(|m| m.as_str()).unwrap_or("?")
        );
    }
    0
}

/// send：手动回报一笔在途订单（确认/成交/拒单/撤单成功）
async fn cmd_send(cli: &Cli, a: &SendCmd) -> i32 {
    // 先拉快照：把“id 或名称”翻译成后端只认的 id，顺带拿到网关分类
    // （拒单缺省原因码按分类取界面同款默认：深圳 20009，上海 1025）
    let resp = match call(&cli.resolved_url, json!({ "cmd": "get_snapshot" }), cli.timeout).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("错误: {}", e);
            return 1;
        }
    };
    let data = match to_result(resp) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("错误: {}", e);
            return 1;
        }
    };
    let snap: Snapshot = match serde_json::from_value(data) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误: 解析快照失败: {}", e);
            return 1;
        }
    };
    let (gw, pf_opt) = match find_platform(&snap, &a.gateway, Some(&a.platform)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("错误: {}", e);
            return 1;
        }
    };
    let pf = match pf_opt {
        Some(p) => p,
        None => {
            eprintln!("错误: 平台未找到");
            return 1;
        }
    };
    let kind = match a.kind {
        SendKind::Ack => "ack",
        SendKind::Trade => "trade",
        SendKind::Reject => "reject",
        SendKind::Cancel => "cancel",
    };
    let mut payload = json!({
        "cmd": "send_report",
        "gatewayId": gw.config.id,
        "platformId": pf.id,
        "clOrdId": a.order,
        "kind": kind,
    });
    match a.kind {
        SendKind::Trade => {
            // 只在显式给出 --qty/--price 时才带，缺省由后端按剩余量/委托价兜底
            if let Some(v) = a.qty {
                if v <= 0.0 {
                    eprintln!("错误: 成交数量必须大于 0");
                    return 1;
                }
                set_str(&mut payload, "qty", json!(v));
            }
            if let Some(v) = a.price {
                if v <= 0.0 {
                    eprintln!("错误: 成交价格必须大于 0");
                    return 1;
                }
                set_str(&mut payload, "price", json!(v));
            }
        }
        SendKind::Reject => {
            // 拒单原因码：显式给过 --reason 就用，否则按网关分类默认
            let reason = match a.reason {
                Some(v) => v,
                None => match gw.config.category.as_str() {
                    "sz" => 20009,
                    _ => 1025,
                },
            };
            set_str(&mut payload, "reason", json!(reason));
            if a.front_reject {
                set_str(&mut payload, "frontReject", json!(true));
            }
        }
        _ => {}
    }
    match run_cmd(cli, payload).await {
        Ok(data) => {
            if !cli.json {
                // 后端返回的 desc 是给界面提示的文案，这里直接展示
                let desc = data.get("desc").and_then(|d| d.as_str()).unwrap_or("回报已发送");
                println!("{}", desc);
            }
            0
        }
        Err(()) => 1,
    }
}

/// start / stop / reset-stats：通用“对网关执行单命令”的转发。
/// 先拉快照把“网关 id 或名称”解析成 id（后端不支持按名称定位）。
async fn cmd_simple(cli: &Cli, cmd: &str, gw_key: &str, label: &str) -> i32 {
    let resp = match call(&cli.resolved_url, json!({ "cmd": "get_snapshot" }), cli.timeout).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("错误: {}", e);
            return 1;
        }
    };
    let data = match to_result(resp) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("错误: {}", e);
            return 1;
        }
    };
    let snap: Snapshot = match serde_json::from_value(data) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误: 解析快照失败: {}", e);
            return 1;
        }
    };
    let id = match find_platform(&snap, gw_key, None).map(|(g, _)| g.config.id.clone()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("错误: {}", e);
            return 1;
        }
    };
    match run_cmd(cli, json!({ "cmd": cmd, "id": id })).await {
        Ok(_) => {
            if !cli.json {
                println!("已{}网关 {}", label, gw_key);
            }
            0
        }
        Err(()) => 1,
    }
}

/// validate-lua：只语法校验，不保存（供脚本在写回策略前自检）
async fn cmd_validate_lua(cli: &Cli, a: &ValidateLuaCmd) -> i32 {
    let script = match read_input(&a.script) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误: {}", e);
            return 1;
        }
    };
    match run_cmd(cli, json!({ "cmd": "validate_lua_script", "script": script })).await {
        Ok(data) => {
            if !cli.json {
                let msg = data.get("message").and_then(|m| m.as_str()).unwrap_or("脚本语法正确");
                println!("{}", msg);
            }
            0
        }
        Err(()) => 1,
    }
}

/// health：探活（能连上就算通，顺带打印网关数量便于脚本断言）
async fn cmd_health(cli: &Cli) -> i32 {
    match run_cmd(cli, json!({ "cmd": "get_snapshot" })).await {
        Ok(_) => {
            if !cli.json {
                println!("ok（已连接 {}）", cli.resolved_url);
            }
            0
        }
        Err(()) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用空策略（除 lua_script 全默认，字段名与真实后端对齐）
    fn strat() -> StrategyConfig {
        StrategyConfig {
            mode: "noAutoReply".into(),
            split_count_min: 0,
            split_count_max: 0,
            price_tick: 0.0,
            reject_via: "executionReport".into(),
            reject_reason: 10,
            reject_text: String::new(),
            ack_delay: DelayConfig { min_ms: 0, max_ms: 0 },
            trade_delay: DelayConfig { min_ms: 0, max_ms: 0 },
            cancel_mode: "default".into(),
            cancel_reject_reason: 0,
            cancel_front_reject: false,
            cancel_delay: DelayConfig { min_ms: 0, max_ms: 0 },
            lua_script: String::new(),
        }
    }

    fn pf(id: &str, name: &str) -> PlatformConfig {
        PlatformConfig {
            id: id.into(),
            name: name.into(),
            port: 10001,
            db: None,
            strategy: strat(),
        }
    }

    fn gw(id: &str, name: &str, platforms: Vec<PlatformConfig>) -> GatewaySnapshot {
        GatewaySnapshot {
            config: GatewayConfig {
                id: id.into(),
                name: name.into(),
                category: "sz".into(),
                platforms,
            },
            running: true,
            platforms: Vec::new(),
        }
    }

    fn snap(gateways: Vec<GatewaySnapshot>) -> Snapshot {
        Snapshot { gateways }
    }

    /// 名称唯一 → 正常命中
    #[test]
    fn name_unique_hit() {
        let s = snap(vec![
            gw("gw-1", "网关A", vec![pf("p-1", "平台1")]),
            gw("gw-2", "网关B", vec![]),
        ]);
        assert_eq!(find_gateway(&s, "网关A").unwrap().config.id, "gw-1");
        // 平台名称唯一同样可命中
        let (g, p) = find_platform(&s, "网关A", Some("平台1")).unwrap();
        assert_eq!(g.config.id, "gw-1");
        assert_eq!(p.unwrap().id, "p-1");
    }

    /// 网关重名：按名称必须报错并列出候选 id，绝不默默取第一个
    #[test]
    fn duplicate_gateway_name_errors() {
        let s = snap(vec![
            gw("gw-dup-1", "重名网关", vec![]),
            gw("gw-dup-2", "重名网关", vec![]),
        ]);
        let err = find_gateway(&s, "重名网关").unwrap_err();
        assert!(err.contains("重复"), "错误应提示重复: {}", err);
        assert!(err.contains("gw-dup-1") && err.contains("gw-dup-2"), "应列出候选 id: {}", err);
    }

    /// 平台重名：在网关内报错并列出候选平台 id
    #[test]
    fn duplicate_platform_errors() {
        let s = snap(vec![gw(
            "gw-1",
            "网关A",
            vec![pf("p-a", "重名平台"), pf("p-b", "重名平台")],
        )]);
        let (g, _) = find_platform(&s, "gw-1", None).unwrap();
        let err = find_gateway_platform(g, "重名平台").unwrap_err();
        assert!(err.contains("p-a") && err.contains("p-b"), "应列出候选 id: {}", err);
        // 用 id 精确定位在同名场景下正常工作
        assert_eq!(find_gateway_platform(g, "p-b").unwrap().id, "p-b");
    }

    /// id 恒优先于名称：即使网关 B 的名字恰好等于网关 A 的 id，也按 id 命中 A
    #[test]
    fn id_wins_over_name() {
        let s = snap(vec![
            gw("abc", "名字一", vec![]),
            gw("g-2", "abc", vec![]),
        ]);
        assert_eq!(find_gateway(&s, "abc").unwrap().config.id, "abc");
        // 名称路径在“abc”是网关 B 的名称时依然能唯一命中它（A 的 id 与它不同）
        assert_eq!(find_gateway(&s, "名字一").unwrap().config.id, "abc");
    }

    /// 数字与 id 都找不到时报“未找到”，提示用户用 snapshot 查看
    #[test]
    fn no_match_errors() {
        let s = snap(vec![gw("gw-1", "网关A", vec![])]);
        assert!(find_gateway(&s, "不存在的网关").unwrap_err().contains("未找到网关"));
    }
}