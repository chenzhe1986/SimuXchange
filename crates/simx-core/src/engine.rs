//! 引擎：网关生命周期管理、配置持久化、快照。
//!
//! # 职责总览
//!
//! Engine 是后端所有操作的统一入口，前端的每个操作最终都落到它头上：
//! - 保存/删除网关配置（写入 gateways.json，重启后不丢）
//! - 启动网关：为每个平台绑定 TCP 监听端口，等柜台来连
//! - 停止网关：通知所有监听任务和已建立的连接优雅退出
//! - 提供快照（snapshot）：前端每秒拉一次，用于刷新界面
//!
//! # 并发模型（为什么到处是 Arc/Mutex）
//!
//! 后端同时执行监听端口、服务柜台连接、响应前端请求等多组并发任务
//! （tokio 异步任务）。多任务共享同一份数据时的机制：
//! - `Arc<T>`：引用计数智能指针（类比 C++ shared_ptr），让多任务共享数据
//! - `Mutex<T>`：互斥锁，保证同一时刻只有一个任务修改数据
//! - `watch` 通道：停止信号广播（发送 true 后所有订阅者感知）
//! - `broadcast` 通道：日志事件广播（推给前端）

use crate::capture::{ConnRecorder, PacketPage};
use crate::config::{
    DirConfig, GatewayCategory, GatewayConfig, PlatformConfig, StrategyConfig, StrategyMode,
};
use crate::event::{ConnInfo, EngineEvent};
use crate::oplog::OpLog;
use crate::orderbook::{OrderBook, OrderEntry};
// 三个网关目录的会话模块（类别名保持不变，分派代码无需感知目录）
use crate::shbond::session as session_shbond;
use crate::shjj::session as session_shjj;
use crate::sz::session::{self as session_sz, ManualReportKind, SessionCtx};
use crate::stats::{PlatformStats, StatsSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch, Mutex};

/// 运行中的平台：保存该平台监听任务对应的统计器和连接表，
/// 供快照时读取（配置本身在 gateways 里，这里只存“运行时状态”）
struct RunningPlatform {
    /// 对应 PlatformConfig.id
    cfg_id: String,
    /// 网关分类（手动回复时决定用哪套协议的回报构造器）
    category: GatewayCategory,
    /// 平台配置（手动回复构造回报时读分区号等字段）
    cfg: Arc<PlatformConfig>,
    /// 运行时回报策略（热更新：网关运行中修改后对后续委托实时生效）
    strategy: Arc<StdRwLock<StrategyConfig>>,
    /// 统计计数器（委托/确认/成交…，原子变量，会话线程直接累加）
    stats: Arc<PlatformStats>,
    /// 当前存活的柜台连接（key 为连接序号）
    connections: Arc<StdMutex<HashMap<u64, ConnInfo>>>,
    /// 各连接的收发报文记录器（供前端报文弹窗拉取）
    recorders: Arc<StdMutex<HashMap<u64, Arc<ConnRecorder>>>>,
    /// 各连接的回报发送通道（手动回复成交/拒单/撤单时投递回报）
    conn_tx: Arc<StdMutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>,
    /// 订单缓存（平台开启“缓存订单”时创建；界面查看订单列表、撤单判断用）
    orders: Option<Arc<OrderBook>>,
}

/// 运行中的网关：持有停止信号的发送端，drop 或 send(true) 即可让
/// 该网关下所有监听任务与连接退出
struct RunningGateway {
    shutdown: watch::Sender<bool>,
    platforms: Vec<RunningPlatform>,
}

/// 引擎内部状态（被 Arc 包裹，多处共享）
struct Inner {
    /// gateways.json 所在目录
    data_dir: PathBuf,
    /// 全部网关配置（含未运行的）
    gateways: Mutex<Vec<GatewayConfig>>,
    /// 全部网关目录（与网关配置同存 gateways.json，平铺存储、
    /// 靠 DirConfig.parent_id 表达父子层级）
    dirs: Mutex<Vec<DirConfig>>,
    /// 正在运行的网关，key 为网关 id
    running: Mutex<HashMap<String, RunningGateway>>,
    /// 前端操作日志（带前端 IP，按日期存 log/ 目录）
    oplog: OpLog,
    /// 日志事件广播通道（容量 4096，满了会丢弃最旧的）
    events: broadcast::Sender<EngineEvent>,
}

/// 模拟撮合引擎（可被 Tauri 内嵌，也可被 simx-server 独立托管）。
/// Clone 得到的是同一个引擎的另一个引用（内部 Arc 共享），而非副本。
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    /// 创建引擎并从 data_dir/gateways.json 加载历史配置（没有则为空）
    pub fn new(data_dir: PathBuf) -> Self {
        let (events, _) = broadcast::channel(4096);
        let mut store = load_store(&data_dir);
        // 历史配置可能没有创建/修改时间（本功能之前生成的）：
        // 启动时统一回填当前时间并落盘一次，让时间固定下来——
        // 否则每次启动都会生成新的“回填时间”，创建时间永远在变
        if backfill_times(&mut store) {
            let _ = persist_store_file(&data_dir, &store);
        }
        Self {
            inner: Arc::new(Inner {
                data_dir: data_dir.clone(),
                gateways: Mutex::new(store.gateways),
                dirs: Mutex::new(store.dirs),
                running: Mutex::new(HashMap::new()),
                oplog: OpLog::new(data_dir),
                events,
            }),
        }
    }

    /// 订阅引擎事件（每个订阅者获得独立的接收端，互不影响）
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.inner.events.subscribe()
    }

    /// 发一条网关级日志事件（send 失败说明没有订阅者，忽略即可）
    fn emit(&self, level: &str, gateway_id: &str, msg: String) {
        let _ = self
            .inner
            .events
            .send(EngineEvent::log(level, gateway_id, "", msg));
    }

    /// 记一条前端操作日志（带前端 IP；由 api::dispatch 与 WebSocket 连接层调用）。
    /// 写失败静默忽略（辅助记录，不阻塞主流程）。
    pub fn log_op(&self, ip: &str, msg: &str) {
        self.inner.oplog.log(ip, msg);
    }

    /// 按 id 查网关名称（前端操作日志里把 id 换成可读名称用）。
    /// 查不到返回 None，调用方可用 id 兜底。
    pub async fn gateway_name(&self, id: &str) -> Option<String> {
        let gws = self.inner.gateways.lock().await;
        gws.iter().find(|g| g.id == id).map(|g| g.name.clone())
    }

    /// 按 id 查目录名称（操作日志用）。查不到返回 None，调用方用 id 兜底。
    pub async fn dir_name(&self, id: &str) -> Option<String> {
        let dirs = self.inner.dirs.lock().await;
        dirs.iter().find(|d| d.id == id).map(|d| d.name.clone())
    }

    /// 按网关 id + 平台 id 查 (网关名, 平台名)（操作日志用）。
    /// 查不到返回 None，调用方用原始 id 兜底。
    pub async fn platform_name(
        &self,
        gateway_id: &str,
        platform_id: &str,
    ) -> Option<(String, String)> {
        let gws = self.inner.gateways.lock().await;
        let gw = gws.iter().find(|g| g.id == gateway_id)?;
        let plat = gw.platforms.iter().find(|p| p.id == platform_id)?;
        Some((gw.name.clone(), plat.name.clone()))
    }

    /// 校验 Lua 回报脚本（mode == LuaScript 的平台在保存时调用）：
    /// 脚本不能为空 + 语法必须编译通过。错误信息带平台名，直接展示给用户。
    fn validate_lua_script(platform_name: &str, strategy: &StrategyConfig) -> Result<(), String> {
        if strategy.mode != StrategyMode::LuaScript {
            return Ok(());
        }
        if strategy.lua_script.trim().is_empty() {
            return Err(format!(
                "平台 [{}] 已选择 Lua 脚本回报模式，但脚本内容为空",
                platform_name
            ));
        }
        crate::lua_engine::check_syntax(&strategy.lua_script)
            .map_err(|e| format!("平台 [{}] 的 Lua 回报脚本语法错误：{}", platform_name, e))
    }

    /// 新建或更新网关配置并落盘。
    /// 规则：运行中禁改；id 为空视为新建（自动生成 id）；同网关内端口不可重复
    pub async fn save_gateway(&self, mut gw: GatewayConfig) -> Result<GatewayConfig, String> {
        if self.inner.running.lock().await.contains_key(&gw.id) {
            return Err("网关正在运行，请先停止后再修改".into());
        }
        if gw.name.trim().is_empty() {
            return Err("网关名称不能为空".into());
        }
        if gw.id.is_empty() {
            gw.id = gen_id();
        }
        for p in gw.platforms.iter_mut() {
            if p.id.is_empty() {
                p.id = gen_id();
            }
            // “展示收发报文”与“持久化到文件”已合并：勾选展示即自动持久化，
            // 保存时强制两者一致，兼容旧配置里只开其中一个的情况
            p.persist_packets = p.show_packets;
            // Lua 脚本回报模式：保存时校验脚本（非空 + 语法），错误则整体失败，
            // 保证落盘的配置里的脚本都是可执行的
            Self::validate_lua_script(&p.name, &p.strategy)?;
        }
        // 校验同一网关内端口不重复
        let mut ports: Vec<u16> = gw.platforms.iter().map(|p| p.port).collect();
        ports.sort_unstable();
        ports.dedup();
        if ports.len() != gw.platforms.len() {
            return Err("同一网关下平台监听端口不能重复".into());
        }

        // ---- 创建/修改时间维护 ----
        // 新建（id 刚生成）→ 创建时间与修改时间都取当下；
        // 编辑 → 创建时间保留（旧配置缺时间字段时由启动回填兜底），修改时间刷新。
        // 平台同理：新平台两个时间都取当下；已有平台保留创建时间，
        // 内容有变化才刷新修改时间（内容比较需先清掉时间字段，否则恒不相等）
        let now = now_str();
        // 目录列表仅落盘用：先克隆（锁即取即还），避免持有网关表锁的同时
        // 再去等目录锁（全工程统一“先目录锁后网关锁”的加锁次序防死锁）
        let dirs = self.inner.dirs.lock().await.clone();
        let mut gws = self.inner.gateways.lock().await;
        let old_gw = gws.iter().find(|g| g.id == gw.id).cloned();
        match &old_gw {
            Some(prev) => {
                gw.created_at = if prev.created_at.is_empty() {
                    now.clone()
                } else {
                    prev.created_at.clone()
                };
                gw.updated_at = now.clone();
                for p in gw.platforms.iter_mut() {
                    match prev.platforms.iter().find(|x| x.id == p.id) {
                        Some(pp) => {
                            p.created_at = pp.created_at.clone();
                            // 时间字段由服务端维护，比较“内容有没有变”时先清掉
                            let mut old_body = pp.clone();
                            old_body.created_at.clear();
                            old_body.updated_at.clear();
                            let mut new_body = p.clone();
                            new_body.created_at.clear();
                            new_body.updated_at.clear();
                            if old_body != new_body {
                                p.updated_at = now.clone();
                            } else {
                                p.updated_at = pp.updated_at.clone();
                            }
                        }
                        None => {
                            p.created_at = now.clone();
                            p.updated_at = now.clone();
                        }
                    }
                }
            }
            None => {
                gw.created_at = now.clone();
                gw.updated_at = now.clone();
                for p in gw.platforms.iter_mut() {
                    p.created_at = now.clone();
                    p.updated_at = now.clone();
                }
            }
        }

        if let Some(slot) = gws.iter_mut().find(|g| g.id == gw.id) {
            // 保存配置不改变“上次运行状态”（was_running 保留旧值，
            // 与旧版 gateway_state.json 不被保存配置改动时的行为一致）
            gw.was_running = slot.was_running;
            *slot = gw.clone();
        } else {
            gws.push(gw.clone());
        }
        self.persist_store(&dirs, &gws)?;
        Ok(gw)
    }

    /// 删除网关配置（运行中禁删）并落盘（was_running 随配置一起消失）
    pub async fn delete_gateway(&self, id: &str) -> Result<(), String> {
        if self.inner.running.lock().await.contains_key(id) {
            return Err("网关正在运行，请先停止后再删除".into());
        }
        // 目录列表仅落盘用：先克隆（锁即取即还），理由同 save_gateway
        let dirs = self.inner.dirs.lock().await.clone();
        let mut gws = self.inner.gateways.lock().await;
        gws.retain(|g| g.id != id);
        self.persist_store(&dirs, &gws)?;
        Ok(())
    }

    /// 把目录与网关配置全量写回 gateways.json（每次改动都全量重写，简单可靠）。
    /// 调用方负责在持锁期间把两份最新数据切片传进来；本函数内部不再取任何
    /// 锁，因此不存在加锁次序问题。
    fn persist_store(&self, dirs: &[DirConfig], gateways: &[GatewayConfig]) -> Result<(), String> {
        persist_store_file(
            &self.inner.data_dir,
            &GatewayStore {
                dirs: dirs.to_vec(),
                gateways: gateways.to_vec(),
            },
        )
    }

    /// 新建或重命名网关目录并落盘（id 为空 = 新建，自动生成 id）。
    /// 层级规则：目录最多两级——子目录的父目录必须存在且本身是根级目录；
    /// 重命名只改名字，目录不允许在层级间移动（parent_id 保持旧值）。
    pub async fn save_dir(&self, mut dir: DirConfig) -> Result<DirConfig, String> {
        if dir.name.trim().is_empty() {
            return Err("目录名称不能为空".into());
        }
        if dir.id.is_empty() {
            dir.id = gen_id();
        }
        let mut dirs = self.inner.dirs.lock().await;
        if !dir.parent_id.is_empty() {
            // 子目录的父目录必须存在，且父目录本身必须是根级目录（两层层级上限）
            let parent = dirs
                .iter()
                .find(|d| d.id == dir.parent_id)
                .ok_or_else(|| "父目录不存在，可能已被删除".to_string())?;
            if !parent.parent_id.is_empty() {
                return Err("目录最多两级，子目录内不能再新建目录".into());
            }
        }
        // 同一父级下目录名不能重复（重命名时与自身比较需排除）
        if dirs
            .iter()
            .any(|d| d.id != dir.id && d.parent_id == dir.parent_id && d.name == dir.name)
        {
            return Err(format!("同级下已存在同名目录「{}」", dir.name));
        }
        match dirs.iter_mut().find(|d| d.id == dir.id) {
            Some(slot) => {
                dir.parent_id = slot.parent_id.clone();
                *slot = dir.clone();
            }
            None => dirs.push(dir.clone()),
        }
        // 落盘需要网关表快照：此时持有目录锁再取网关锁，次序符合约定
        let gws = self.inner.gateways.lock().await.clone();
        self.persist_store(&dirs, &gws)?;
        Ok(dir)
    }

    /// 删除网关目录（连同其子目录一起删除）并落盘。
    /// delete_gateways = true 时目录（含子目录）下的全部网关一并删除，
    /// 但只要有一个网关在运行就整体失败；false 时网关保留、移回根层级。
    pub async fn delete_dir(&self, id: &str, delete_gateways: bool) -> Result<(), String> {
        let mut dirs = self.inner.dirs.lock().await;
        if !dirs.iter().any(|d| d.id == id) {
            return Err("目录不存在，可能已被删除".into());
        }
        // 目录最多两级，收集一层子目录 id 即可覆盖整个待删范围
        let sub_ids: Vec<String> = dirs
            .iter()
            .filter(|d| d.parent_id == id)
            .map(|d| d.id.clone())
            .collect();
        let in_scope = |dir_id: &str| dir_id == id || sub_ids.iter().any(|s| s == dir_id);
        let mut gws = self.inner.gateways.lock().await;
        if delete_gateways {
            // 运行表与网关表都已在手：先整体检查再动手，
            // 避免删到一半才发现有运行中网关留下半删状态
            let running = self.inner.running.lock().await;
            for g in gws.iter().filter(|g| in_scope(&g.dir_id)) {
                if running.contains_key(&g.id) {
                    return Err(format!("网关「{}」正在运行，不允许删除", g.name));
                }
            }
            gws.retain(|g| !in_scope(&g.dir_id));
        } else {
            // 不删网关：目录（含子目录）下的网关移回根层级（dir_id 清空）
            for g in gws.iter_mut().filter(|g| in_scope(&g.dir_id)) {
                g.dir_id = String::new();
                g.updated_at = now_str(); // 归属变化属于配置修改，刷新修改时间
            }
        }
        dirs.retain(|d| !in_scope(&d.id));
        self.persist_store(&dirs, &gws)?;
        Ok(())
    }

    /// 把网关移动到目录（dir_id 传空串 = 移回根层级）并落盘。
    /// 只修改归属字段、不触碰运行时数据，因此运行中的网关也允许移动。
    pub async fn move_gateway(&self, id: &str, dir_id: &str) -> Result<(), String> {
        // 先校验目标目录并克隆目录快照（锁即取即还），再动网关表
        let dirs = {
            let d = self.inner.dirs.lock().await;
            if !dir_id.is_empty() && !d.iter().any(|x| x.id == dir_id) {
                return Err("目标目录不存在，可能已被删除".into());
            }
            d.clone()
        };
        let mut gws = self.inner.gateways.lock().await;
        let gw = gws
            .iter_mut()
            .find(|g| g.id == id)
            .ok_or_else(|| "网关不存在，可能已被删除".to_string())?;
        gw.dir_id = dir_id.to_string();
        gw.updated_at = now_str(); // 归属变化属于配置修改，刷新修改时间
        self.persist_store(&dirs, &gws)?;
        Ok(())
    }

    /// 更新网关的标签与备注（右键“详情”弹窗保存时调用）。
    /// 只改元数据字段、不触碰运行时数据，因此运行中的网关也允许修改；
    /// 修改时间随标签/备注一起刷新。
    pub async fn update_gateway_meta(&self, id: &str, tags: &str, notes: &str) -> Result<(), String> {
        // 目录列表仅落盘用：先克隆（锁即取即还），理由同 save_gateway
        let dirs = self.inner.dirs.lock().await.clone();
        let mut gws = self.inner.gateways.lock().await;
        let gw = gws
            .iter_mut()
            .find(|g| g.id == id)
            .ok_or_else(|| "网关不存在，可能已被删除".to_string())?;
        gw.tags = tags.trim().to_string();
        gw.notes = notes.trim().to_string();
        gw.updated_at = now_str();
        self.persist_store(&dirs, &gws)?;
        Ok(())
    }

    /// 启动网关：为其下每个平台绑定监听端口，并为每个平台起一个
    /// “accept 循环”异步任务：每接受一个柜台连接就再起一个会话任务去伺候它。
    /// 任一端口绑定失败则整体失败（已绑定的随函数返回自动释放）。
    pub async fn start_gateway(&self, id: &str) -> Result<(), String> {
        {
            if self.inner.running.lock().await.contains_key(id) {
                return Err("网关已在运行".into());
            }
        }
        let gw = {
            let gws = self.inner.gateways.lock().await;
            gws.iter().find(|g| g.id == id).cloned().ok_or("网关不存在")?
        };
        if gw.platforms.is_empty() {
            return Err("网关下没有配置平台，请先添加平台".into());
        }

        // 先完成所有端口绑定，任一失败则整体回退
        let mut listeners: Vec<(PlatformConfig, TcpListener)> = Vec::new();
        for p in &gw.platforms {
            let addr = format!("{}:{}", p.listen_host, p.port);
            match TcpListener::bind(&addr).await {
                Ok(l) => listeners.push((p.clone(), l)),
                Err(e) => {
                    return Err(format!("平台 [{}] 监听 {} 失败: {}", p.name, addr, e));
                }
            }
        }

        // 每个平台一个 accept 循环任务：用 tokio::select! 同时等待
        // “新连接到来”和“停止信号”两件事，哪个先发生处理哪个。
        // 网关分类决定新连接交给哪套会话处理器（深交所 / 上交所竞价）
        let category = gw.category;
        let (shutdown_tx, _) = watch::channel(false);
        let mut running_platforms = Vec::new();
        // 报文文件写入目录的共享句柄（各平台/连接共用同一个 data_dir）
        let data_dir = Arc::new(self.inner.data_dir.clone());
        for (pcfg, listener) in listeners {
            let stats = Arc::new(PlatformStats::default());
            let connections: Arc<StdMutex<HashMap<u64, ConnInfo>>> =
                Arc::new(StdMutex::new(HashMap::new()));
            let recorders: Arc<StdMutex<HashMap<u64, Arc<ConnRecorder>>>> =
                Arc::new(StdMutex::new(HashMap::new()));
            let conn_tx: Arc<StdMutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>> =
                Arc::new(StdMutex::new(HashMap::new()));
            let capture_seq = Arc::new(AtomicU64::new(0));
            // 运行时策略共享区：与持久化配置独立，支持网关运行中热更新
            let strategy = Arc::new(StdRwLock::new(pcfg.strategy.clone()));
            // 开启“缓存订单”的平台才创建订单缓存（关闭时零开销，撤单回到统一失败）
            let orders = pcfg
                .cache_orders
                .then(|| Arc::new(OrderBook::new()));
            let cfg = Arc::new(pcfg);
            let ctx = SessionCtx {
                gateway_id: gw.id.clone(),
                gateway_name: gw.name.clone(),
                cfg: cfg.clone(),
                strategy: strategy.clone(),
                // Lua 回报脚本编译缓存：平台生命周期内共享，策略热更新后
                // 下一笔委托自动重新编译
                lua_cache: Arc::new(crate::lua_engine::ScriptCache::new()),
                stats: stats.clone(),
                connections: connections.clone(),
                recorders: recorders.clone(),
                conn_tx: conn_tx.clone(),
                orders: orders.clone(),
                data_dir: data_dir.clone(),
                capture_seq: capture_seq.clone(),
                events: self.inner.events.clone(),
            };
            let mut shutdown_rx = shutdown_tx.subscribe();
            self.emit(
                "info",
                &gw.id,
                format!(
                    "[{}/{}] 开始监听 {}:{}",
                    gw.name, cfg.name, cfg.listen_host, cfg.port
                ),
            );
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => break,
                        r = listener.accept() => match r {
                            Ok((stream, peer)) => {
                                // 单连接限制：一个平台同一时刻只服务一个柜台连接。
                                // 若已有连接（含尚未登录的），直接关闭新连接拒绝接入
                                let existing = ctx
                                    .connections
                                    .lock()
                                    .unwrap()
                                    .values()
                                    .next()
                                    .cloned();
                                if let Some(prev) = existing {
                                    ctx.log(
                                        "warn",
                                        format!(
                                            "已有柜台连接（{}），拒绝新连接 {} 接入",
                                            prev.peer, peer
                                        ),
                                    );
                                    drop(stream); // 直接断开 TCP，柜台会感知到被拒绝
                                    continue;
                                }
                                let ctx = ctx.clone();
                                let conn_shutdown = shutdown_rx.clone();
                                // 按网关分类分派：三套协议的报文格式完全不同，
                                // 各自的 handle_conn 负责自己那套的登录/心跳/回报
                                match category {
                                    GatewayCategory::Sz => {
                                        tokio::spawn(session_sz::handle_conn(ctx, stream, peer, conn_shutdown));
                                    }
                                    GatewayCategory::Shjj => {
                                        tokio::spawn(session_shjj::handle_conn(ctx, stream, peer, conn_shutdown));
                                    }
                                    GatewayCategory::Shbond => {
                                        tokio::spawn(session_shbond::handle_conn(ctx, stream, peer, conn_shutdown));
                                    }
                                }
                            }
                            Err(e) => {
                                ctx.events.send(EngineEvent::log(
                                    "error", &ctx.gateway_id, &ctx.cfg.id,
                                    format!("accept 失败: {}", e),
                                )).ok();
                            }
                        }
                    }
                }
            });
            running_platforms.push(RunningPlatform {
                cfg_id: cfg.id.clone(),
                category,
                cfg,
                strategy,
                stats,
                connections,
                recorders,
                conn_tx,
                orders,
            });
        }

        self.inner.running.lock().await.insert(
            id.to_string(),
            RunningGateway {
                shutdown: shutdown_tx,
                platforms: running_platforms,
            },
        );
        self.emit("info", id, format!("网关 [{}] 已启动", gw.name));
        // 记住“上次运行状态”：下次带 --auto-start 启动时恢复本网关
        self.set_gateway_state(id, true).await;
        Ok(())
    }

    /// 停止网关：发送停止信号，监听任务退出，各连接发 Logout 后断开
    pub async fn stop_gateway(&self, id: &str) -> Result<(), String> {
        let rg = self
            .inner
            .running
            .lock()
            .await
            .remove(id)
            .ok_or("网关未在运行")?;
        let _ = rg.shutdown.send(true);
        // 停止后不再属于“上次运行状态”，下次 --auto-start 不会恢复它
        self.set_gateway_state(id, false).await;
        self.emit("info", id, "网关已停止".into());
        Ok(())
    }

    /// 热更新某平台的模拟回报策略与回报延迟（网关运行中实时生效）。
    ///
    /// 同时做两件事：
    /// 1. 写入运行时共享区——之后到达的每笔委托都按新策略生成回报；
    /// 2. 同步持久化配置（gateways.json）——重启平台后新值仍然保留。
    /// 网关未运行也可调用（等价于只改持久化配置），前端无需区分两种场景。
    pub async fn update_strategy(
        &self,
        gateway_id: &str,
        platform_id: &str,
        strategy: StrategyConfig,
    ) -> Result<(), String> {
        // 0) Lua 脚本模式先校验脚本（非空 + 语法）——校验失败直接整体报错，
        //    不写运行时共享区也不落盘；平台名查不到时用 id 兜底（提示仍可定位）
        {
            let gws = self.inner.gateways.lock().await;
            let pname = gws
                .iter()
                .find(|g| g.id == gateway_id)
                .and_then(|g| g.platforms.iter().find(|p| p.id == platform_id))
                .map(|p| p.name.clone());
            Self::validate_lua_script(pname.as_deref().unwrap_or(platform_id), &strategy)?;
        }
        // 1) 更新运行中平台的策略（未运行则跳过，只落盘）
        {
            let running = self.inner.running.lock().await;
            if let Some(rg) = running.get(gateway_id) {
                let rp = rg
                    .platforms
                    .iter()
                    .find(|p| p.cfg_id == platform_id)
                    .ok_or("平台不存在")?;
                *rp.strategy.write().unwrap() = strategy.clone();
            }
        }
        // 2) 同步到持久化配置，保证重启后仍生效
        // 目录列表仅落盘用：先克隆（锁即取即还），理由同 save_gateway
        let dirs = self.inner.dirs.lock().await.clone();
        let mut gws = self.inner.gateways.lock().await;
        let name = {
            let gw = gws
                .iter_mut()
                .find(|g| g.id == gateway_id)
                .ok_or("网关不存在")?;
            let p = gw
                .platforms
                .iter_mut()
                .find(|p| p.id == platform_id)
                .ok_or("平台不存在")?;
            p.strategy = strategy;
            // 策略热更新属于配置修改：平台与网关的修改时间一起刷新
            p.updated_at = now_str();
            gw.updated_at = now_str();
            p.name.clone()
        };
        self.persist_store(&dirs, &gws)?;
        self.emit(
            "info",
            gateway_id,
            format!("平台 [{}] 的模拟回报策略已热更新（运行中实时生效）", name),
        );
        Ok(())
    }

    /// 启动“上次运行中”的网关（供 --auto-start 启动参数调用）。
    ///
    /// 只恢复 gateways.json 里 was_running=true 的网关（该字段由
    /// start_gateway/stop_gateway 维护），其余保持停止。
    /// 返回失败列表（网关 id, 错误信息），全部成功时为空列表。
    pub async fn auto_start_previous(&self) -> Vec<(String, String)> {
        let ids: Vec<String> = {
            let gws = self.inner.gateways.lock().await;
            gws.iter()
                .filter(|g| g.was_running)
                .map(|g| g.id.clone())
                .collect()
        };
        let mut failures = Vec::new();
        for id in ids {
            if let Err(e) = self.start_gateway(&id).await {
                failures.push((id, e));
            }
        }
        failures
    }

    /// 记录网关“上次运行状态”：启动成功置 was_running=true，停止置 false。
    /// 状态随配置一起存进 gateways.json（与配置同文件、同一次落盘）。
    /// 状态写入失败只打一条日志（辅助记录，不阻塞网关启停主流程）。
    async fn set_gateway_state(&self, id: &str, running: bool) {
        // 目录列表仅落盘用：先克隆（锁即取即还），理由同 save_gateway
        let dirs = self.inner.dirs.lock().await.clone();
        let mut gws = self.inner.gateways.lock().await;
        let Some(gw) = gws.iter_mut().find(|g| g.id == id) else {
            return;
        };
        gw.was_running = running;
        if let Err(e) = self.persist_store(&dirs, &gws) {
            self.emit("warn", id, e);
        }
    }

    /// 重置指定网关的统计计数（仅限运行中的网关，归零后重新累计）
    pub async fn reset_stats(&self, id: &str) -> Result<(), String> {
        let running = self.inner.running.lock().await;
        let rg = running.get(id).ok_or("网关未在运行")?;
        for p in &rg.platforms {
            p.stats.reset();
        }
        Ok(())
    }

    /// 拉取某个连接的收发报文（前端报文弹窗每隔一小段时间轮询）。
    /// after_seq 为游标，只返回 seq 大于它的新报文（after_seq=0 取当前缓冲全部）。
    /// 连接已关闭或未开启报文捕获时返回错误。
    pub async fn conn_packets(
        &self,
        gateway_id: &str,
        platform_id: &str,
        conn_id: u64,
        after_seq: u64,
    ) -> Result<PacketPage, String> {
        let running = self.inner.running.lock().await;
        let rg = running.get(gateway_id).ok_or("网关未在运行")?;
        let rp = rg
            .platforms
            .iter()
            .find(|p| p.cfg_id == platform_id)
            .ok_or("平台不存在")?;
        let rec = rp.recorders.lock().unwrap().get(&conn_id).cloned();
        let rec = rec.ok_or("连接不存在或未开启报文捕获")?;
        let mut page = rec.page(after_seq);
        // 连接级查询：摘要只有当前这一条连接（兼容旧前端字段）
        page.conns = vec![rec.brief()];
        Ok(page)
    }

    /// 拉取某个平台的订单缓存列表（前端订单弹窗拉取；最新在前）。
    /// 平台未运行或未开启“缓存订单”时返回错误。
    pub async fn orders(
        &self,
        gateway_id: &str,
        platform_id: &str,
    ) -> Result<Vec<OrderEntry>, String> {
        let running = self.inner.running.lock().await;
        let rg = running.get(gateway_id).ok_or("网关未在运行")?;
        let rp = rg
            .platforms
            .iter()
            .find(|p| p.cfg_id == platform_id)
            .ok_or("平台不存在")?;
        let ob = rp.orders.as_ref().ok_or("该平台未开启缓存订单")?;
        Ok(ob.all())
    }

    /// 拉取某个平台全部连接的收发报文（平台级报文弹窗轮询）。
    /// 各连接共享平台级序号发生器，把每个连接的缓冲按 seq 过滤后合并排序，
    /// 前端以 after_seq 为游标增量追加（与连接级 conn_packets 语义一致）。
    pub async fn platform_packets(
        &self,
        gateway_id: &str,
        platform_id: &str,
        after_seq: u64,
    ) -> Result<PacketPage, String> {
        let running = self.inner.running.lock().await;
        let rg = running.get(gateway_id).ok_or("网关未在运行")?;
        let rp = rg
            .platforms
            .iter()
            .find(|p| p.cfg_id == platform_id)
            .ok_or("平台不存在")?;
        let recorders = rp.recorders.lock().unwrap();
        let mut packets: Vec<crate::capture::PacketRecord> = Vec::new();
        let mut latest_seq = 0u64;
        let mut persist = false;
        let mut conns = Vec::new();
        for rec in recorders.values() {
            let page = rec.page(after_seq);
            packets.extend(page.packets);
            latest_seq = latest_seq.max(page.latest_seq);
            persist = persist || page.persist;
            // 收集连接摘要（含已断开的：记录器保留，前端按连接分组展示）
            conns.push(rec.brief());
        }
        // seq 平台内全局唯一，按它排序即得全平台的时间顺序
        packets.sort_by_key(|p| p.seq);
        // 摘要按连接号排序，前端分组标题顺序稳定
        conns.sort_by_key(|c| c.conn_id);
        Ok(PacketPage {
            packets,
            latest_seq,
            persist,
            conns,
        })
    }

    /// 手动回复一笔在途订单：把成交/拒单/撤单成功回报发给当前活动连接。
    ///
    /// kind 为回报种类；qty/price 是自然单位（股/元），缺省时由构造器
    /// 用缓存值兜底（全成/委托价）；reason 为拒单原因代码（默认 1）。
    /// 订单必须是缓存中的在途单（已报/部分成交）；平台单连接模式下，
    /// 历史订单的回报也统一从当前连接发出。
    pub async fn send_report(
        &self,
        gateway_id: &str,
        platform_id: &str,
        cl_ord_id: &str,
        kind: ManualReportKind,
        qty: Option<f64>,
        price: Option<f64>,
        reason: Option<i32>,
        front_reject: bool,
    ) -> Result<String, String> {
        let running = self.inner.running.lock().await;
        let rg = running.get(gateway_id).ok_or("网关未在运行")?;
        let rp = rg
            .platforms
            .iter()
            .find(|p| p.cfg_id == platform_id)
            .ok_or("平台不存在")?;
        let ob = rp.orders.as_ref().ok_or("该平台未开启缓存订单")?;
        let entry = ob.find(cl_ord_id).ok_or("订单不存在")?;
        if !entry.status.is_inflight() {
            return Err(format!("订单 [{}] 已是终态，不能再手动回复", cl_ord_id));
        }
        if kind == ManualReportKind::Trade && qty.is_some_and(|q| q <= 0.0) {
            return Err("成交数量必须大于 0".into());
        }
        // 按网关分类调用对应协议的回报构造器（frame/desc/订单状态更新）；
        // 深市按业务特征返回 Result（无成交回报的业务不允许手动成交）
        let (frame, desc, update) = match rp.category {
            GatewayCategory::Sz => session_sz::build_manual_report(
                &rp.cfg, &rp.stats, &entry, kind, qty, price, reason, front_reject,
            )
            .map_err(|e| e)?,
            GatewayCategory::Shjj => session_shjj::build_manual_report(
                &rp.cfg, &rp.stats, &entry, kind, qty, price, reason, front_reject,
            )
            .map_err(|e| e)?,
            GatewayCategory::Shbond => session_shbond::build_manual_report(
                &rp.cfg, &rp.stats, &entry, kind, qty, price, reason, front_reject,
            )
            .map_err(|e| e)?,
        };
        // 回报统一从“当前活动连接”发出：平台同一时刻只服务一个柜台连接，
        // 历史订单可能来自已断开的旧连接，手动回复也走当前连接
        // （取连接号最大者 = 最近接入的当前连接）
        let tx = {
            let map = rp.conn_tx.lock().unwrap();
            let conn_id = map
                .keys()
                .max()
                .copied()
                .ok_or("当前无活动连接，无法发送回报")?;
            map.get(&conn_id).cloned().expect("刚取到的键必然存在")
        };
        tx.send(frame)
            .await
            .map_err(|_| "当前连接已断开，无法发送回报".to_string())?;
        // 统计：确认/成交/拒单计入对应计数；撤单成功不占计数（那是撤单请求的计数）
        match kind {
            ManualReportKind::Ack => rp.stats.acks.fetch_add(1, Ordering::Relaxed),
            ManualReportKind::Trade => rp.stats.trades.fetch_add(1, Ordering::Relaxed),
            ManualReportKind::Reject => rp.stats.order_rejects.fetch_add(1, Ordering::Relaxed),
            ManualReportKind::Cancel => 0,
        };
        // 回报真实发出后同步订单缓存（终态保护：不会把终态改回在途）
        ob.apply(cl_ord_id, &update);
        self.emit(
            "info",
            gateway_id,
            format!("[{}] {}", rp.cfg.name, desc),
        );
        Ok(desc)
    }

    /// 全量快照（配置 + 运行状态 + 统计 + 连接）。
    /// 前端每秒轮询一次，拿到后整体替换界面状态，简单可靠。
    pub async fn snapshot(&self) -> Snapshot {
        let dirs = self.inner.dirs.lock().await.clone();
        let gws = self.inner.gateways.lock().await.clone();
        let running = self.inner.running.lock().await;
        let gateways = gws
            .into_iter()
            .map(|cfg| {
                let rg = running.get(&cfg.id);
                let platforms = cfg
                    .platforms
                    .iter()
                    .map(|p| {
                        let rp = rg.and_then(|r| r.platforms.iter().find(|x| x.cfg_id == p.id));
                        match rp {
                            Some(rp) => PlatformSnapshot {
                                platform_id: p.id.clone(),
                                listening: true,
                                stats: rp.stats.snapshot(),
                                connections: rp
                                    .connections
                                    .lock()
                                    .unwrap()
                                    .values()
                                    .cloned()
                                    .collect(),
                            },
                            None => PlatformSnapshot {
                                platform_id: p.id.clone(),
                                listening: false,
                                stats: StatsSnapshot::default(),
                                connections: Vec::new(),
                            },
                        }
                    })
                    .collect();
                GatewaySnapshot {
                    running: rg.is_some(),
                    config: cfg,
                    platforms,
                }
            })
            .collect();
        Snapshot { dirs, gateways }
    }
}

/// 快照根结构（序列化成 JSON 发给前端，字段名与前端 src/types.ts 对应）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// 全部网关目录（界面侧栏按目录分组展示用）
    pub dirs: Vec<DirConfig>,
    pub gateways: Vec<GatewaySnapshot>,
}

/// 单个网关的快照：配置 + 是否运行 + 各平台运行状态
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySnapshot {
    pub config: GatewayConfig,
    pub running: bool,
    pub platforms: Vec<PlatformSnapshot>,
}

/// 单个平台的运行状态快照
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformSnapshot {
    /// 对应 PlatformConfig.id
    pub platform_id: String,
    /// 是否正在监听端口
    pub listening: bool,
    /// 委托/确认/成交等计数
    pub stats: StatsSnapshot,
    /// 当前柜台连接列表
    pub connections: Vec<ConnInfo>,
}

/// gateways.json 的文件结构（新版）：目录定义与网关配置放同一个文件。
/// 目录与网关都是平铺数组，层级关系靠 parent_id / dir_id 两个 id 字段表达。
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GatewayStore {
    /// 全部网关目录（含子目录）
    pub dirs: Vec<DirConfig>,
    /// 全部网关配置（dir_id 指向所属目录，空 = 根层级）
    pub gateways: Vec<GatewayConfig>,
}

/// gateways.json 兼容加载的中间枚举：untagged 让 serde 按声明顺序逐个
/// 尝试反序列化——先试新版对象结构，失败再试旧版纯网关数组。旧版本
/// 生成的配置文件升级到新版程序时无需任何手工迁移。
#[derive(Deserialize)]
#[serde(untagged)]
enum StoreFile {
    New(GatewayStore),
    /// 旧版格式：文件内容就是一个网关配置数组，没有目录概念
    Old(Vec<GatewayConfig>),
}

/// 配置文件完整路径：<data_dir>/gateways.json
/// （网关配置、目录分组与上次运行状态 was_running 都在这个文件里）
fn gateways_path(dir: &PathBuf) -> PathBuf {
    dir.join("gateways.json")
}

/// 启动时加载历史配置；文件不存在或格式错误时返回空（不报错）。
/// 新格式为对象 { dirs, gateways }，旧格式为纯网关数组（自动兼容，无目录）。
fn load_store(dir: &PathBuf) -> GatewayStore {
    let path = gateways_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<StoreFile>(&s) {
            Ok(StoreFile::New(store)) => store,
            Ok(StoreFile::Old(gateways)) => GatewayStore {
                dirs: Vec::new(),
                gateways,
            },
            Err(_) => GatewayStore::default(),
        },
        Err(_) => GatewayStore::default(),
    }
}

/// 把整个 GatewayStore 写回 gateways.json（全量重写，简单可靠）
fn persist_store_file(dir: &PathBuf, store: &GatewayStore) -> Result<(), String> {
    let s = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    std::fs::write(gateways_path(dir), s)
        .map_err(|e| format!("保存配置失败 {}: {}", gateways_path(dir).display(), e))
}

/// 当前时间字符串（yyyy-MM-dd HH:mm:ss），网关/平台的创建、修改时间用。
/// 该格式可直接按字典序比较大小（同一年度内），存储与界面展示两用。
fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 为旧配置补齐创建/修改时间：网关或平台的时间字段为空时，把当前时间
/// 同时赋给创建时间与修改时间。返回是否发生过补齐（调用方据此决定是否
/// 落盘一次让时间固定下来，否则每次启动都会生成新的“回填时间”）。
fn backfill_times(store: &mut GatewayStore) -> bool {
    let now = now_str();
    let mut changed = false;
    for g in &mut store.gateways {
        if g.created_at.is_empty() || g.updated_at.is_empty() {
            g.created_at = now.clone();
            g.updated_at = now.clone();
            changed = true;
        }
        for p in &mut g.platforms {
            if p.created_at.is_empty() || p.updated_at.is_empty() {
                p.created_at = now.clone();
                p.updated_at = now.clone();
                changed = true;
            }
        }
    }
    changed
}

/// 简易唯一 ID：时间戳 + 随机数（单机场景足够，无需 UUID 那么重）
fn gen_id() -> String {
    use rand::Rng;
    let ts = chrono::Local::now().format("%y%m%d%H%M%S");
    let r: u32 = rand::thread_rng().gen_range(0x1000..=0xFFFF);
    format!("{}{:x}", ts, r)
}
