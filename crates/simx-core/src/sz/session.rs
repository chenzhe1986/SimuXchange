//! 深交所 TCP 会话管理：Logon/Logout/心跳/委托处理
//!
//! # 连接生命周期
//!
//! 柜台（OMS）每连上一个网关监听端口即产生一个会话，完整生命周期如下：
//!
//! ```text
//! 柜台发起 TCP 连接
//!   → 网关等待 Logon 登录报文（可选校验密码）
//!   → 登录成功：回复 Logon 确认 + 下发平台信息/平台状态
//!   → 进入正常工作期：
//!       - 双方按约定间隔互发心跳，证明“我还活着”
//!       - 收到新订单(1xxx01，28 种业务) → 按策略回送确认/成交/拒绝回报
//!       - 收到撤单(190007) → 在途单按原单业务回撤单成功(2xxx02)，否则回撤单失败(290008)
//!   → 结束：柜台注销 / 连接断开 / 心跳超时 / 网关停止
//!   → 清理：从连接表移除，记录日志
//! ```
//!
//! # 心跳超时检测
//!
//! 未单独启动“心跳检查”任务：read_frame 读取报文时携带超时
//! （心跳间隔的 3 倍）。只要对端持续发消息（含心跳），读超时被不断
//! 推迟；长时间收不到任何字节则 read_frame 返回超时错误，会话随之
//! 结束——即常见的空闲检测机制。

use crate::capture::ConnRecorder;
use crate::config::{CancelMode, PlatformConfig, StrategyConfig};
use crate::event::{ConnInfo, EngineEvent};
use crate::orderbook::{OrderBook, OrderEntry, OrderStatus, OrderUpdate};
use super::protocol::{
    self as protocol, exec_type, msg_type, ord_status, BusinessReject, CancelReject,
    Designation, DesignationReport, Evote, EvoteReport, ExecRptAck, ExecRptTrade,
    IndicationOfInterest, IOIResponse, Logon, Logout, MarginQuery, MarginQueryResult,
    MultilegExecRpt, MultilegOrder, NewOrder, OrderCancelRequest, PasswordService,
    PasswordServiceReport, Quote, QuoteItem, QuoteRequest, QuoteRequestAck, QuoteResponse,
    QuoteStatusReport, ReportSync, TcrAck, TradingSessionStatus, TradeCaptureReport,
};
use crate::stats::PlatformStats;
use super::strategy::{self as strategy, biz_info_by_appl_id, ReportKind};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};

/// 全局连接序号发生器：每来一个新连接就 +1，作为连接的唯一编号。
/// AtomicU64 是“原子整数”，多个任务同时加 1 也不会算错，无需加锁。
static CONN_SEQ: AtomicU64 = AtomicU64::new(0);

/// 会话上下文：会话处理时需要用到的“公共资料袋”。
///
/// 由 engine 在启动平台监听时创建一份，之后每接受一个柜台连接就
/// clone 一份传进去。注意里面的字段全是 Arc（共享引用）或可低成本
/// 克隆的类型，所以 clone 很便宜，且大家操作的仍是同一份统计器、
/// 同一张连接表。
#[derive(Clone)]
pub struct SessionCtx {
    /// 所属网关 id（写日志时标注来源用）
    pub gateway_id: String,
    /// 所属网关名称（拼进日志文本，方便人看）
    pub gateway_name: String,
    /// 平台配置（端口、密码等静态字段），只读共享
    pub cfg: Arc<PlatformConfig>,
    /// 运行时回报策略（模拟策略/回报延迟可热更新：网关运行中修改
    /// 后对后续委托实时生效；每次生成回报计划时加锁读取最新值）
    pub strategy: Arc<std::sync::RwLock<StrategyConfig>>,
    /// 平台统计计数器（委托数/成交数…），各连接共同累加
    pub stats: Arc<PlatformStats>,
    /// 当前存活连接表：连接建立时登记、断开时移除，供前端展示
    pub connections: Arc<StdMutex<HashMap<u64, ConnInfo>>>,
    /// 各连接的收发报文记录器（仅当平台开启展示或持久化时登记）
    pub recorders: Arc<StdMutex<HashMap<u64, Arc<ConnRecorder>>>>,
    /// 各连接的回报发送通道（手动回复成交/拒单/撤单时，把回报投给对应连接）
    pub conn_tx: Arc<StdMutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>,
    /// 订单缓存（平台开启“缓存订单”时创建；登记委托、撤单判断用）
    pub orders: Option<Arc<OrderBook>>,
    /// 数据目录（报文文件写到 <data_dir>/packets/ 下）
    pub data_dir: Arc<PathBuf>,
    /// 平台级报文全局序号发生器（同一平台所有连接共享，跨连接唯一）
    pub capture_seq: Arc<AtomicU64>,
    /// 日志事件广播通道的发送端（最终推送到前端日志面板）
    pub events: broadcast::Sender<EngineEvent>,
}

impl SessionCtx {
    /// 发一条带“[网关名/平台名]”前缀的日志事件。
    /// send 失败只说明当前没有订阅者（如前端还没打开），忽略即可。
    /// pub(crate)：shjj/shbond 会话模块复用同一份上下文与日志格式。
    pub(crate) fn log(&self, level: &str, msg: String) {
        let _ = self.events.send(EngineEvent::log(
            level,
            &self.gateway_id,
            &self.cfg.id,
            format!("[{}/{}] {}", self.gateway_name, self.cfg.name, msg),
        ));
    }

    /// 为一个新连接创建报文记录器（三套会话共用）。
    /// 仅当平台配置开启“展示收发报文”或“持久化到文件”任一开关时才创建；
    /// 开启持久化时报文追加写入
    /// <data_dir>/packets/<YYYYMMDD>/<网关名>_<平台名>/pkg_<接入时间>_ip-<IP>_port-<端口>.log
    /// （按日期分文件夹、日期下再按“网关名_平台名”分文件夹；文件名带接入
    /// 时间与柜台地址，一眼知道是几点从哪台柜台接入的连接）。
    pub(crate) fn start_capture(
        &self,
        conn_id: u64,
        peer: &SocketAddr,
    ) -> Option<Arc<ConnRecorder>> {
        if !self.cfg.show_packets && !self.cfg.persist_packets {
            return None;
        }
        let path = if self.cfg.persist_packets {
            // 目录：packets/<YYYYMMDD>/<网关名>_<平台名>/（网关名与平台名都清洗，防非法字符）
            let date = chrono::Local::now().format("%Y%m%d");
            Some(
                self.data_dir
                    .join("packets")
                    .join(date.to_string())
                    .join(format!(
                        "{}_{}",
                        sanitize_file_name(&self.gateway_name),
                        sanitize_file_name(&self.cfg.name)
                    ))
                    .join(packet_file_name(peer)),
            )
        } else {
            None
        };
        let rec = Arc::new(ConnRecorder::new(
            conn_id,
            self.capture_seq.clone(),
            self.cfg.persist_packets,
            path.as_deref(),
            peer.to_string(),
            chrono::Local::now().format("%H:%M:%S").to_string(),
        ));
        self.recorders.lock().unwrap().insert(conn_id, rec.clone());
        Some(rec)
    }
}

/// 报文文件名：pkg_<接入时间HHMMSS>_ip-<IP(点转下划线)>_port-<端口>.log。
/// IP 里的点替换成下划线、非法字符清洗，保证任意地址都能作为文件名落盘。
fn packet_file_name(peer: &SocketAddr) -> String {
    let ip = sanitize_file_name(&peer.ip().to_string().replace('.', "_"));
    format!(
        "pkg_{}_ip-{}_port-{}.log",
        chrono::Local::now().format("%H%M%S"),
        ip,
        peer.port()
    )
}

/// 把名字里的非法字符替换成下划线（Windows 文件名不允许 \ / : * ? " < > |），
/// 空名给个默认值——用于报文文件目录/文件名，保证任意平台名/网关名都能落盘。
fn sanitize_file_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() { "未命名".into() } else { trimmed.into() }
}

/// 从 TCP 流中读取一条完整报文，返回（消息类型, 消息体, 校验和是否正确, 完整原始帧字节）。
///
/// 按协议格式分三步读：8 字节头（MsgType+BodyLength）→ 消息体 → 4 字节校验和。
/// TCP 是“字节流”，不保证一次能收到一整条报文，read_exact 会一直等到
/// 凑够指定字节数才返回，这正好解决了“拆包”问题。
///
/// 返回的最后一项是“头+体+校验和”拼成的完整原始帧，供报文捕获原样记录。
///
/// idle_timeout 同时作用在“读报文头 / 读消息体 / 读校验和”三步上：
/// 若对方长时间一个字节都不发（连心跳都没有），或发完报文头就停住不发
/// 剩余部分（半包攻击/对端异常），读超时都会触发，让会话结束——否则会话
/// 会被一个只发头部的连接永久挂起，占死单连接槽位。
async fn read_frame(
    rh: &mut OwnedReadHalf,
    idle_timeout: Duration,
) -> std::io::Result<(u32, Vec<u8>, bool, Vec<u8>)> {
    // 带超时读满 buf：超时/读错统一转成 io::Error，由调用方结束会话
    async fn read_exact_timeout(
        rh: &mut OwnedReadHalf,
        buf: &mut [u8],
        idle_timeout: Duration,
    ) -> std::io::Result<()> {
        match tokio::time::timeout(idle_timeout, rh.read_exact(buf)).await {
            // 内层是 read_exact 的 io 错误，直接透传（tokio 新版返回已读字节数，
            // 这里只关心成败，忽略 usize）；外层是 timeout 的 Elapsed，转成 io::Error
            Ok(r) => r.map(|_| ()),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "读取超时（心跳丢失）",
            )),
        }
    }

    let mut head = [0u8; 8];
    read_exact_timeout(rh, &mut head, idle_timeout).await?;
    // 报文头是大端序：前 4 字节消息类型，后 4 字节消息体长度
    let mt = u32::from_be_bytes(head[0..4].try_into().unwrap());
    let body_len = u32::from_be_bytes(head[4..8].try_into().unwrap()) as usize;
    // 防御性检查：长度大得离谱（>8MB）说明数据错乱或恶意报文，
    // 直接报错断开，避免按这个长度分配内存把程序撑爆
    if body_len > 8 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("消息体长度异常: {}", body_len),
        ));
    }
    let mut body = vec![0u8; body_len];
    read_exact_timeout(rh, &mut body, idle_timeout).await?;
    let mut cks_buf = [0u8; 4];
    read_exact_timeout(rh, &mut cks_buf, idle_timeout).await?;
    let recv_cks = u32::from_be_bytes(cks_buf);
    // 自己对“头+体”重算一遍校验和，与对方发来的比对，验证数据没被损坏
    let mut all = Vec::with_capacity(8 + body_len + 4);
    all.extend_from_slice(&head);
    all.extend_from_slice(&body);
    let ok = protocol::checksum(&all) == recv_cks;
    // 拼上校验和字节构成完整原始帧，供报文捕获原样记录
    all.extend_from_slice(&cks_buf);
    Ok((mt, body, ok, all))
}

/// 处理一个柜台连接的完整生命周期（本文件的“主函数”，见模块头的流程图）。
///
/// 参数说明：
/// - ctx：会话上下文（配置、统计器、日志通道等）
/// - stream：已建立的 TCP 连接
/// - peer：对方的 IP:端口（写日志用）
/// - shutdown：网关停止信号的接收端，收到 true 表示要优雅关闭
pub async fn handle_conn(
    ctx: SessionCtx,
    stream: TcpStream,
    peer: SocketAddr,
    mut shutdown: watch::Receiver<bool>,
) {
    let conn_id = CONN_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    // 关闭 Nagle 算法：小报文立即发出，不为攒包而等待，降低回报延迟
    let _ = stream.set_nodelay(true);
    // 把连接拆成“读半边”和“写半边”，读写可以在不同任务里独立进行
    let (mut rh, mut wh) = stream.into_split();

    // 报文捕获：平台开启展示/持久化时为本连接建一个记录器（否则为 None，零开销）
    let recorder = ctx.start_capture(conn_id, &peer);
    let rec_send = recorder.clone();

    // 写通道：可能有多方要给柜台发消息（登录回复、心跳任务、延迟回报任务…），
    // 若各自直接写 socket 会互相穿插打乱字节流。因此统一投递到这个 mpsc
    // 队列，由下面唯一的 writer 任务按先来后到逐条写出，天然保证顺序。
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4096);
    // 登记发送通道：界面手动回复成交/拒单/撤单时，经此把回报投给本连接
    ctx.conn_tx.lock().unwrap().insert(conn_id, tx.clone());
    let stats = ctx.stats.clone();
    let writer = tokio::spawn(async move {
        // 队列里有消息就写出去；写失败（对方已断开）或队列关闭则退出
        while let Some(mut buf) = rx.recv().await {
            // 回报记录号 ReportIndex 在真实发送时分配并补写进报文：
            // 分配时机与线上发送顺序严格一致，多笔并发回报（含延迟回报）也不会乱序；
            // 非回报消息（Logon/心跳/业务拒绝/平台状态等）跳过，不占回报序号。
            // 记录号按分区连续编号（规范 3.15：每个分区从 1 开始），
            // 分区号从帧偏移 8 处解析（所有回报消息体统一以 PartitionNo 开头）。
            // 帧里 ReportIndex 非 0 说明是“回报同步重发的历史帧”（重发时保留
            // 原记录号让柜台对账），跳过重新分配，避免同一条回报重复占号。
            if buf.len() >= 20 {
                let mt = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                if protocol::is_report_frame(mt) {
                    if protocol::frame_report_index(&buf) == 0 {
                        let partition = protocol::frame_partition(&buf);
                        protocol::patch_report_index(&mut buf, stats.next_report_index_for(partition));
                    }
                }
            }
            // 发送前捕获（补号已完成，捕获字节与线上完全一致），
            // 并同步解析字段：头 8 字节消息类型，消息体在 [8..len-4]
            if let Some(r) = &rec_send {
                let fields = if buf.len() >= 12 {
                    let mt = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                    protocol::describe_fields(mt, &buf[8..buf.len() - 4])
                } else {
                    Vec::new()
                };
                r.record_send(&buf, fields);
                // 回报帧同时登记进“重发缓存”：回报同步请求按 begin 重发时用
                // （重发帧同 (分区, 记录号) 覆盖，不会重复登记）
                if buf.len() >= 20 {
                    let mt = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                    if protocol::is_report_frame(mt) {
                        r.record_report(
                            protocol::frame_partition(&buf),
                            protocol::frame_report_index(&buf),
                            &buf,
                        );
                    }
                }
            }
            if wh.write_all(&buf).await.is_err() {
                break;
            }
        }
        let _ = wh.shutdown().await;
    });

    // 登记连接：计入累计连接数，并加入存活连接表（前端可以看到）
    ctx.stats.total_connections.fetch_add(1, Ordering::Relaxed);
    ctx.connections.lock().unwrap().insert(
        conn_id,
        ConnInfo {
            id: conn_id,
            peer: peer.to_string(),
            comp_id: String::new(),
            logged_on: false,
            since: chrono::Local::now().format("%H:%M:%S").to_string(),
        },
    );
    ctx.log("info", format!("柜台 {} 已连接，等待 Logon", peer));

    let mut hb_secs: u64 = 30; // 心跳间隔（秒），Logon 时以对方要求为准
    let mut logged_on = false; // 是否已登录（未登录前拒绝处理业务消息）
    let mut hb_task: Option<tokio::task::JoinHandle<()>> = None; // 心跳发送任务句柄
    // 延迟回报任务登记表：回报存在延迟时用独立任务“睡眠+发送”，该任务持有
    // 发送通道的 clone；会话结束必须把它们全部 abort 并等其释放发送端，
    // 否则 writer.await 会一直等队列关闭（见下方清理段注释）。
    // 用 parking_lot 锁：取锁即得 guard，无中毒路径（任务登记不允许失败）
    let report_tasks: Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));

    // 主循环：一次处理一条报文，直到连接结束
    loop {
        // 空闲超时 = 心跳间隔 × 3（宽容两次丢失），且不低于 10 秒
        let idle = Duration::from_secs((hb_secs * 3).max(10));
        // select! 同时等两件事：网关停止信号 / 下一条报文，谁先来处理谁
        let frame = tokio::select! {
            _ = shutdown.changed() => {
                // 网关停止：主动注销
                let _ = tx.send(Logout { session_status: 4, text: "网关停止".into() }.encode()).await;
                break;
            }
            r = read_frame(&mut rh, idle) => match r {
                Ok(f) => f,
                Err(e) => {
                    ctx.log("warn", format!("柜台 {} 连接断开: {}", peer, e));
                    break;
                }
            }
        };
        let (mt, body, cks_ok, raw) = frame;
        // 捕获收到的报文（校验和错误的也记录，便于排查），同时按字段解析
        if let Some(r) = &recorder {
            r.record_recv(&raw, protocol::describe_fields(mt, &body));
        }
        // 校验和不对：数据可能在传输中损坏，丢弃这一条，继续收下一条
        if !cks_ok {
            ctx.log("warn", format!("柜台 {} 消息校验和错误 MsgType={}，已忽略", peer, mt));
            continue;
        }

        // 按消息类型分发处理
        match mt {
            msg_type::LOGON => {
                let logon = match Logon::decode(&body) {
                    Ok(l) => l,
                    Err(e) => {
                        ctx.log("error", format!("Logon 消息解析失败: {}", e));
                        break;
                    }
                };
                // 若平台配置开启了密码校验，密码不符则回 Logout(状态5) 并断开
                if ctx.cfg.check_password && logon.password != ctx.cfg.password {
                    ctx.log("warn", format!("柜台 {} 登录密码错误，拒绝登录", peer));
                    let _ = tx
                        .send(Logout { session_status: 5, text: "不合法的用户名或口令".into() }.encode())
                        .await;
                    break;
                }
                // 心跳间隔采用对方 Logon 里的要求，限制在 1~300 秒的合理范围；
                // 对端填 0 表示未指定（协议约定），用默认 30 秒，
                // 否则 0 会被钳成 1 秒，正常柜台偶发延迟就会被 3 秒超时误断
                hb_secs = if logon.heart_bt_int == 0 {
                    30
                } else {
                    logon.heart_bt_int.clamp(1, 300) as u64
                };
                // 回复 Logon 确认登录（真实交易所的握手流程也是如此），
                // 心跳间隔回填实际生效值（含 0→默认 30 的归一化），双方一致
                let reply = Logon {
                    sender_comp_id: ctx.cfg.comp_id.clone(),
                    target_comp_id: logon.sender_comp_id.clone(),
                    heart_bt_int: hb_secs as i32,
                    password: String::new(),
                    default_appl_ver_id: if logon.default_appl_ver_id.is_empty() {
                        "1.29".into()
                    } else {
                        logon.default_appl_ver_id.clone()
                    },
                };
                let _ = tx.send(reply.encode()).await;
                // 登录成功后下发平台信息、平台状态（状态 2 = 开放，可以报单）。
                // 平台信息携带全部分区号（支持多分区配置），OMS 按它初始化分区
                let _ = tx
                    .send(protocol::encode_platform_info(
                        ctx.cfg.platform_type,
                        &ctx.cfg.partitions(),
                    ))
                    .await;
                let _ = tx
                    .send(protocol::encode_platform_state(ctx.cfg.platform_type, 2))
                    .await;
                // 5.6 交易会话状态：目前仅固定收益交易平台（平台号 6）提供
                // 本消息（表 5-7），登录后下发一条，揭示当前所处交易会话
                if ctx.cfg.platform_type == 6 {
                    let _ = tx.send(encode_session_status()).await;
                }
                logged_on = true;
                if let Some(c) = ctx.connections.lock().unwrap().get_mut(&conn_id) {
                    c.comp_id = logon.sender_comp_id.clone();
                    c.logged_on = true;
                }
                ctx.log(
                    "info",
                    format!(
                        "柜台 {} 登录成功 CompID={} 心跳间隔={}s",
                        peer, logon.sender_comp_id, hb_secs
                    ),
                );
                // 启动心跳定时任务：每隔 hb_secs 秒给柜台发一条心跳。
                // 若柜台重复 Logon，先停掉旧任务，避免出现两个心跳源
                if let Some(h) = hb_task.take() {
                    h.abort();
                }
                let hb_tx = tx.clone();
                hb_task = Some(tokio::spawn(async move {
                    let mut iv = tokio::time::interval(Duration::from_secs(hb_secs));
                    iv.tick().await; // 跳过立即触发的第一次
                    loop {
                        iv.tick().await;
                        if hb_tx.send(protocol::encode_heartbeat()).await.is_err() {
                            break;
                        }
                    }
                }));
            }
            msg_type::HEARTBEAT => {
                // 收到心跳不需要任何回应——它的作用已经起到了：
                // read_frame 每收到一条报文就重新开始计时，空闲超时被刷新
            }
            msg_type::LOGOUT => {
                // 柜台主动注销：回一条确认后结束会话
                let status = Logout::decode(&body).map(|l| l.session_status).unwrap_or(0);
                ctx.log("info", format!("柜台 {} 请求注销（状态={}）", peer, status));
                let _ = tx
                    .send(Logout { session_status: 4, text: "会话退登完成".into() }.encode())
                    .await;
                break;
            }
            msg_type::REPORT_SYNC => {
                // 回报同步请求（5.2）：OMS 登录后告知各分区期望接收的下一条回报记录号。
                // 处理流程：校验分区号（非法/重复则回业务拒绝 20106）→ 按 begin 重发该分区历史回报。
                // 注意：按实测真实交易所行为，发完历史回报后不回“回报结束(7)”，也没有其它结束标记，
                // 柜台按回报记录号自行对账（详见下方 for 循环内的注释）。
                match ReportSync::decode(mt, &body) {
                    Ok(rs) => {
                        // 分区号须存在且不重复（规范 3.15：含非法分区号的同步消息
                        // 会被丢弃，OMS 收到 20106 业务拒绝后必须重新发送同步消息，
                        // 否则收不到回报）
                        let valid = ctx.cfg.partitions();
                        let mut seen: HashSet<i32> = HashSet::new();
                        let mut bad: Vec<i32> = Vec::new();
                        for p in &rs.partitions {
                            if !valid.contains(&p.partition_no) || !seen.insert(p.partition_no) {
                                bad.push(p.partition_no);
                            }
                        }
                        if !bad.is_empty() {
                            ctx.log(
                                "warn",
                                format!(
                                    "柜台 {} 回报同步请求含非法/重复分区号 {:?}，回业务拒绝(20106)",
                                    peer, bad
                                ),
                            );
                            let rej = BusinessReject {
                                appl_id: String::new(),
                                transact_time: protocol::now_timestamp(),
                                submitting_pbu_id: String::new(),
                                security_id: String::new(),
                                security_id_source: String::new(),
                                ref_seq_num: 0,
                                ref_msg_type: msg_type::REPORT_SYNC,
                                business_reject_ref_id: String::new(),
                                business_reject_reason: 20106,
                                business_reject_text: "回报同步消息含非法分区号".into(),
                            };
                            let _ = tx.send(rej.encode()).await;
                            ctx.stats.business_rejects.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        for p in &rs.partitions {
                            // 从该记录号开始重发本分区历史回报（含历史连接发过的；
                            // 未开启报文捕获时无缓存，重发 0 条即功能关闭）。
                            // 按实测真实交易所行为：发完历史回报后不回“回报结束(7)”，
                            // 也没有其它结束标记，柜台按回报记录号自行对账
                            let n = resend_reports_since(&ctx, &tx, p.partition_no, p.report_index)
                                .await;
                            if n > 0 {
                                ctx.log(
                                    "info",
                                    format!(
                                        "按回报同步请求重发分区 {} 的历史回报 {} 条",
                                        p.partition_no, n
                                    ),
                                );
                            }
                        }
                        ctx.log(
                            "info",
                            format!(
                                "柜台 {} 回报同步完成（{} 个分区）",
                                peer,
                                rs.partitions.len()
                            ),
                        );
                    }
                    Err(e) => {
                        ctx.log("warn", format!("回报同步请求解析失败: {}", e));
                    }
                }
            }
            // “if logged_on”是匹配守卫：只有登录后才接受委托/撤单，
            // 未登录时会落入下面的 other 分支被忽略
            // 4.5.1 新订单：28 种业务消息类型统一分发（m 绑定实际消息类型，
            // handle_new_order 按业务解码委托并选回报报文类型）
            m if logged_on && NewOrder::is_new_order(m) => {
                handle_new_order(m, &ctx, &tx, &body, conn_id, &report_tasks).await;
            }
            msg_type::ORDER_CANCEL_REQUEST if logged_on => {
                handle_cancel(&ctx, &tx, &body, &report_tasks).await;
            }
            // 4.6.1 报价 / 4.6.3 报价回复：回 4.6.2 报价状态回报（表 4-67 注 1/2）
            m if logged_on && (Quote::is_quote(m) || QuoteResponse::is_quote_response(m)) => {
                handle_quote(m, &ctx, &tx, &body).await;
            }
            // 4.7.1 询价请求：回 4.7.2 询价请求响应
            m if logged_on && QuoteRequest::is_quote_request(m) => {
                handle_quote_request(m, &ctx, &tx, &body).await;
            }
            // 4.8.1 意向申报：回 4.8.2 意向申报响应
            m if logged_on && IndicationOfInterest::is_ioi(m) => {
                handle_ioi(m, &ctx, &tx, &body).await;
            }
            // 4.9.1 成交申报：回 4.9.2 成交申报响应
            m if logged_on && TradeCaptureReport::is_trade_capture_report(m) => {
                handle_tcr(m, &ctx, &tx, &body).await;
            }
            // 4.10.1 注册：回 4.10.2 注册执行报告
            m if logged_on && Designation::is_designation(m) => {
                handle_designation(m, &ctx, &tx, &body).await;
            }
            // 4.11.1 投票：回 4.11.2 投票执行报告
            m if logged_on && Evote::is_evote(m) => {
                handle_evote(m, &ctx, &tx, &body).await;
            }
            // 4.12.1 密码服务：回 4.12.2 密码服务执行报告
            m if logged_on && PasswordService::is_password_service(m) => {
                handle_password_service(m, &ctx, &tx, &body).await;
            }
            // 4.13.1 保证金查询：回 4.13.2 保证金查询结果
            m if logged_on && MarginQuery::is_margin_query(m) => {
                handle_margin_query(m, &ctx, &tx, &body).await;
            }
            // 4.14.1 多腿订单：回 4.14.2 多腿订单执行报告
            m if logged_on && MultilegOrder::is_multileg(m) => {
                handle_multileg(m, &ctx, &tx, &body).await;
            }
            other => {
                if !logged_on {
                    ctx.log("warn", format!("柜台 {} 未登录即发送消息 MsgType={}，忽略", peer, other));
                } else {
                    ctx.log("warn", format!("收到暂不支持的消息 MsgType={}，忽略", other));
                }
            }
        }
    }

    // ── 会话结束，开始清理 ──
    if let Some(h) = hb_task {
        h.abort(); // 停止心跳任务
    }
    // 先终止延迟回报任务：它们各自持有 tx 的 clone，不结束的话 writer 的
    // recv 永远等不到队列关闭，下面的 writer.await 会一直挂在这里，
    // connections 移除 / mark_dead 都执行不到（重连会被单连接限制拒绝）。
    // abort 后 await 等任务真正退出（释放发送端）再继续。
    let pending_tasks = std::mem::take(&mut *report_tasks.lock());
    for h in &pending_tasks {
        h.abort();
    }
    for h in pending_tasks {
        let _ = h.await;
    }
    // 先移除发送通道：它持有一份 tx clone，不先移除的话 writer 的 recv
    // 永远等不到队列关闭，writer.await 会一直挂在这里，后续的
    // connections 移除 / mark_dead 都执行不到（重连会被单连接限制拒绝）
    ctx.conn_tx.lock().unwrap().remove(&conn_id);
    drop(tx); // 关闭写通道发送端，writer 任务的 recv 返回 None 随之退出
    let _ = writer.await; // 等 writer 把队列里剩余消息（如 Logout）发完再走
    ctx.connections.lock().unwrap().remove(&conn_id); // 从存活连接表移除
    // 记录器保留不删：平台级报文弹窗要能按连接分组回看本连接的历史报文，
    // 这里只标记为“已离线”（分组标题上显示）
    if let Some(r) = &recorder {
        r.mark_dead();
    }
    ctx.log("info", format!("柜台 {} 会话结束", peer));
}

/// 回报同步重发：把本平台所有连接（含已断开的——记录器保留不删）捕获缓存里、
/// 指定分区“记录号 >= begin”的回报帧，按记录号升序经当前连接重发。
/// 返回重发条数。重发帧保留原 ReportIndex（writer 侧见记录号非 0 不再重新
/// 分配），柜台按 (分区, 记录号) 对账；上交所重发帧的 MsgSeqNum 仍由 writer
/// 按当前连接重新补号（会话序号，与回报记录号无关）。
/// 未开启报文捕获时没有缓存，返回 0（重发功能随捕获开关一起关闭）。
/// 跨平台隔离：ctx.recorders 只登记本平台的连接，别的平台的历史回报取不到。
/// 三套协议共用（shjj/shbond 复用 sz::session 的 SessionCtx 与 ConnRecorder）。
pub(crate) async fn resend_reports_since(
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    partition: i32,
    begin: i64,
) -> usize {
    let mut frames: BTreeMap<i64, Vec<u8>> = BTreeMap::new();
    {
        let recorders = ctx.recorders.lock().unwrap();
        for rec in recorders.values() {
            for (idx, frame) in rec.reports_since(partition, begin) {
                // 同一 (分区, 记录号) 只可能来自一条连接；合并去重保险
                frames.insert(idx, frame);
            }
        }
    }
    let n = frames.len();
    for (_, frame) in frames {
        if tx.send(frame).await.is_err() {
            break; // 连接已断开，剩余不再重发
        }
    }
    n
}

/// 处理新订单（4.5.1，MsgType=1xxx01，共 28 种业务消息类型）：核心业务入口。
///
/// 流程：按消息类型解析委托 → 计入统计 → 让 strategy 模块按业务特征表
/// （BizInfo）生成“回报计划”（每条含：延迟毫秒数 + 编码好的报文 + 类型 + 描述）
/// → 按计划逐条发送给柜台。
async fn handle_new_order(
    mt: u32,
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
    conn_id: u64,
    report_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) {
    let order = match NewOrder::decode(mt, body) {
        Ok(o) => o,
        Err(e) => {
            ctx.log("error", format!("新订单({})解析失败: {}", mt, e));
            return;
        }
    };
    // 平台校验（表 3-1/表 3-3）：委托的 ApplID 必须属于当前接入平台，
    // 否则回 20108 业务拒绝（参照 tgw_error.csv），订单不进入受理流程
    if !reject_platform(
        ctx,
        tx,
        mt,
        &order.common.appl_id,
        &order.common.cl_ord_id,
        &order.common.submitting_pbu_id,
        &order.common.security_id,
        &order.common.security_id_source,
    )
    .await
    {
        return;
    }
    ctx.stats.orders.fetch_add(1, Ordering::Relaxed);
    // 日志里把协议的放大整数还原成人类可读的值：价格÷10000，数量÷100
    ctx.log(
        "info",
        format!(
            "收到委托 MsgType={} ApplID={} ClOrdID={} 证券={} 方向={} 价格={:.4} 数量={}",
            mt,
            order.common.appl_id,
            order.common.cl_ord_id,
            order.common.security_id,
            side_name(order.common.side),
            order.common.price as f64 / 10000.0,
            order.common.order_qty / 100
        ),
    );

    // 缓存订单：登记一条新订单（状态=已报），回报逐条发出时再更新状态，
    // 这样撤单请求到达时能查到订单的真实当前状态；conn_id 记录订单来自
    // 哪个连接（手动回复成交/拒单/撤单时按它定位发送通道）
    if let Some(ob) = &ctx.orders {
        ob.add(OrderEntry {
            cl_ord_id: order.common.cl_ord_id.clone(),
            order_id: String::new(),
            security_id: order.common.security_id.clone(),
            side: side_name(order.common.side).to_string(),
            price: order.common.price as f64 / 10000.0,
            qty: order.common.order_qty as f64 / 100.0,
            cum_qty: 0.0,
            leaves_qty: order.common.order_qty as f64 / 100.0,
            status: OrderStatus::New,
            ord_type: order.common.ord_type,
            account: order.common.account_id.clone(),
            branch: order.common.branch_id.clone(),
            ts: chrono::Local::now().format("%H:%M:%S").to_string(),
            conn_id,
            // 协议回填字段：手动回复回报时与自动回报保持一致
            // （深市无 Pbu 分区机制，pbu 存申报交易单元，回报两字段共用）
            pbu: order.common.submitting_pbu_id.clone(),
            biz_id: 0,
            biz_pbu: String::new(),
            owner_type: order.common.owner_type,
            credit_tag: String::new(),
            clearing_firm: order.common.clearing_firm.clone(),
            user_info: order.common.user_info.clone(),
            // 业务标识 = 委托的 ApplID：撤单成功/手动回复按它反查业务特征
            biz: order.common.appl_id.clone(),
        });
    }

    // 生成回报计划（具体几条确认/成交、延迟多少，由运行时策略配置决定；
    // 策略支持热更新，每次委托都读取最新值，改动即时生效）
    // 注意：策略读锁只在生成计划期间持有，不跨越 await 点——
    // RwLockReadGuard 不是 Send，跨 await 存活会阻止会话任务在线程间调度
    let plans = {
        let st = ctx.strategy.read().unwrap();
        strategy::plan_reports(&st, ctx.cfg.partition_for(&order.common.security_id), &order, &ctx.stats)
    };
    // 所有回报延迟都为 0 时可以当场发完；否则需要另起任务慢慢发
    let all_sync = plans.iter().all(|p| p.delay_ms == 0);
    let ctx2 = ctx.clone();
    let tx2 = tx.clone();
    // 把“逐条发送”的逻辑包成一个异步代码块，下面再决定怎么跑它
    let fut = async move {
        for p in plans {
            if p.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(p.delay_ms)).await;
            }
            // 发送前检查订单缓存：订单已到终态（如延迟期间被撤单成功/已成交/已拒）
            // 时，丢弃尚未发出的回报——撤单成功后不再补发此前规划的确认/成交，
            // 保证线上回报序列与订单状态一致（撤单成功回报本身不受此限）
            if p.kind != ReportKind::Cancel {
                if let Some(ob) = &ctx2.orders {
                    if ob
                        .find(&p.cl_ord_id)
                        .is_some_and(|o| o.status.is_terminal())
                    {
                        continue;
                    }
                }
            }
            if tx2.send(p.frame).await.is_err() {
                break; // 连接已断开，剩余回报不必再发
            }
            // 按回报类型累加对应的统计计数（前端统计面板的数据来源）
            match p.kind {
                ReportKind::Ack => ctx2.stats.acks.fetch_add(1, Ordering::Relaxed),
                ReportKind::Trade => ctx2.stats.trades.fetch_add(1, Ordering::Relaxed),
                ReportKind::Reject => ctx2.stats.order_rejects.fetch_add(1, Ordering::Relaxed),
                ReportKind::BusinessReject => {
                    ctx2.stats.business_rejects.fetch_add(1, Ordering::Relaxed)
                }
                // 撤单成功回报不占统计计数（那是撤单请求/成交/拒单的计数）
                ReportKind::Cancel => 0,
            };
            // 回报真实发出后，把该笔回报对应的订单状态同步进缓存
            // （计划里带着委托编号，对号入座）
            if let Some(upd) = &p.order_update {
                if let Some(ob) = &ctx2.orders {
                    ob.apply(&p.cl_ord_id, upd);
                }
            }
            ctx2.log("info", format!("发送 {}", p.desc));
        }
    };
    if all_sync {
        // 延迟全为 0：在当前消息处理流程内同步发送，
        // 保证回报一定排在下一笔委托的回报之前（严格保序）
        fut.await;
    } else {
        // 存在延迟：交给独立任务去睡眠+发送，主循环立即继续
        // 接收下一笔委托，不会被延迟卡住；句柄登记到会话级任务表，
        // 会话结束时统一 abort（防止任务持有发送端导致清理挂起）
        let h = tokio::spawn(fut);
        report_tasks.lock().push(h);
    }
}

/// 处理撤单请求(190007)。
///
/// 撤单自动回报模式（StrategyConfig.cancel_mode）：
/// - 默认模式：开启“缓存订单”时按原单状态回复——原单在途（已报/部分成交）
///   → 撤单成功（执行报告 ExecType=4 已撤）；找不到或已是终态 → 撤单拒单
/// - 回撤单拒单模式：无论原单状态一律撤单拒单
/// 撤单拒单默认回撤单失败响应(290008)；勾选“前台拒单”（cancel_front_reject）
/// 时改发业务拒绝消息(4)，原因代码取 cancel_reject_reason。
/// 撤单成功与拒单回报都按 cancel_delay 延迟发送（0 = 同步）。
async fn handle_cancel(
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
    report_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) {
    let req = match OrderCancelRequest::decode(body) {
        Ok(r) => r,
        Err(e) => {
            ctx.log("error", format!("撤单请求(190007)解析失败: {}", e));
            return;
        }
    };
    // 平台校验：撤单请求的 ApplID 属于当前平台才受理；空 ApplID 无法判断
    // 所属平台，放行由订单缓存兜底。不符回 20108 业务拒绝（参照 tgw_error.csv）
    if !req.appl_id.trim().is_empty()
        && !reject_platform(
            ctx,
            tx,
            msg_type::ORDER_CANCEL_REQUEST,
            &req.appl_id,
            &req.cl_ord_id,
            &req.submitting_pbu_id,
            &req.security_id,
            &req.security_id_source,
        )
        .await
    {
        return;
    }
    ctx.stats.cancels.fetch_add(1, Ordering::Relaxed);
    // 撤单自动回报配置在块作用域内读取（读锁不跨 await 存活）
    let (cancel_mode, cancel_reason, front_reject, delay_ms) = {
        let st = ctx.strategy.read().unwrap();
        (
            st.cancel_mode,
            st.cancel_reject_reason,
            st.cancel_front_reject,
            st.cancel_delay.sample(),
        )
    };
    // 回撤单拒单模式：不按原单状态，一律拒单
    if cancel_mode == CancelMode::AlwaysReject {
        send_cancel_reject(ctx, tx, &req, cancel_reason, front_reject, delay_ms, report_tasks)
            .await;
        return;
    }

    // ---- 全部撤单成功模式：无论有无缓存/原单状态，一律回撤单成功 ----
    if cancel_mode == CancelMode::AlwaysSuccess {
        // 在途订单标记为已撤（缓存一致性）；无缓存/终态/找不到不改变缓存
        let entry = ctx.orders.as_ref().and_then(|ob| ob.find(&req.orig_cl_ord_id));
        if let (Some(ob), Some(en)) = (&ctx.orders, &entry) {
            if en.status.is_inflight() {
                ob.cancel_inflight(&req.orig_cl_ord_id);
            }
        }
        let e = entry.as_ref();
        // 撤单成功回报：按原单业务（订单缓存里存的 ApplID）反查确认执行
        // 报告报文类型（表 4-30：各业务撤单成功回各自的 2xxx02），
        // ExecType=4 / OrdStatus=4（已撤）；查不到业务/订单时按现货竞价兜底
        let biz = e
            .and_then(|x| biz_info_by_appl_id(&x.biz))
            .unwrap_or_else(|| strategy::biz_info(msg_type::NEW_ORDER_CASH));
        let cxl = ExecRptAck {
            msg_type: biz.ack_msg_type,
            partition_no: ctx.cfg.partition_for(&req.security_id),
            report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
            appl_id: if req.appl_id.is_empty() {
                biz.appl_id.into()
            } else {
                req.appl_id.clone()
            },
            reporting_pbu_id: req.submitting_pbu_id.clone(),
            submitting_pbu_id: req.submitting_pbu_id.clone(),
            security_id: req.security_id.clone(),
            security_id_source: if req.security_id_source.is_empty() {
                "102".into()
            } else {
                req.security_id_source.clone()
            },
            owner_type: req.owner_type,
            clearing_firm: req.clearing_firm.clone(),
            transact_time: protocol::now_timestamp(),
            user_info: req.user_info.clone(),
            // 订单号/价格/数量回填原单的值（缓存里存的自然单位再放大回协议整数）；
            // 找不到原单时用撤单请求带的字段/默认值兜底
            order_id: e
                .map(|x| x.order_id.clone())
                .unwrap_or_else(|| req.order_id.clone()),
            cl_ord_id: req.cl_ord_id.clone(),
            orig_cl_ord_id: req.orig_cl_ord_id.clone(),
            exec_id: ctx.stats.next_exec_id(),
            exec_type: exec_type::CANCELLED,
            ord_status: ord_status::CANCELLED,
            ord_rej_reason: 0,
            leaves_qty: 0,
            cum_qty: e.map(|x| (x.cum_qty * 100.0).round() as i64).unwrap_or(0),
            side: req.side,
            ord_type: e.map(|x| x.ord_type).unwrap_or(0),
            order_qty: e.map(|x| (x.qty * 100.0) as i64).unwrap_or(req.order_qty),
            price: e.map(|x| (x.price * 10000.0) as i64).unwrap_or(0),
            account_id: e.map(|x| x.account.clone()).unwrap_or_default(),
            branch_id: e.map(|x| x.branch.clone()).unwrap_or_default(),
            order_restrictions: String::new(),
            // 撤单成功回报无业务扩展字段（表 4-30 扩展仅订单响应使用）
            extend: Default::default(),
        };
        send_cancel_report(tx, cxl.encode(), delay_ms, report_tasks).await;
        ctx.log(
            "info",
            format!(
                "收到撤单请求 ClOrdID={} OrigClOrdID={}，全部撤单成功模式，已回撤单成功({})",
                req.cl_ord_id, req.orig_cl_ord_id, biz.ack_msg_type
            ),
        );
        return;
    }

    // ---- 默认模式：有订单缓存先尝试按原单状态撤单 ----
    if let Some(ob) = &ctx.orders {
        // cancel_inflight 只在原单存在且为在途时返回 true（并已置为已撤）
        if ob.cancel_inflight(&req.orig_cl_ord_id) {
            let entry = ob.find(&req.orig_cl_ord_id).expect("刚撤单成功的订单必然存在");
            // 撤单成功回报：按原单业务（订单缓存里存的 ApplID）反查确认执行
            // 报告报文类型（表 4-30：各业务撤单成功回各自的 2xxx02），
            // ExecType=4 / OrdStatus=4（已撤）；查不到业务时按现货竞价兜底
            let biz = biz_info_by_appl_id(&entry.biz)
                .unwrap_or_else(|| strategy::biz_info(msg_type::NEW_ORDER_CASH));
            let cxl = ExecRptAck {
                msg_type: biz.ack_msg_type,
                partition_no: ctx.cfg.partition_for(&req.security_id),
                report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
                appl_id: if req.appl_id.is_empty() {
                    biz.appl_id.into()
                } else {
                    req.appl_id.clone()
                },
                reporting_pbu_id: req.submitting_pbu_id.clone(),
                submitting_pbu_id: req.submitting_pbu_id.clone(),
                security_id: req.security_id.clone(),
                security_id_source: if req.security_id_source.is_empty() {
                    "102".into()
                } else {
                    req.security_id_source.clone()
                },
                owner_type: req.owner_type,
                clearing_firm: req.clearing_firm.clone(),
                transact_time: protocol::now_timestamp(),
                user_info: req.user_info.clone(),
                // 订单号/价格/数量回填原单的值（缓存里存的自然单位再放大回协议整数）
                order_id: entry.order_id.clone(),
                cl_ord_id: req.cl_ord_id.clone(),
                orig_cl_ord_id: req.orig_cl_ord_id.clone(),
                exec_id: ctx.stats.next_exec_id(),
                exec_type: exec_type::CANCELLED,
                ord_status: ord_status::CANCELLED,
                ord_rej_reason: 0,
                leaves_qty: 0,
                cum_qty: (entry.cum_qty * 100.0).round() as i64,
                side: req.side,
                ord_type: entry.ord_type,
                order_qty: (entry.qty * 100.0) as i64,
                price: (entry.price * 10000.0) as i64,
                account_id: entry.account.clone(),
                branch_id: entry.branch.clone(),
                order_restrictions: String::new(),
                // 撤单成功回报无业务扩展字段（表 4-30 扩展仅订单响应使用）
                extend: Default::default(),
            };
            send_cancel_report(tx, cxl.encode(), delay_ms, report_tasks).await;
            ctx.log(
                "info",
                format!(
                    "收到撤单请求 ClOrdID={} OrigClOrdID={}，原单在途，已回撤单成功({})",
                    req.cl_ord_id, req.orig_cl_ord_id, biz.ack_msg_type
                ),
            );
            return;
        }
        // 原单存在但已是终态：不在这里记录“撤单失败”（日志统一走下面）
    }

    // ---- 撤单拒单：未开启缓存 / 找不到原单 / 原单已是终态 ----
    send_cancel_reject(ctx, tx, &req, cancel_reason, front_reject, delay_ms, report_tasks).await;
}

/// 构造并发送撤单拒单：默认回撤单失败响应(290008，原因码可配置)；
/// 勾选“前台拒单”时改发业务拒绝消息(4，原因码填 BusinessRejectReason)。
/// 按撤单回报延迟发送（延迟由调用方已抽样好）。
async fn send_cancel_reject(
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    req: &OrderCancelRequest,
    reason: u16,
    front_reject: bool,
    delay_ms: u64,
    report_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) {
    if front_reject {
        // 前台拒单：改发业务拒绝消息（表 5-2：撤单请求的业务层 ID = ClOrdID）
        let rej = BusinessReject {
            appl_id: if req.appl_id.is_empty() { "010".into() } else { req.appl_id.clone() },
            transact_time: protocol::now_timestamp(),
            submitting_pbu_id: req.submitting_pbu_id.clone(),
            security_id: req.security_id.clone(),
            security_id_source: req.security_id_source.clone(),
            ref_seq_num: 0,
            ref_msg_type: msg_type::ORDER_CANCEL_REQUEST,
            business_reject_ref_id: req.cl_ord_id.clone(),
            business_reject_reason: reason,
            business_reject_text: "撤单被拒".into(),
        };
        send_cancel_report(tx, rej.encode(), delay_ms, report_tasks).await;
        ctx.stats.business_rejects.fetch_add(1, Ordering::Relaxed);
        ctx.log(
            "info",
            format!(
                "收到撤单请求 ClOrdID={} OrigClOrdID={}，回业务拒绝(4) 原因={}",
                req.cl_ord_id, req.orig_cl_ord_id, reason
            ),
        );
        return;
    }
    // 撤单失败响应（290008）：大部分字段直接回填请求里的值（回报要能对得上号）
    let rej = CancelReject {
        partition_no: ctx.cfg.partition_for(&req.security_id),
        report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
        appl_id: if req.appl_id.is_empty() { "010".into() } else { req.appl_id.clone() },
        reporting_pbu_id: req.submitting_pbu_id.clone(),
        submitting_pbu_id: req.submitting_pbu_id.clone(),
        security_id: req.security_id.clone(),
        security_id_source: req.security_id_source.clone(),
        owner_type: req.owner_type,
        clearing_firm: req.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: req.user_info.clone(),
        cl_ord_id: req.cl_ord_id.clone(),
        orig_cl_ord_id: req.orig_cl_ord_id.clone(),
        side: req.side,
        ord_status: b'8', // '8' = 已拒绝（未找到原始订单或原单不可撤）
        cxl_rej_reason: reason,
        reject_text: "SIM NO ORDER".into(),
        order_id: req.order_id.clone(),
    };
    send_cancel_report(tx, rej.encode(), delay_ms, report_tasks).await;
    ctx.log(
        "info",
        format!(
            "收到撤单请求 ClOrdID={} OrigClOrdID={}，原单不存在或不可撤，已回撤单失败响应(290008) 原因={}",
            req.cl_ord_id, req.orig_cl_ord_id, reason
        ),
    );
}

/// 按撤单回报延迟发送一条回报：0 延迟立即投递；否则另起任务 sleep 后投递
/// （任务登记到会话级 report_tasks，会话结束时统一 abort，防止持有发送端
/// 阻塞 writer 退出）。撤单成功与撤单拒单回报统一走这里；三套协议共用。
pub(crate) async fn send_cancel_report(
    tx: &mpsc::Sender<Vec<u8>>,
    frame: Vec<u8>,
    delay_ms: u64,
    report_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) {
    if delay_ms == 0 {
        let _ = tx.send(frame).await;
        return;
    }
    let tx2 = tx.clone();
    let h = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let _ = tx2.send(frame).await;
    });
    report_tasks.lock().push(h);
}

/// 业务拒绝原因文本（tgw_error.csv 20108：平台非法-委托申报的平台错误，
/// 例如向现货集中竞价平台申报发行认购业务的委托）。字段宽 50 字节，
/// 超长部分由编码器按字符边界自动截断。
const BUSINESS_REJECT_PLATFORM_TEXT: &str =
    "平台非法-委托申报的平台错误,例如向现货集中竞价平台申报发行认购业务的委托";

/// 平台校验（表 3-1/表 3-3）：申报业务的 ApplID 必须属于当前接入平台。
///
/// 返回 true = 平台相符可继续处理；false = 平台不符或 ApplID 未知，已回
/// 业务拒绝消息（MsgType=4，BusinessRejectReason=20108，文本参照
/// tgw_error.csv）并计入业务拒绝统计，调用方应放弃处理本条申报。
///
/// 各申报消息（新订单/撤单/报价/询价/意向/成交申报/注册/投票/密码服务/
/// 保证金查询/多腿订单）统一走本函数，避免重复实现。
async fn reject_platform(
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    mt: u32,
    appl_id: &str,
    ref_id: &str,
    submitting_pbu_id: &str,
    security_id: &str,
    security_id_source: &str,
) -> bool {
    let ok = strategy::platform_of_appl_id(appl_id) == Some(ctx.cfg.platform_type);
    if ok {
        return true;
    }
    let rej = BusinessReject {
        appl_id: appl_id.trim_end().to_string(),
        transact_time: protocol::now_timestamp(),
        submitting_pbu_id: submitting_pbu_id.to_string(),
        security_id: security_id.to_string(),
        security_id_source: security_id_source.to_string(),
        ref_seq_num: 0,
        ref_msg_type: mt,
        business_reject_ref_id: ref_id.to_string(),
        business_reject_reason: 20108,
        business_reject_text: BUSINESS_REJECT_PLATFORM_TEXT.into(),
    };
    let _ = tx.send(rej.encode()).await;
    ctx.stats.business_rejects.fetch_add(1, Ordering::Relaxed);
    let belong = strategy::platform_of_appl_id(appl_id)
        .map(|p| p.to_string())
        .unwrap_or_else(|| "未知".into());
    ctx.log(
        "warn",
        format!(
            "业务拒绝(4) MsgType={} ApplID={} RefID={}：业务属于平台{}，当前接入平台{}（20108 {}",
            mt,
            appl_id.trim_end(),
            ref_id,
            belong,
            ctx.cfg.platform_type,
            BUSINESS_REJECT_PLATFORM_TEXT,
        ),
    );
    false
}

/// 处理报价（4.6.1）/报价回复（4.6.3）：平台校验后回 4.6.2 报价状态回报。
///
/// 报价被接受 → QuoteStatus=0（Accepted）；报价回复被接受也回 2xxx06
/// （表 4-67 注 1/2：作为报价回复的响应时，重复组取值同报价回复消息）。
async fn handle_quote(mt: u32, ctx: &SessionCtx, tx: &mpsc::Sender<Vec<u8>>, body: &[u8]) {
    if Quote::is_quote(mt) {
        let q = match Quote::decode(mt, body) {
            Ok(q) => q,
            Err(e) => {
                ctx.log("error", format!("报价({})解析失败: {}", mt, e));
                return;
            }
        };
        if !reject_platform(
            ctx,
            tx,
            mt,
            &q.appl_id,
            &q.quote_msg_id,
            &q.submitting_pbu_id,
            &q.security_id,
            &q.security_id_source,
        )
        .await
        {
            return;
        }
        // 接受报价：报价状态回报回填报价字段，扩展字段照抄请求
        let rpt = QuoteStatusReport {
            msg_type: QuoteStatusReport::response_msg_type(mt),
            partition_no: ctx.cfg.partition_for(&q.security_id),
            report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
            appl_id: q.appl_id.clone(),
            reporting_pbu_id: q.submitting_pbu_id.clone(),
            submitting_pbu_id: q.submitting_pbu_id.clone(),
            security_id: q.security_id.clone(),
            security_id_source: q.security_id_source.clone(),
            owner_type: q.owner_type,
            clearing_firm: q.clearing_firm.clone(),
            transact_time: protocol::now_timestamp(),
            user_info: q.user_info.clone(),
            quote_msg_id: q.quote_msg_id.clone(),
            account_id: q.account_id.clone(),
            quote_req_id: q.quote_req_id.clone(),
            quote_status: 0, // Accepted
            quote_reject_reason: 0,
            quote_type: q.quote_type,
            bid_px: q.bid_px,
            offer_px: q.offer_px,
            bid_size: q.bid_size,
            offer_size: q.offer_size,
            // 扩展字段：订单号/执行号由交易所分配
            bid_position_effect: q.bid_position_effect,
            offer_position_effect: q.offer_position_effect,
            contract_account_code: q.contract_account_code.clone(),
            branch_id: q.branch_id.clone(),
            order_id: ctx.stats.next_order_id(),
            exec_id: ctx.stats.next_exec_id(),
            quote_resp_id: q.quote_resp_id.clone(),
            private_quote: q.private_quote,
            side: 0,
            price_type: q.price_type,
            valid_until_time: q.valid_until_time,
            cash_margin: q.cash_margin,
            counterparty_pbu_id: q.counterparty_pbu_id.clone(),
            memo: q.memo.clone(),
            quote_reject_text: String::new(),
            member_id: q.member_id.clone(),
            investor_type: q.investor_type.clone(),
            investor_id: q.investor_id.clone(),
            investor_name: q.investor_name.clone(),
            trader_code: q.trader_code.clone(),
            settl_type: q.settl_type,
            settl_period: q.settl_period,
            pre_trade_anonymity: q.pre_trade_anonymity,
            max_floor: q.max_floor,
            min_qty: q.min_qty,
            no_counterparty: q.no_counterparty,
            counterparties: q.counterparties.clone(),
            // 作为报价申报的响应：NoQuote=1、QuoteID 填报价 QuoteID（注 1）
            no_quote: 1,
            quotes: vec![QuoteItem {
                quote_id: q.quote_id.clone(),
                quote_price: 0,
                quote_qty: 0,
            }],
        };
        let _ = tx.send(rpt.encode()).await;
        ctx.log(
            "info",
            format!(
                "收到报价({}) QuoteMsgID={} 证券={} 买价={:.4} 卖价={:.4}，已接受({})",
                mt,
                q.quote_msg_id,
                q.security_id,
                q.bid_px as f64 / 10000.0,
                q.offer_px as f64 / 10000.0,
                rpt.msg_type
            ),
        );
    } else {
        // 4.6.3 报价回复：回 2xxx06，重复组取值同报价回复消息（注 2）
        let q = match QuoteResponse::decode(mt, body) {
            Ok(q) => q,
            Err(e) => {
                ctx.log("error", format!("报价回复({})解析失败: {}", mt, e));
                return;
            }
        };
        if !reject_platform(
            ctx,
            tx,
            mt,
            &q.appl_id,
            &q.cl_ord_id,
            &q.submitting_pbu_id,
            &q.security_id,
            &q.security_id_source,
        )
        .await
        {
            return;
        }
        let rpt = QuoteStatusReport {
            msg_type: QuoteStatusReport::response_msg_type(mt),
            partition_no: ctx.cfg.partition_for(&q.security_id),
            report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
            appl_id: q.appl_id.clone(),
            reporting_pbu_id: q.submitting_pbu_id.clone(),
            submitting_pbu_id: q.submitting_pbu_id.clone(),
            security_id: q.security_id.clone(),
            security_id_source: q.security_id_source.clone(),
            owner_type: q.owner_type,
            clearing_firm: q.clearing_firm.clone(),
            transact_time: protocol::now_timestamp(),
            user_info: q.user_info.clone(),
            quote_msg_id: q.quotes.first().map(|i| i.quote_id.clone()).unwrap_or_default(),
            account_id: q.account_id.clone(),
            quote_req_id: String::new(),
            quote_status: 0, // Accepted
            quote_reject_reason: 0,
            quote_type: q.quote_type,
            bid_px: 0,
            offer_px: 0,
            bid_size: 0,
            offer_size: 0,
            branch_id: q.branch_id.clone(),
            order_id: ctx.stats.next_order_id(),
            exec_id: ctx.stats.next_exec_id(),
            quote_resp_id: q.quote_resp_id.clone(),
            private_quote: 0,
            side: q.side,
            price_type: q.price_type,
            valid_until_time: q.valid_until_time,
            cash_margin: q.cash_margin,
            memo: String::new(),
            member_id: q.member_id.clone(),
            investor_type: q.investor_type.clone(),
            investor_id: q.investor_id.clone(),
            investor_name: q.investor_name.clone(),
            trader_code: q.trader_code.clone(),
            settl_type: q.settl_type,
            settl_period: q.settl_period,
            no_quote: q.no_quote,
            quotes: q.quotes.clone(),
            ..Default::default()
        };
        let _ = tx.send(rpt.encode()).await;
        ctx.log(
            "info",
            format!(
                "收到报价回复({}) QuoteRespID={} ClOrdID={} 回复类型={}，已接受({})",
                mt,
                q.quote_resp_id,
                q.cl_ord_id,
                quote_resp_type_name(q.quote_resp_type),
                rpt.msg_type
            ),
        );
    }
}

/// 处理询价请求（4.7.1）：平台校验后回 4.7.2 询价请求响应（已接受）。
async fn handle_quote_request(
    mt: u32,
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
) {
    let q = match QuoteRequest::decode(mt, body) {
        Ok(q) => q,
        Err(e) => {
            ctx.log("error", format!("询价请求({})解析失败: {}", mt, e));
            return;
        }
    };
    if !reject_platform(
        ctx,
        tx,
        mt,
        &q.appl_id,
        &q.cl_ord_id,
        &q.submitting_pbu_id,
        &q.security_id,
        &q.security_id_source,
    )
    .await
    {
        return;
    }
    let rpt = QuoteRequestAck {
        msg_type: QuoteRequestAck::response_msg_type(mt),
        partition_no: ctx.cfg.partition_for(&q.security_id),
        report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
        appl_id: q.appl_id.clone(),
        reporting_pbu_id: q.submitting_pbu_id.clone(),
        submitting_pbu_id: q.submitting_pbu_id.clone(),
        security_id: q.security_id.clone(),
        security_id_source: q.security_id_source.clone(),
        owner_type: q.owner_type,
        clearing_firm: q.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: q.user_info.clone(),
        order_id: ctx.stats.next_order_id(),
        exec_id: ctx.stats.next_exec_id(),
        cl_ord_id: q.cl_ord_id.clone(),
        account_id: q.account_id.clone(),
        branch_id: q.branch_id.clone(),
        quote_req_id: q.quote_req_id.clone(),
        quote_request_trans_type: q.quote_request_trans_type,
        quote_request_type: 101, // Submit
        private_quote: q.private_quote,
        quote_request_status: 0, // Accepted
        quote_request_reject_reason: 0,
        order_qty: q.order_qty,
        price: q.price,
        side: q.side,
        expire_time: q.expire_time,
        quote_type: q.quote_type,
        quote_price_type: q.quote_price_type,
        memo: q.memo.clone(),
        // 扩展字段照抄请求（响应消息扩展同请求，表 4-79 注 2）
        cash_margin: q.cash_margin,
        no_counterparty_pbu: q.no_counterparty_pbu,
        counterparty_pbus: q.counterparty_pbus.clone(),
        member_id: q.member_id.clone(),
        investor_type: q.investor_type.clone(),
        investor_id: q.investor_id.clone(),
        investor_name: q.investor_name.clone(),
        trader_code: q.trader_code.clone(),
        settl_type: q.settl_type,
        settl_period: q.settl_period,
        pre_trade_anonymity: q.pre_trade_anonymity,
        quote_request_reject_text: String::new(),
        no_counterparty: q.no_counterparty,
        counterparties: q.counterparties.clone(),
    };
    let _ = tx.send(rpt.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到询价请求({}) QuoteReqID={} ClOrdID={} 证券={}，已接受({})",
            mt, q.quote_req_id, q.cl_ord_id, q.security_id, rpt.msg_type
        ),
    );
}

/// 处理意向申报（4.8.1）：平台校验后回 4.8.2 意向申报响应（已接受）。
async fn handle_ioi(mt: u32, ctx: &SessionCtx, tx: &mpsc::Sender<Vec<u8>>, body: &[u8]) {
    let q = match IndicationOfInterest::decode(mt, body) {
        Ok(q) => q,
        Err(e) => {
            ctx.log("error", format!("意向申报({})解析失败: {}", mt, e));
            return;
        }
    };
    if !reject_platform(
        ctx,
        tx,
        mt,
        &q.appl_id,
        &q.ioi_id,
        &q.submitting_pbu_id,
        &q.security_id,
        &q.security_id_source,
    )
    .await
    {
        return;
    }
    let rpt = IOIResponse {
        msg_type: msg_type::IOI_RESPONSE,
        partition_no: ctx.cfg.partition_for(&q.security_id),
        report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
        appl_id: q.appl_id.clone(),
        reporting_pbu_id: q.submitting_pbu_id.clone(),
        submitting_pbu_id: q.submitting_pbu_id.clone(),
        security_id: q.security_id.clone(),
        security_id_source: q.security_id_source.clone(),
        owner_type: q.owner_type,
        clearing_firm: q.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: q.user_info.clone(),
        quote_resp_id: ctx.stats.next_exec_id(), // 交易所意向申报响应编号
        quote_resp_type: 2,                     // 意向申报响应
        exec_type: 0,                           // New
        quote_reject_reason: 0,
        ioi_id: q.ioi_id.clone(),
        ioi_ref_id: q.ioi_ref_id.clone(),
        ioi_trans_type: q.ioi_trans_type,
        side: q.side,
        account_id: q.account_id.clone(),
        branch_id: q.branch_id.clone(),
        ioi_qty: q.ioi_qty,
        price: q.price,
        contactor: q.contactor.clone(),
        contact_info: q.contact_info.clone(),
    };
    let _ = tx.send(rpt.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到意向申报({}) IOIID={} 证券={} 方向={}，已接受({})",
            mt, q.ioi_id, q.security_id, side_name(q.side), rpt.msg_type
        ),
    );
}

/// 处理成交申报（4.9.1，11 种业务）：平台校验后回 4.9.2 成交申报响应（已接受）。
/// 扩展字段按原始字节原样回写（表 4-100 注 2），保证柜台对得上号。
async fn handle_tcr(mt: u32, ctx: &SessionCtx, tx: &mpsc::Sender<Vec<u8>>, body: &[u8]) {
    let tcr = match TradeCaptureReport::decode(mt, body) {
        Ok(t) => t,
        Err(e) => {
            ctx.log("error", format!("成交申报({})解析失败: {}", mt, e));
            return;
        }
    };
    if !reject_platform(
        ctx,
        tx,
        mt,
        &tcr.appl_id,
        &tcr.trade_report_id,
        &tcr.submitting_pbu_id,
        &tcr.security_id,
        &tcr.security_id_source,
    )
    .await
    {
        return;
    }
    let rpt = TcrAck {
        msg_type: TcrAck::response_msg_type(mt),
        partition_no: ctx.cfg.partition_for(&tcr.security_id),
        report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
        appl_id: tcr.appl_id.clone(),
        reporting_pbu_id: tcr.submitting_pbu_id.clone(),
        submitting_pbu_id: tcr.submitting_pbu_id.clone(),
        security_id: tcr.security_id.clone(),
        security_id_source: tcr.security_id_source.clone(),
        owner_type: tcr.owner_type,
        clearing_firm: tcr.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: tcr.user_info.clone(),
        trade_id: ctx.stats.next_order_id(), // 交易所成交申报编号
        trade_report_id: tcr.trade_report_id.clone(),
        trade_report_type: tcr.trade_report_type,
        trade_report_trans_type: tcr.trade_report_trans_type,
        trade_handling_instr: tcr.trade_handling_instr,
        trade_report_ref_id: tcr.trade_report_ref_id.clone(),
        trd_ack_status: 0,  // Accepted
        trd_rpt_status: 0,  // 接受
        trade_report_reject_reason: 0,
        last_px: tcr.last_px,
        last_qty: tcr.last_qty,
        trd_type: tcr.trd_type,
        trd_sub_type: tcr.trd_sub_type,
        confirm_id: tcr.confirm_id.clone(),
        exec_id: ctx.stats.next_exec_id(),
        side: tcr.side,
        pbu_id: tcr.pbu_id.clone(),
        account_id: tcr.account_id.clone(),
        branch_id: tcr.branch_id.clone(),
        counterparty_pbu_id: tcr.counterparty_pbu_id.clone(),
        counterparty_account_id: tcr.counterparty_account_id.clone(),
        counterparty_branch_id: tcr.counterparty_branch_id.clone(),
        trade_report_reject_text: String::new(),
        extend: tcr.extend.clone(), // 扩展字段原样回写
    };
    let _ = tx.send(rpt.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到成交申报({}) TradeReportID={} 证券={} 价格={:.4} 数量={}，已接受({})",
            mt,
            tcr.trade_report_id,
            tcr.security_id,
            tcr.last_px as f64 / 10000.0,
            tcr.last_qty / 100,
            rpt.msg_type
        ),
    );
}

/// 处理注册（4.10.1，转托管）：平台校验后回 4.10.2 注册执行报告（已接受）。
async fn handle_designation(
    mt: u32,
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
) {
    let d = match Designation::decode(mt, body) {
        Ok(d) => d,
        Err(e) => {
            ctx.log("error", format!("注册(102099)解析失败: {}", e));
            return;
        }
    };
    if !reject_platform(
        ctx,
        tx,
        mt,
        &d.appl_id,
        &d.cl_ord_id,
        &d.submitting_pbu_id,
        &d.security_id,
        &d.security_id_source,
    )
    .await
    {
        return;
    }
    let rpt = DesignationReport {
        msg_type: msg_type::DESIGNATION_REPORT,
        partition_no: ctx.cfg.partition_for(&d.security_id),
        report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
        appl_id: d.appl_id.clone(),
        reporting_pbu_id: d.submitting_pbu_id.clone(),
        submitting_pbu_id: d.submitting_pbu_id.clone(),
        security_id: d.security_id.clone(),
        security_id_source: d.security_id_source.clone(),
        owner_type: d.owner_type,
        clearing_firm: d.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: d.user_info.clone(),
        order_id: ctx.stats.next_order_id(),
        cl_ord_id: d.cl_ord_id.clone(),
        orig_cl_ord_id: d.orig_cl_ord_id.clone(),
        exec_id: ctx.stats.next_exec_id(),
        exec_type: 0, // New
        ord_rej_reason: 0,
        designation_instruction: d.designation_instruction,
        designation_trans_type: d.designation_trans_type,
        account_id: d.account_id.clone(),
        branch_id: d.branch_id.clone(),
        order_qty: d.order_qty,
        transferee_pbu_id: d.transferee_pbu_id.clone(),
    };
    let _ = tx.send(rpt.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到注册({}) ClOrdID={} 账户={} 转入单元={}，已接受({})",
            mt, d.cl_ord_id, d.account_id, d.transferee_pbu_id, rpt.msg_type
        ),
    );
}

/// 处理投票（4.11.1）：平台校验后回 4.11.2 投票执行报告（已接受）。
async fn handle_evote(mt: u32, ctx: &SessionCtx, tx: &mpsc::Sender<Vec<u8>>, body: &[u8]) {
    let v = match Evote::decode(mt, body) {
        Ok(v) => v,
        Err(e) => {
            ctx.log("error", format!("投票(102197)解析失败: {}", e));
            return;
        }
    };
    if !reject_platform(
        ctx,
        tx,
        mt,
        &v.appl_id,
        &v.cl_ord_id,
        &v.submitting_pbu_id,
        &v.security_id,
        &v.security_id_source,
    )
    .await
    {
        return;
    }
    let rpt = EvoteReport {
        msg_type: msg_type::EVOTE_REPORT,
        partition_no: ctx.cfg.partition_for(&v.security_id),
        report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
        appl_id: v.appl_id.clone(),
        reporting_pbu_id: v.submitting_pbu_id.clone(),
        submitting_pbu_id: v.submitting_pbu_id.clone(),
        security_id: v.security_id.clone(),
        security_id_source: v.security_id_source.clone(),
        owner_type: v.owner_type,
        clearing_firm: v.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: v.user_info.clone(),
        order_id: ctx.stats.next_order_id(),
        cl_ord_id: v.cl_ord_id.clone(),
        exec_id: ctx.stats.next_exec_id(),
        exec_type: 0, // New
        ord_rej_reason: 0,
        account_id: v.account_id.clone(),
        branch_id: v.branch_id.clone(),
        voting_proposal: v.voting_proposal,
        voting_sub_proposal: v.voting_sub_proposal,
        voting_preference: v.voting_preference,
        order_qty: v.order_qty,
    };
    let _ = tx.send(rpt.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到投票({}) ClOrdID={} 议案={} 意向={}，已接受({})",
            mt, v.cl_ord_id, v.voting_proposal, v.voting_preference, rpt.msg_type
        ),
    );
}

/// 处理密码服务（4.12.1）：平台校验后回 4.12.2 密码服务执行报告（已接受）。
async fn handle_password_service(
    mt: u32,
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
) {
    let p = match PasswordService::decode(mt, body) {
        Ok(p) => p,
        Err(e) => {
            ctx.log("error", format!("密码服务(102489)解析失败: {}", e));
            return;
        }
    };
    if !reject_platform(
        ctx,
        tx,
        mt,
        &p.appl_id,
        &p.cl_ord_id,
        &p.submitting_pbu_id,
        &p.security_id,
        &p.security_id_source,
    )
    .await
    {
        return;
    }
    let rpt = PasswordServiceReport {
        msg_type: msg_type::PASSWORD_SERVICE_REPORT,
        partition_no: ctx.cfg.partition_for(&p.security_id),
        report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
        appl_id: p.appl_id.clone(),
        reporting_pbu_id: p.submitting_pbu_id.clone(),
        submitting_pbu_id: p.submitting_pbu_id.clone(),
        security_id: p.security_id.clone(),
        security_id_source: p.security_id_source.clone(),
        owner_type: p.owner_type,
        clearing_firm: p.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: p.user_info.clone(),
        order_id: ctx.stats.next_order_id(),
        cl_ord_id: p.cl_ord_id.clone(),
        exec_id: ctx.stats.next_exec_id(),
        exec_type: 0, // New
        ord_rej_reason: 0,
        account_id: p.account_id.clone(),
        branch_id: p.branch_id.clone(),
        validation_code: p.validation_code,
    };
    let _ = tx.send(rpt.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到密码服务({}) ClOrdID={} 账户={}，已接受({})",
            mt, p.cl_ord_id, p.account_id, rpt.msg_type
        ),
    );
}

/// 处理保证金查询（4.13.1）：平台校验后回 4.13.2 保证金查询结果。
/// 模拟器不保存真实资金，按“查询成功”回 4 条金额为 0 的保证金条目
/// （表 4-122 注 1：1=可用余额 2=总金额 3/4=预留）。
async fn handle_margin_query(
    mt: u32,
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
) {
    let m = match MarginQuery::decode(mt, body) {
        Ok(m) => m,
        Err(e) => {
            ctx.log("error", format!("保证金查询(102587)解析失败: {}", e));
            return;
        }
    };
    if !reject_platform(
        ctx,
        tx,
        mt,
        &m.appl_id,
        &m.cl_ord_id,
        &m.submitting_pbu_id,
        &m.security_id,
        &m.security_id_source,
    )
    .await
    {
        return;
    }
    let rpt = MarginQueryResult {
        msg_type: msg_type::MARGIN_QUERY_RESULT,
        partition_no: ctx.cfg.partition_for(&m.security_id),
        report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
        appl_id: m.appl_id.clone(),
        reporting_pbu_id: m.submitting_pbu_id.clone(),
        submitting_pbu_id: m.submitting_pbu_id.clone(),
        security_id: m.security_id.clone(),
        security_id_source: m.security_id_source.clone(),
        owner_type: m.owner_type,
        clearing_firm: m.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: m.user_info.clone(),
        cl_ord_id: m.cl_ord_id.clone(),
        exec_id: ctx.stats.next_exec_id(),
        exec_type: 0, // New
        ord_rej_reason: 0,
        fund_pbu_id: m.fund_pbu_id.clone(),
        no_margin_items: 4,
        margin_items: vec![(1, 0), (2, 0), (3, 0), (4, 0)],
    };
    let _ = tx.send(rpt.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到保证金查询({}) ClOrdID={} 结算账号={}，已回查询结果({})",
            mt, m.cl_ord_id, m.fund_pbu_id, rpt.msg_type
        ),
    );
}

/// 处理多腿订单（4.14.1，期权行权合并/组合策略）：平台校验后回
/// 4.14.2 多腿订单响应执行报告（已接受，扩展字段同多腿订单）。
async fn handle_multileg(mt: u32, ctx: &SessionCtx, tx: &mpsc::Sender<Vec<u8>>, body: &[u8]) {
    let o = match MultilegOrder::decode(mt, body) {
        Ok(o) => o,
        Err(e) => {
            ctx.log("error", format!("多腿订单({})解析失败: {}", mt, e));
            return;
        }
    };
    if !reject_platform(
        ctx,
        tx,
        mt,
        &o.common.appl_id,
        &o.common.cl_ord_id,
        &o.common.submitting_pbu_id,
        &o.common.security_id,
        &o.common.security_id_source,
    )
    .await
    {
        return;
    }
    let rpt = MultilegExecRpt {
        msg_type: MultilegExecRpt::response_msg_type(mt),
        partition_no: ctx.cfg.partition_for(&o.common.security_id),
        report_index: 0, // 发送时由 writer 任务补写（见 handle_conn）
        appl_id: o.common.appl_id.clone(),
        reporting_pbu_id: o.common.submitting_pbu_id.clone(),
        submitting_pbu_id: o.common.submitting_pbu_id.clone(),
        security_id: o.common.security_id.clone(),
        security_id_source: o.common.security_id_source.clone(),
        owner_type: o.common.owner_type,
        clearing_firm: o.common.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: o.common.user_info.clone(),
        order_id: ctx.stats.next_order_id(),
        cl_ord_id: o.common.cl_ord_id.clone(),
        orig_cl_ord_id: String::new(),
        exec_id: ctx.stats.next_exec_id(),
        exec_type: exec_type::NEW,
        ord_status: ord_status::NEW,
        ord_rej_reason: 0,
        leaves_qty: o.common.order_qty,
        cum_qty: 0,
        side: o.common.side,
        ord_type: o.common.ord_type,
        order_qty: o.common.order_qty,
        price: o.common.price,
        account_id: o.common.account_id.clone(),
        branch_id: o.common.branch_id.clone(),
        order_restrictions: o.common.order_restrictions.clone(),
        // 扩展字段同多腿订单扩展字段（表 4-126 注 2）
        contract_account_code: o.contract_account_code.clone(),
        secondary_order_id: o.secondary_order_id.clone(),
        security_type: o.security_type.clone(),
        security_sub_type: o.security_sub_type.clone(),
        no_legs: o.no_legs,
        legs: o.legs.clone(),
    };
    let _ = tx.send(rpt.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到多腿订单({}) ClOrdID={} 合约数={}，已接受({})",
            mt, o.common.cl_ord_id, o.no_legs, rpt.msg_type
        ),
    );
}

/// 报价回复类型的中文名（日志用）：1=Hit/Lift 2=Counter 6=Pass
fn quote_resp_type_name(t: u8) -> &'static str {
    match t {
        1 => "接受",
        2 => "重报",
        6 => "拒绝",
        _ => "未知",
    }
}

/// 5.6 交易会话状态消息（仅固定收益交易平台）：按当前时刻推断交易会话
/// 子 ID（表 5-7 注 1 的 13 个时间段），起始/结束时间填对应段落的当日
/// 时间戳（YYYYMMDDHHMMSSsss），登录后由主循环下发。
fn encode_session_status() -> Vec<u8> {
    let now = chrono::Local::now();
    let sub = trading_session_sub_id(now);
    TradingSessionStatus {
        msg_type: msg_type::TRADING_SESSION_STATUS,
        market_id: String::new(),
        market_segment_id: "6".into(),
        trading_session_id: String::new(),
        trading_session_sub_id: sub.into(),
        trad_ses_status: 0,
        trad_ses_start_time: session_time_range(now, sub).0,
        trad_ses_end_time: session_time_range(now, sub).1,
    }
    .encode()
}

/// 按当前时刻推断固定收益平台的交易会话子 ID（表 5-7 注 1）
fn trading_session_sub_id(now: chrono::DateTime<chrono::Local>) -> &'static str {
    let hm = now.format("%H%M").to_string().parse::<u32>().unwrap_or(9999);
    match hm {
        0..=859 => "0",          // 开市前 0:00-9:00
        900..=914 => "100",      // 匹配成交前交易 9:00-9:15
        915..=919 => "130",      // 开盘集合竞价（可撤单）9:15-9:20
        920..=924 => "150",      // 开盘集合竞价（不可撤单）9:20-9:25
        925..=929 => "170",      // 匹配成交暂停 9:25-9:30
        930..=959 => "200",      // 上午交易 9:30-10:00
        1000..=1129 => "230",    // 上午交易（竞买应价）10:00-11:30
        1130..=1259 => "300",    // 中午休市 11:30-13:00
        1300..=1329 => "400",    // 下午交易（不可互联）13:00-13:30
        1330..=1459 => "430",    // 下午交易 13:30-15:00
        1500..=1526 => "450",    // 下午交易（分销后）15:00-15:27
        1527..=1529 => "480",    // 收盘连续竞价 15:27-15:30
        _ => "600",              // 收市后 15:30-24:00
    }
}

/// 交易会话子 ID 对应的时间段起止（当日时间戳，YYYYMMDDHHMMSSsss）
fn session_time_range(now: chrono::DateTime<chrono::Local>, sub: &str) -> (i64, i64) {
    let date = now.format("%Y%m%d").to_string();
    let (sh, sm, eh, em, es) = match sub {
        "0" => (0, 0, 9, 0, 0),
        "100" => (9, 0, 9, 15, 0),
        "130" => (9, 15, 9, 20, 0),
        "150" => (9, 20, 9, 25, 0),
        "170" => (9, 25, 9, 30, 0),
        "200" => (9, 30, 10, 0, 0),
        "230" => (10, 0, 11, 30, 0),
        "300" => (11, 30, 13, 0, 0),
        "400" => (13, 0, 13, 30, 0),
        "430" => (13, 30, 15, 0, 0),
        "450" => (15, 0, 15, 27, 0),
        "480" => (15, 27, 15, 30, 0),
        _ => (15, 30, 23, 59, 999),
    };
    let start: i64 = format!("{}{:02}{:02}00000", date, sh, sm).parse().unwrap_or(0);
    // YYYYMMDDHHMMSSsss（秒 00 + 毫秒），与 start 位数一致（17 位）
    let end: i64 = format!("{}{:02}{:02}00{:03}", date, eh, em, es).parse().unwrap_or(0);
    (start, end)
}

/// 手动回复的回报种类（界面在途单上选择“确认/成交/拒单/撤单成功”时指定）。
/// 字段名走 snake_case，与前端 send_report 命令的 kind 参数对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualReportKind {
    /// 确认回报（执行报告 ExecType=0，订单保持已报状态、可继续回复）
    Ack,
    /// 成交回报（可指定成交数量与价格）
    Trade,
    /// 拒单回报（可指定拒单原因代码；front_reject=true 时改发业务拒绝消息）
    Reject,
    /// 撤单成功回报
    Cancel,
}

/// 构造一笔手动回复报文（成交/拒单/撤单成功）。
///
/// 返回 Ok((编码好的完整帧, 日志描述, 订单缓存同步更新))；
/// 业务不支持成交回报时返回 Err（表 3-4：ETF 申赎/认购/行权等无成交回报）。
/// qty/price 用自然单位（股/元），内部换算成协议放大整数；不传时用
/// 订单缓存里的值兜底：
/// - 成交数量默认剩余量（即全部成交），超过剩余量也按剩余量算
/// - 成交价格默认委托价
/// - 拒单原因默认 1
///
/// 订单缓存里只登记了通用字段（编号/证券/方向/价格数量/账户/营业部），
/// 协议特有的交易单元等字段用空串/默认值占位——柜台按 ClOrdID/数量/价格
/// 关联订单即可，不影响手动回复的业务语义。
pub fn build_manual_report(
    cfg: &PlatformConfig,
    stats: &PlatformStats,
    entry: &OrderEntry,
    kind: ManualReportKind,
    qty: Option<f64>,
    price: Option<f64>,
    reason: Option<i32>,
    front_reject: bool,
) -> Result<(Vec<u8>, String, OrderUpdate), String> {
    // 按订单缓存的业务标识（深市 ApplID）反查业务特征，决定回报报文类型；
    // 查不到（老缓存/沪市订单）时按现货竞价兜底
    let biz = biz_info_by_appl_id(&entry.biz)
        .unwrap_or_else(|| strategy::biz_info(msg_type::NEW_ORDER_CASH));
    match kind {
        ManualReportKind::Ack => {
            // 确认回报：执行报告 ExecType=0/OrdStatus=0（已报），订单保持“已报”
            // 状态、可继续回复成交/拒单/撤单。交易所订单号尚无则新分配
            // （不自动回复模式下订单没分配过订单号）
            let mut rpt = base_manual_ack(cfg, stats, entry, biz);
            if rpt.order_id.is_empty() {
                rpt.order_id = stats.next_order_id();
            }
            let desc = format!(
                "手动确认回报({}) ClOrdID={} OrderID={}",
                biz.ack_msg_type,
                entry.cl_ord_id,
                rpt.order_id.trim_start_matches('0')
            );
            let update = OrderUpdate {
                order_id: rpt.order_id.clone(),
                cum_qty: entry.cum_qty,
                leaves_qty: entry.leaves_qty,
                status: entry.status,
            };
            Ok((rpt.encode(), desc, update))
        }
        ManualReportKind::Trade => {
            // 无成交回报的业务（表 3-4）拒绝手动成交：模拟器不编造交易所
            // 不会发的报文
            let Some(trade_msg_type) = biz.trade_msg_type else {
                return Err(format!("业务[{}]无成交回报（表3-4），不能手动回复成交", biz.name));
            };
            // 成交数量钳制到 (0, 剩余量]：不传或超限都按剩余量全成
            // 剩余量不足 1 股（小数股委托或已基本成交）时拒绝手动成交：
            // 否则下方 clamp(1, 0) 会触发 panic（min > max）
            if entry.leaves_qty < 1.0 {
                return Err(format!(
                    "订单 [{}] 剩余数量不足 1 股，不能手动回复成交",
                    entry.cl_ord_id
                ));
            }
            let fill_shares = qty
                .map(|q| q as i64)
                .unwrap_or(entry.leaves_qty as i64)
                .clamp(1, entry.leaves_qty as i64);
            let fill_px = price.unwrap_or(entry.price).max(0.0001);
            let leaves = (entry.leaves_qty - fill_shares as f64).max(0.0);
            let filled = leaves == 0.0;
            // 交易所订单号：尚无则新分配（不自动回复模式下订单没分配过订单号，
            // 直接手动成交也要带合法订单号，且与订单缓存保持一致）
            let order_id = if entry.order_id.is_empty() {
                stats.next_order_id()
            } else {
                entry.order_id.clone()
            };
            let trade = ExecRptTrade {
                msg_type: trade_msg_type,
                partition_no: cfg.partition_for(&entry.security_id),
                report_index: 0, // 发送时由 writer 任务补写
                appl_id: biz.appl_id.into(),
                reporting_pbu_id: entry.pbu.clone(),
                submitting_pbu_id: entry.pbu.clone(),
                security_id: entry.security_id.clone(),
                security_id_source: "102".into(),
                owner_type: entry.owner_type,
                clearing_firm: entry.clearing_firm.clone(),
                transact_time: protocol::now_timestamp(),
                user_info: entry.user_info.clone(),
                order_id: order_id.clone(),
                cl_ord_id: entry.cl_ord_id.clone(),
                exec_id: stats.next_exec_id(),
                exec_type: exec_type::TRADE,
                ord_status: if filled {
                    ord_status::FILLED
                } else {
                    ord_status::PARTIALLY_FILLED
                },
                last_px: (fill_px * 10000.0).round() as i64,
                last_qty: fill_shares * 100,
                leaves_qty: (leaves * 100.0).round() as i64,
                cum_qty: ((entry.cum_qty + fill_shares as f64) * 100.0).round() as i64,
                side: side_byte(&entry.side),
                account_id: entry.account.clone(),
                branch_id: entry.branch.clone(),
                extend: Default::default(),
            };
            let desc = format!(
                "手动成交回报({}) ClOrdID={} 价格={:.4} 数量={} 剩余={}",
                trade_msg_type, entry.cl_ord_id, fill_px, fill_shares, leaves
            );
            let update = OrderUpdate {
                order_id: order_id.clone(),
                cum_qty: entry.cum_qty + fill_shares as f64,
                leaves_qty: leaves,
                status: if filled {
                    OrderStatus::Filled
                } else {
                    OrderStatus::Partial
                },
            };
            Ok((trade.encode(), desc, update))
        }
        // 拒单：默认回订单响应及撤单成功执行报告（2xxx02，ExecType=8）；
        // 勾选“前台拒单”时改回业务拒绝消息（MsgType=4），不进执行报告流
        ManualReportKind::Reject => {
            if front_reject {
                let rej_mt = strategy::order_msg_type_by_appl_id(&entry.biz)
                    .unwrap_or(msg_type::NEW_ORDER_CASH);
                let rej = BusinessReject {
                    appl_id: entry.biz.clone(),
                    transact_time: protocol::now_timestamp(),
                    submitting_pbu_id: entry.pbu.clone(),
                    security_id: entry.security_id.clone(),
                    security_id_source: "102".into(),
                    ref_seq_num: 0,
                    ref_msg_type: rej_mt,
                    business_reject_ref_id: entry.cl_ord_id.clone(),
                    business_reject_reason: reason.unwrap_or(1) as u16,
                    business_reject_text: String::new(),
                };
                let desc = format!(
                    "手动前台拒单(4) ClOrdID={} 原因代码={}",
                    entry.cl_ord_id, rej.business_reject_reason
                );
                let update = OrderUpdate {
                    order_id: String::new(), // 业务拒绝未分配交易所订单号
                    cum_qty: entry.cum_qty,
                    leaves_qty: 0.0,
                    status: OrderStatus::Rejected,
                };
                Ok((rej.encode(), desc, update))
            } else {
                let mut rpt = base_manual_ack(cfg, stats, entry, biz);
                if rpt.order_id.is_empty() {
                    rpt.order_id = stats.next_order_id();
                }
                rpt.exec_type = exec_type::REJECT;
                rpt.ord_status = ord_status::REJECTED;
                rpt.ord_rej_reason = reason.unwrap_or(1) as u16;
                rpt.leaves_qty = 0;
                rpt.cum_qty = (entry.cum_qty * 100.0).round() as i64;
                let desc = format!(
                    "手动拒单回报({}) ClOrdID={} 原因代码={}",
                    biz.ack_msg_type, entry.cl_ord_id, rpt.ord_rej_reason
                );
                let update = OrderUpdate {
                    order_id: rpt.order_id.clone(),
                    cum_qty: entry.cum_qty,
                    leaves_qty: 0.0,
                    status: OrderStatus::Rejected,
                };
                Ok((rpt.encode(), desc, update))
            }
        }
        ManualReportKind::Cancel => {
            let mut rpt = base_manual_ack(cfg, stats, entry, biz);
            if rpt.order_id.is_empty() {
                rpt.order_id = stats.next_order_id();
            }
            rpt.exec_type = exec_type::CANCELLED;
            rpt.ord_status = ord_status::CANCELLED;
            // 手动撤单没有“撤单请求编号”，原单号同时填 ClOrdID/OrigClOrdID，
            // 柜台按任一字段都能关联上
            rpt.orig_cl_ord_id = entry.cl_ord_id.clone();
            rpt.leaves_qty = 0;
            rpt.cum_qty = (entry.cum_qty * 100.0).round() as i64;
            let desc = format!(
                "手动撤单成功回报({}) ClOrdID={}",
                biz.ack_msg_type, entry.cl_ord_id
            );
            let update = OrderUpdate {
                order_id: rpt.order_id.clone(),
                cum_qty: entry.cum_qty,
                leaves_qty: 0.0,
                status: OrderStatus::Cancelled,
            };
            Ok((rpt.encode(), desc, update))
        }
    }
}

/// 手动回报的公共骨架：把订单缓存里登记的信息回填进确认执行报告（2xxx02），
/// 再补上新的回报序号/执行编号；报文类型按业务特征（biz）选择；
/// 执行类型/状态由调用方再改
fn base_manual_ack(
    cfg: &PlatformConfig,
    stats: &PlatformStats,
    entry: &OrderEntry,
    biz: strategy::BizInfo,
) -> ExecRptAck {
    ExecRptAck {
        msg_type: biz.ack_msg_type,
        partition_no: cfg.partition_for(&entry.security_id),
        report_index: 0, // 发送时由 writer 任务补写
        appl_id: biz.appl_id.into(),
        reporting_pbu_id: entry.pbu.clone(),
        submitting_pbu_id: entry.pbu.clone(),
        security_id: entry.security_id.clone(),
        security_id_source: "102".into(),
        owner_type: entry.owner_type,
        clearing_firm: entry.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: entry.user_info.clone(),
        order_id: entry.order_id.clone(),
        cl_ord_id: entry.cl_ord_id.clone(),
        orig_cl_ord_id: String::new(),
        exec_id: stats.next_exec_id(),
        exec_type: exec_type::NEW,
        ord_status: ord_status::NEW,
        ord_rej_reason: 0,
        leaves_qty: (entry.leaves_qty * 100.0).round() as i64,
        cum_qty: (entry.cum_qty * 100.0).round() as i64,
        side: side_byte(&entry.side),
        ord_type: entry.ord_type,
        order_qty: (entry.qty * 100.0).round() as i64,
        price: (entry.price * 10000.0).round() as i64,
        account_id: entry.account.clone(),
        branch_id: entry.branch.clone(),
        order_restrictions: String::new(),
        extend: Default::default(),
    }
}

/// 把订单缓存里的方向中文转回协议字节（'1'买/'2'卖，手动回报回填用）
fn side_byte(side: &str) -> u8 {
    if side.contains("卖") {
        b'2'
    } else {
        b'1'
    }
}

/// 把协议里的买卖方向码（'1'/'2'）转成中文，仅供日志显示
fn side_name(side: u8) -> &'static str {
    match side {
        b'1' => "买",
        b'2' => "卖",
        _ => "?",
    }
}
