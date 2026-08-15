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
use crate::config::{GatewayCategory, GatewayConfig, PlatformConfig, StrategyConfig};
use crate::event::{ConnInfo, EngineEvent};
use crate::orderbook::{OrderBook, OrderEntry};
// 三个网关目录的会话模块（类别名保持不变，分派代码无需感知目录）
use crate::shbond::session as session_shbond;
use crate::shjj::session as session_shjj;
use crate::sz::session::{self as session_sz, ManualReportKind, SessionCtx};
use crate::stats::{PlatformStats, StatsSnapshot};
use serde::Serialize;
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
    /// 正在运行的网关，key 为网关 id
    running: Mutex<HashMap<String, RunningGateway>>,
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
        let gateways = load_gateways(&data_dir);
        Self {
            inner: Arc::new(Inner {
                data_dir,
                gateways: Mutex::new(gateways),
                running: Mutex::new(HashMap::new()),
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
        }
        // 校验同一网关内端口不重复
        let mut ports: Vec<u16> = gw.platforms.iter().map(|p| p.port).collect();
        ports.sort_unstable();
        ports.dedup();
        if ports.len() != gw.platforms.len() {
            return Err("同一网关下平台监听端口不能重复".into());
        }

        let mut gws = self.inner.gateways.lock().await;
        if let Some(slot) = gws.iter_mut().find(|g| g.id == gw.id) {
            // 保存配置不改变“上次运行状态”（was_running 保留旧值，
            // 与旧版 gateway_state.json 不被保存配置改动时的行为一致）
            gw.was_running = slot.was_running;
            *slot = gw.clone();
        } else {
            gws.push(gw.clone());
        }
        persist_gateways(&self.inner.data_dir, &gws)?;
        Ok(gw)
    }

    /// 删除网关配置（运行中禁删）并落盘（was_running 随配置一起消失）
    pub async fn delete_gateway(&self, id: &str) -> Result<(), String> {
        if self.inner.running.lock().await.contains_key(id) {
            return Err("网关正在运行，请先停止后再删除".into());
        }
        let mut gws = self.inner.gateways.lock().await;
        gws.retain(|g| g.id != id);
        persist_gateways(&self.inner.data_dir, &gws)?;
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
            p.name.clone()
        };
        persist_gateways(&self.inner.data_dir, &gws)?;
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
        let mut gws = self.inner.gateways.lock().await;
        let Some(gw) = gws.iter_mut().find(|g| g.id == id) else {
            return;
        };
        gw.was_running = running;
        if let Err(e) = persist_gateways(&self.inner.data_dir, &gws) {
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
        // 按网关分类调用对应协议的回报构造器（frame/desc/订单状态更新）
        let (frame, desc, update) = match rp.category {
            GatewayCategory::Sz => session_sz::build_manual_report(
                &rp.cfg, &rp.stats, &entry, kind, qty, price, reason,
            ),
            GatewayCategory::Shjj => session_shjj::build_manual_report(
                &rp.cfg, &rp.stats, &entry, kind, qty, price, reason,
            ),
            GatewayCategory::Shbond => session_shbond::build_manual_report(
                &rp.cfg, &rp.stats, &entry, kind, qty, price, reason,
            ),
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
        // 统计：成交/拒单计入对应计数；撤单成功不占计数（那是撤单请求的计数）
        match kind {
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
        Snapshot { gateways }
    }
}

/// 快照根结构（序列化成 JSON 发给前端，字段名与前端 src/types.ts 对应）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
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

/// 配置文件完整路径：<data_dir>/gateways.json
/// （网关配置与上次运行状态 was_running 都在这个文件里）
fn gateways_path(dir: &PathBuf) -> PathBuf {
    dir.join("gateways.json")
}

/// 启动时加载历史配置；文件不存在或格式错误时返回空列表（不报错）
fn load_gateways(dir: &PathBuf) -> Vec<GatewayConfig> {
    let path = gateways_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 把全部网关配置写回 gateways.json（每次保存/删除都全量重写，简单可靠）
fn persist_gateways(dir: &PathBuf, gws: &[GatewayConfig]) -> Result<(), String> {
    let path = gateways_path(dir);
    let s = serde_json::to_string_pretty(gws).map_err(|e| e.to_string())?;
    std::fs::write(&path, s).map_err(|e| format!("保存配置失败 {}: {}", path.display(), e))
}

/// 简易唯一 ID：时间戳 + 随机数（单机场景足够，无需 UUID 那么重）
fn gen_id() -> String {
    use rand::Rng;
    let ts = chrono::Local::now().format("%y%m%d%H%M%S");
    let r: u32 = rand::thread_rng().gen_range(0x1000..=0xFFFF);
    format!("{}{:x}", ts, r)
}
