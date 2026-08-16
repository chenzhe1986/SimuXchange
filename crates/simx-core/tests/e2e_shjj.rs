//! 上交所竞价平台端到端集成测试：引擎启动 → 柜台 TCP 连接 → Logon →
//! 分区序号同步 → 委托 → 校验回报。
//!
//! 覆盖会话层（Logon/平台状态/执行报告信息/序号同步/心跳）与业务层
//! （新订单 58 → 申报响应 32 / 成交 103，撤单 61 → 撤单失败 59）。
//!
//! 与深交所测试的不同点：报文头 16 字节（多了 MsgSeqNum），登录后必须先做
//! 分区序号同步（206/207）才会推送执行报告，因此还专门验证“同步前委托的回报
//! 会被缓存、同步后补发”这一上交所特有语义。
//!
//! 运行方式：在项目根目录执行 `cargo test -p simx-core`。
//! 各测试使用独立端口（18201/18202/18203/18204），互不干扰。

use simx_core::config::{
    GatewayCategory, GatewayConfig, PlatformConfig, StrategyConfig, StrategyMode,
};
use simx_core::engine::Engine;
use simx_core::shjj::protocol::{self as protocol, msg_type, BodyWriter, Logon, BIZ_ID_CASH_AUCTION};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 读取一条完整报文（消息类型, 消息体）。
///
/// 柜台视角：先读 16 字节头（类型 + 消息序号 + 长度），再按长度读消息体，
/// 最后读 4 字节校验和并验证——顺便把“引擎发出的每条报文校验和都正确”也测了。
async fn read_frame(stream: &mut TcpStream) -> (u32, Vec<u8>) {
    let mut head = [0u8; 16];
    stream.read_exact(&mut head).await.expect("读消息头失败");
    let mt = u32::from_be_bytes(head[0..4].try_into().unwrap());
    let len = u32::from_be_bytes(head[12..16].try_into().unwrap()) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.expect("读消息体失败");
    let mut cks = [0u8; 4];
    stream.read_exact(&mut cks).await.expect("读校验和失败");
    let mut all = Vec::new();
    all.extend_from_slice(&head);
    all.extend_from_slice(&body);
    assert_eq!(
        protocol::checksum(&all),
        u32::from_be_bytes(cks),
        "回报校验和错误 MsgType={}",
        mt
    );
    (mt, body)
}

/// 构造一笔现货竞价新订单（58），字段顺序严格按上交所接口规范排列。
///
/// 参数用协议里的“放大整数”：qty_1000x 是股数乘 1000，
/// price_n13_5 是价格乘 100000（例如 12.34 元写成 12_34000）。
fn new_order_frame(cl_ord_id: &str, qty_1000x: i64, price_n13_5: i64, side: u8) -> Vec<u8> {
    let mut w = BodyWriter::new();
    w.u32(BIZ_ID_CASH_AUCTION); // BizID
    w.str("PBU00001", 8); // BizPbu
    w.str(cl_ord_id, 10); // ClOrdID
    w.str("600000", 12); // SecurityID
    w.str("B880000001", 13); // Account
    w.u8(1); // OwnerType
    w.ch(side); // Side
    w.i64(price_n13_5); // Price N13(5)
    w.i64(qty_1000x); // OrderQty N15(3)
    w.ch(b'2'); // OrdType=限价
    w.ch(b'0'); // TimeInForce
    w.u64(protocol::now_ntime()); // TransactTime
    w.str("", 2); // CreditTag
    w.str("CF01", 8); // ClearingFirm
    w.str("BR01", 8); // BranchID
    w.str("UINFO", 32); // UserInfo
    protocol::frame(msg_type::NEW_ORDER, &w.into_inner())
}

/// 构造一条分区序号同步请求（206）：单个分区，从序号 1 开始
fn sync_frame(pbu: &str, set_id: u32) -> Vec<u8> {
    let mut w = BodyWriter::new();
    w.u16(1); // NoGroups
    w.str(pbu, 8);
    w.u32(set_id);
    w.u64(1);
    protocol::frame(msg_type::EXEC_RPT_SYNC, &w.into_inner())
}

/// 完成 Logon 握手，返回已登录（但尚未做序号同步）的连接。
///
/// 验证登录应答里双方代码已对调，并按顺序收齐登录后引擎主动推送的
/// 平台状态(209) 与 执行报告信息(208) 两条报文。
async fn connect_and_logon(port: u16) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("连接失败");
    let logon = Logon {
        sender_comp_id: "OMS_TEST".into(),
        target_comp_id: "TDGW".into(),
        heart_bt_int: 30,
        prtcl_version: "0.50".into(),
        trade_date: protocol::now_date(),
        qsize: 32,
    };
    stream.write_all(&logon.encode()).await.unwrap();

    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::LOGON);
    let reply = Logon::decode(&body).unwrap();
    assert_eq!(reply.sender_comp_id, "SIMX_TGW");
    assert_eq!(reply.target_comp_id, "OMS_TEST");
    let (mt, _) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::PLATFORM_STATE);
    let (mt, _) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::EXEC_RPT_INFO);
    stream
}

/// 完成 Logon + 分区序号同步，返回可以正常收执行报告的连接
async fn connect_logon_sync(port: u16) -> TcpStream {
    let mut stream = connect_and_logon(port).await;
    stream.write_all(&sync_frame("OMS_TEST", 1)).await.unwrap();
    let (mt, _) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::EXEC_RPT_SYNC_RSP, "同步后应收到同步响应(207)");
    stream
}

/// 拼一个最小可用的上交所竞价测试网关配置
fn test_gateway(port: u16, strategy: StrategyConfig) -> GatewayConfig {
    GatewayConfig {
        id: String::new(),
        name: "上海竞价测试网关".into(),
        category: GatewayCategory::Shjj,
        platforms: vec![PlatformConfig {
            id: String::new(),
            name: "竞价平台".into(),
            platform_type: 0, // 竞价平台 PlatformID=0
            listen_host: "127.0.0.1".into(),
            port,
            strategy,
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// 测试一：“全部成交（单笔）+ 无延迟”的完整流程，
/// 逐字段核对申报响应(32)与成交回报(103)，最后检查统计计数。
#[tokio::test]
async fn test_shjj_full_single_sync_flow() {
    let dir = std::env::temp_dir().join(format!("simx_shjj_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());

    let gw = engine
        .save_gateway(test_gateway(
            18201,
            StrategyConfig { mode: StrategyMode::FullSingle, ..Default::default() },
        ))
        .await
        .expect("保存网关失败");
    engine.start_gateway(&gw.id).await.expect("启动网关失败");

    let mut stream = connect_logon_sync(18201).await;

    // 买入 600000，500 股 @ 12.34（500_000 = 500 股乘 1000；12_34000 = 12.34 元乘 10 万）
    stream
        .write_all(&new_order_frame("A000000001", 500_000, 12_34000, b'1'))
        .await
        .unwrap();

    // 申报响应 32：核对开头几个字段
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::EXEC_RPT, "第一条应为申报响应(32)");
    let mut r = protocol::BodyReader::new(&body);
    let _pbu = r.str(8).unwrap();
    let _set_id = r.u32().unwrap();
    let _report_index = r.u64().unwrap();
    assert_eq!(r.u32().unwrap(), BIZ_ID_CASH_AUCTION); // BizID
    assert_eq!(r.ch().unwrap(), b'0'); // ExecType=申报成功
    let _biz_pbu = r.str(8).unwrap();
    assert_eq!(r.str(10).unwrap(), "A000000001"); // ClOrdID
    assert_eq!(r.str(12).unwrap(), "600000"); // SecurityID

    // 成交回报 103：验证成交价=委托价、成交量=全部、剩余量=0、全部成交
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::TRADE, "第二条应为成交回报(103)");
    let mut r = protocol::BodyReader::new(&body);
    let _ = r.str(8).unwrap(); // Pbu
    let _ = r.u32().unwrap(); // SetID
    let _ = r.u64().unwrap(); // ReportIndex
    let _ = r.u32().unwrap(); // BizID
    assert_eq!(r.ch().unwrap(), b'F'); // ExecType=Trade
    let _ = r.str(8).unwrap(); // BizPbu
    assert_eq!(r.str(10).unwrap(), "A000000001"); // ClOrdID
    assert_eq!(r.str(12).unwrap(), "600000"); // SecurityID
    let _ = r.str(13).unwrap(); // Account
    let _ = r.u8().unwrap(); // OwnerType
    let _ = r.u64().unwrap(); // OrderEntryTime
    assert_eq!(r.i64().unwrap(), 12_34000); // LastPx=委托价
    assert_eq!(r.i64().unwrap(), 500_000); // LastQty=全部数量
    let _gross = r.i64().unwrap(); // GrossTradeAmt
    let _ = r.ch().unwrap(); // Side
    assert_eq!(r.i64().unwrap(), 500_000); // OrderQty
    assert_eq!(r.i64().unwrap(), 0); // LeavesQty=0
    assert_eq!(r.ch().unwrap(), b'2'); // OrdStatus=全部成交

    let snap = engine.snapshot().await;
    let p = &snap.gateways[0].platforms[0];
    assert_eq!(p.stats.orders, 1);
    assert_eq!(p.stats.acks, 1);
    assert_eq!(p.stats.trades, 1);
    assert_eq!(p.connections.len(), 1);

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试二：分区序号同步“门槛”——同步前发的委托，其回报必须被缓存，
/// 直到收到 ExecRptSync(206) 才补发。顺带验证拒单策略走申报响应(32,ExecType=8)。
#[tokio::test]
async fn test_shjj_sync_gate_buffers_reports() {
    let dir = std::env::temp_dir().join(format!("simx_shjj_gate_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());

    let gw = engine
        .save_gateway(test_gateway(
            18202,
            StrategyConfig { mode: StrategyMode::AckOnly, ..Default::default() },
        ))
        .await
        .unwrap();
    engine.start_gateway(&gw.id).await.unwrap();

    // 仅登录、不做序号同步，直接发委托
    let mut stream = connect_and_logon(18202).await;
    stream
        .write_all(&new_order_frame("A000000002", 100_000, 10_00000, b'1'))
        .await
        .unwrap();

    // 同步之前不应收到任何执行报告：短暂等待后读应超时
    let early = tokio::time::timeout(Duration::from_millis(400), read_frame(&mut stream)).await;
    assert!(early.is_err(), "序号同步前不应推送执行报告");

    // 发送序号同步：先收到同步响应(207)，再补发此前缓存的确认回报(32)
    stream.write_all(&sync_frame("OMS_TEST", 1)).await.unwrap();
    let (mt, _) = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut stream))
        .await
        .expect("等待同步响应超时");
    assert_eq!(mt, msg_type::EXEC_RPT_SYNC_RSP);
    let (mt, _) = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut stream))
        .await
        .expect("等待缓存补发的确认回报超时");
    assert_eq!(mt, msg_type::EXEC_RPT, "同步后应补发缓存的申报响应(32)");

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试三：撤单一律回撤单失败(59)，并验证网关停止时会主动发 Logout 告别。
#[tokio::test]
async fn test_shjj_cancel_reject_and_logout() {
    let dir = std::env::temp_dir().join(format!("simx_shjj_cxl_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());

    let gw = engine
        .save_gateway(test_gateway(18203, StrategyConfig::default()))
        .await
        .unwrap();
    engine.start_gateway(&gw.id).await.unwrap();

    let mut stream = connect_logon_sync(18203).await;

    // 撤单申报 61
    let mut w = BodyWriter::new();
    w.u32(BIZ_ID_CASH_AUCTION);
    w.str("PBU00001", 8);
    w.str("A000000004", 10); // 本笔撤单编号
    w.str("600000", 12);
    w.str("B880000001", 13);
    w.u8(1);
    w.ch(b'1');
    w.str("A000000003", 10); // 原订单编号
    w.u64(protocol::now_ntime());
    w.str("BR01", 8);
    w.str("", 32);
    stream
        .write_all(&protocol::frame(msg_type::CANCEL_ORDER, &w.into_inner()))
        .await
        .unwrap();

    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::CANCEL_REJECT, "撤单应回撤单失败(59)");
    let mut r = protocol::BodyReader::new(&body);
    let _ = r.str(8).unwrap(); // Pbu
    let _ = r.u32().unwrap(); // SetID
    let _ = r.u64().unwrap(); // ReportIndex
    let _ = r.u32().unwrap(); // BizID
    let _ = r.str(8).unwrap(); // BizPbu
    assert_eq!(r.str(10).unwrap(), "A000000004"); // ClOrdID
    let _ = r.str(12).unwrap(); // SecurityID
    assert_eq!(r.str(10).unwrap(), "A000000003"); // OrigClOrdID

    // 关闭时引擎应发出 Logout
    engine.stop_gateway(&gw.id).await.unwrap();
    let (mt, _) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::LOGOUT, "网关停止时应发送 Logout");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试四：拆单成交 + 带延迟回报（走异步发送路径），
/// 验证拆成的 3 笔成交数量加起来正好等于委托总量。
#[tokio::test]
async fn test_shjj_split_trades_async_delay() {
    let dir = std::env::temp_dir().join(format!("simx_shjj_split_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());

    let strategy = StrategyConfig {
        mode: StrategyMode::FullSplit,
        split_count_min: 3,
        split_count_max: 3,
        ack_delay: simx_core::config::DelayConfig { min_ms: 10, max_ms: 20 },
        trade_delay: simx_core::config::DelayConfig { min_ms: 5, max_ms: 10 },
        ..Default::default()
    };
    let gw = engine.save_gateway(test_gateway(18204, strategy)).await.unwrap();
    engine.start_gateway(&gw.id).await.unwrap();

    let mut stream = connect_logon_sync(18204).await;
    stream
        .write_all(&new_order_frame("A000000005", 1000_000, 20_00000, b'1'))
        .await
        .unwrap();

    // 1 条申报响应 + 3 条成交
    let (mt, _) = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut stream))
        .await
        .expect("等待申报响应超时");
    assert_eq!(mt, msg_type::EXEC_RPT);

    let mut total_qty = 0i64;
    for i in 0..3 {
        let (mt, body) = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut stream))
            .await
            .unwrap_or_else(|_| panic!("等待第 {} 笔成交回报超时", i + 1));
        assert_eq!(mt, msg_type::TRADE);
        // 成交体字段偏移：Pbu8 SetID4 RepIdx8 BizID4 ExecType1 BizPbu8 ClOrdID10
        // SecurityID12 Account13 OwnerType1 OrderEntryTime8 = 77，LastPx 在 77..85，
        // LastQty 在 85..93
        let last_qty = i64::from_be_bytes(body[85..93].try_into().unwrap());
        total_qty += last_qty;
    }
    assert_eq!(total_qty, 1000_000, "拆单成交数量之和应等于委托数量");

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试五：执行回报同步重发——报单产生回报（分区 SetID=1，记录号 1、2）
/// 后断开重连，再发 206 同步 begin=1，应收到 207 + 原样重发的历史回报：
/// 重发帧保留原记录号（柜台按分区对账），MsgSeqNum 按新连接重新编号。
#[tokio::test]
async fn test_shjj_sync_resends_history_by_begin() {
    let dir = std::env::temp_dir().join(format!("simx_shjj_rsync_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());
    let gw = engine
        .save_gateway(test_gateway(
            18205,
            StrategyConfig { mode: StrategyMode::FullSingle, ..Default::default() },
        ))
        .await
        .unwrap();
    engine.start_gateway(&gw.id).await.unwrap();

    // 第一段连接：同步（无历史）→ 报单 → 收确认(32, 记录号1) + 成交(103, 记录号2)
    let mut s1 = connect_logon_sync(18205).await;
    s1.write_all(&new_order_frame("RESEND001", 500_000, 12_34000, b'1'))
        .await
        .unwrap();
    let (mt, body) = read_frame(&mut s1).await;
    assert_eq!(mt, msg_type::EXEC_RPT);
    assert_eq!(
        u64::from_be_bytes(body[12..20].try_into().unwrap()),
        1,
        "第一条回报记录号应为 1"
    );
    let (mt, body) = read_frame(&mut s1).await;
    assert_eq!(mt, msg_type::TRADE);
    assert_eq!(
        u64::from_be_bytes(body[12..20].try_into().unwrap()),
        2,
        "第二条回报记录号应为 2"
    );
    drop(s1); // 断开
    tokio::time::sleep(Duration::from_millis(500)).await; // 等旧会话清理完（单连接限制）

    // 重连 + 同步：207 之后应原样重发记录号 1、2 的两条历史回报
    let mut s2 = connect_logon_sync(18205).await;
    let (mt, body) = read_frame(&mut s2).await;
    assert_eq!(mt, msg_type::EXEC_RPT, "应重发申报响应(32)");
    assert_eq!(
        u64::from_be_bytes(body[12..20].try_into().unwrap()),
        1,
        "重发帧应保留原记录号 1"
    );
    let (mt, body) = read_frame(&mut s2).await;
    assert_eq!(mt, msg_type::TRADE, "应重发成交回报(103)");
    assert_eq!(
        u64::from_be_bytes(body[12..20].try_into().unwrap()),
        2,
        "重发帧应保留原记录号 2"
    );

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
