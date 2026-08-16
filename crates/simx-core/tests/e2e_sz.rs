//! 深交所端到端集成测试：引擎启动 → 柜台 TCP 连接 → Logon → 委托 → 校验回报
//!
//! 覆盖会话层（Logon/平台信息/平台状态/心跳）与业务层（100101 → 200102/200115）。
//!
//! 端到端（e2e）测试：不测单个函数，而是模拟真实证券柜台——真正启动
//! 引擎、用 TCP 连接、发送二进制报文，逐字节校验回报。测试通过即证明
//! “网络 → 协议 → 策略”整条链路正确。
//! 运行方式：在项目根目录执行 `cargo test -p simx-core`。
//!
//! 三个测试各用一个独立端口（18101/18102/18103），互不干扰，可并行跑。

use simx_core::config::{GatewayConfig, PlatformConfig, StrategyConfig, StrategyMode};
use simx_core::engine::Engine;
use simx_core::sz::protocol::{self as protocol, msg_type, BodyWriter, Logon};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 读取一条完整报文（消息类型, 消息体）
///
/// 这是“柜台视角”的收报文逻辑：先读 8 字节头（类型+长度），
/// 再按长度读消息体，最后读 4 字节校验和并验证——顺便把
/// “引擎发出的每一条报文校验和都正确”也测了。
async fn read_frame(stream: &mut TcpStream) -> (u32, Vec<u8>) {
    let mut head = [0u8; 8];
    stream.read_exact(&mut head).await.expect("读消息头失败");
    let mt = u32::from_be_bytes(head[0..4].try_into().unwrap());
    let len = u32::from_be_bytes(head[4..8].try_into().unwrap()) as usize;
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

/// 构造一笔现货新订单（100101），字段顺序严格按深交所接口规范排列
///
/// 参数用的是协议里的“放大整数”：qty_100x 是股数×100，
/// price_n13_4 是价格×10000（例如 12.34 元写成 12_3400）。
fn new_order_frame(cl_ord_id: &str, qty_100x: i64, price_n13_4: i64, side: u8) -> Vec<u8> {
    new_order_frame_sec(cl_ord_id, "000001", qty_100x, price_n13_4, side)
}

/// 同上，但证券代码可指定（多分区测试要用不同证券哈希到不同分区）
fn new_order_frame_sec(
    cl_ord_id: &str,
    security_id: &str,
    qty_100x: i64,
    price_n13_4: i64,
    side: u8,
) -> Vec<u8> {
    let mut w = BodyWriter::new();
    w.str("010", 3); // ApplID
    w.str("100001", 6); // SubmittingPBUID
    w.str(security_id, 8); // SecurityID
    w.str("102", 4); // SecurityIDSource
    w.u16(1); // OwnerType
    w.str("01", 2); // ClearingFirm
    w.i64(protocol::now_timestamp()); // TransactTime
    w.str("", 8); // UserInfo
    w.str(cl_ord_id, 10); // ClOrdID
    w.str("0123456789AB", 12); // AccountID
    w.str("0001", 4); // BranchID
    w.str("", 4); // OrderRestrictions
    w.ch(side); // Side
    w.ch(b'2'); // OrdType=限价
    w.i64(qty_100x); // OrderQty N15(2)
    w.i64(price_n13_4); // Price N13(4)
    w.i64(0); // StopPx
    w.i64(0); // MinQty
    w.u16(0); // MaxPriceLevels
    w.ch(b'0'); // TimeInForce
    w.ch(b' '); // CashMargin
    protocol::frame(msg_type::NEW_ORDER_CASH, &w.into_inner())
}

/// 完成 Logon 握手，返回已登录的连接
///
/// 验证登录应答里双方代码已对调（我发时 sender=OMS_TEST，
/// 引擎回时 sender=SIMX_TGW、target=OMS_TEST），并按顺序收齐
/// 登录后引擎主动推送的平台信息和平台状态两条报文。
async fn connect_and_logon(port: u16) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("连接失败");
    let logon = Logon {
        sender_comp_id: "OMS_TEST".into(),
        target_comp_id: "SIMX_TGW".into(),
        heart_bt_int: 30,
        password: String::new(),
        default_appl_ver_id: "1.29".into(),
    };
    stream.write_all(&logon.encode()).await.unwrap();

    // 期待依次收到：Logon 应答、平台信息(9)、平台状态(6)
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::LOGON);
    let reply = Logon::decode(&body).unwrap();
    assert_eq!(reply.sender_comp_id, "SIMX_TGW");
    assert_eq!(reply.target_comp_id, "OMS_TEST");
    let (mt, _) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::PLATFORM_INFO);
    let (mt, _) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::PLATFORM_STATE);
    stream
}

/// 拼一个最小可用的测试网关配置：一个网关下挂一个平台，策略由各测试自定
fn test_gateway(port: u16, platform_type: u16, strategy: StrategyConfig) -> GatewayConfig {
    GatewayConfig {
        id: String::new(),
        name: "测试网关".into(),
        platforms: vec![PlatformConfig {
            id: String::new(),
            name: "测试平台".into(),
            listen_host: "127.0.0.1".into(),
            port,
            platform_type,
            strategy,
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// 测试一：“全部成交（单笔）+ 无延迟”的完整交易流程，
/// 逐字段核对确认回报和成交回报的内容，最后检查统计计数。
#[tokio::test]
async fn test_full_single_sync_flow() {
    // 每个测试用独立的临时目录存配置，跑完删掉，不污染真实数据
    let dir = std::env::temp_dir().join(format!("simx_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());

    let gw = engine
        .save_gateway(test_gateway(
            18101,
            1,
            StrategyConfig { mode: StrategyMode::FullSingle, ..Default::default() },
        ))
        .await
        .expect("保存网关失败");
    engine.start_gateway(&gw.id).await.expect("启动网关失败");

    let mut stream = connect_and_logon(18101).await;

    // 发送委托：买入 000001，500 股 @ 12.34
    // （500_00 = 500 股×100；12_3400 = 12.34 元×10000；下划线只是分隔符方便阅读）
    stream
        .write_all(&new_order_frame("TEST000001", 500_00, 12_3400, b'1'))
        .await
        .unwrap();

    // 确认回报 200102
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::EXEC_RPT_CASH_ACK, "第一条应为确认回报");
    let mut r = protocol::BodyReader::new(&body);
    let _partition_no = r.i32().unwrap();
    let _report_index = r.i64().unwrap();
    assert_eq!(r.str(3).unwrap(), "010"); // ApplID
    let _ = r.str(6).unwrap(); // ReportingPBUID
    let _ = r.str(6).unwrap(); // SubmittingPBUID
    assert_eq!(r.str(8).unwrap(), "000001"); // SecurityID

    // 成交回报 200115：全部成交——验证成交价=委托价、成交量=全部、剩余量=0
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::EXEC_RPT_CASH_TRADE, "第二条应为成交回报");
    let mut r = protocol::BodyReader::new(&body);
    let _ = r.i32().unwrap(); // PartitionNo
    let _ = r.i64().unwrap(); // ReportIndex
    let _ = r.str(3).unwrap(); // ApplID
    let _ = r.str(6).unwrap();
    let _ = r.str(6).unwrap();
    let _ = r.str(8).unwrap(); // SecurityID
    let _ = r.str(4).unwrap(); // SecurityIDSource
    let _ = r.u16().unwrap(); // OwnerType
    let _ = r.str(2).unwrap(); // ClearingFirm
    let _ = r.i64().unwrap(); // TransactTime
    let _ = r.str(8).unwrap(); // UserInfo
    let _ = r.str(16).unwrap(); // OrderID
    assert_eq!(r.str(10).unwrap(), "TEST000001"); // ClOrdID
    let _ = r.str(16).unwrap(); // ExecID
    assert_eq!(r.ch().unwrap(), b'F'); // ExecType=Trade
    assert_eq!(r.ch().unwrap(), b'2'); // OrdStatus=全部成交
    assert_eq!(r.i64().unwrap(), 12_3400); // LastPx=委托价
    assert_eq!(r.i64().unwrap(), 500_00); // LastQty=全部数量
    assert_eq!(r.i64().unwrap(), 0); // LeavesQty=0
    assert_eq!(r.i64().unwrap(), 500_00); // CumQty

    // 统计校验：引擎内部计数器也要和刚才的交易对得上
    let snap = engine.snapshot().await;
    let p = &snap.gateways[0].platforms[0];
    assert_eq!(p.stats.orders, 1);
    assert_eq!(p.stats.acks, 1);
    assert_eq!(p.stats.trades, 1);
    assert_eq!(p.connections.len(), 1);

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试二：拒单策略（只回一条拒绝的确认回报，没有成交），
/// 并验证网关停止时会主动给柜台发 Logout 告别。
#[tokio::test]
async fn test_reject_and_ack_only_flow() {
    let dir = std::env::temp_dir().join(format!("simx_test_rej_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());

    // 拒单策略
    let gw = engine
        .save_gateway(test_gateway(
            18102,
            1,
            StrategyConfig { mode: StrategyMode::Reject, ..Default::default() },
        ))
        .await
        .unwrap();
    engine.start_gateway(&gw.id).await.unwrap();

    let mut stream = connect_and_logon(18102).await;
    stream
        .write_all(&new_order_frame("TESTREJ001", 100_00, 10_0000, b'2'))
        .await
        .unwrap();

    // 只应收到一条 200102，ExecType=8（拒绝）
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::EXEC_RPT_CASH_ACK);
    // 定位 ExecType：把前面所有定长字段的字节数加起来
    // 4+8+3+6+6+8+4+2+2+8+8+16+10+10+16 = 111，所以第 111 字节就是 ExecType
    assert_eq!(body[111], b'8', "ExecType 应为拒绝");
    assert_eq!(body[112], b'8', "OrdStatus 应为已拒绝");

    // 关闭时引擎应发出 Logout
    engine.stop_gateway(&gw.id).await.unwrap();
    let (mt, _) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::LOGOUT, "网关停止时应发送 Logout");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试三：拆单成交 + 带延迟回报（走的是异步发送路径），
/// 验证拆成的 3 笔成交数量加起来正好等于委托总量。
#[tokio::test]
async fn test_split_trades_async_delay() {
    let dir = std::env::temp_dir().join(format!("simx_test_split_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());

    // 多笔拆单 + 少量延迟（验证异步发送路径）
    let strategy = StrategyConfig {
        mode: StrategyMode::FullSplit,
        split_count_min: 3,
        split_count_max: 3,
        ack_delay: simx_core::config::DelayConfig { min_ms: 10, max_ms: 20 },
        trade_delay: simx_core::config::DelayConfig { min_ms: 5, max_ms: 10 },
        ..Default::default()
    };
    let gw = engine.save_gateway(test_gateway(18103, 1, strategy)).await.unwrap();
    engine.start_gateway(&gw.id).await.unwrap();

    let mut stream = connect_and_logon(18103).await;
    stream
        .write_all(&new_order_frame("TESTSPL001", 1000_00, 20_0000, b'1'))
        .await
        .unwrap();

    // 因为回报是延迟发的，每次读都套一个 3 秒超时：真出 bug 收不到回报时
    // 测试会快速报错而不是永久卡死
    // 1 条确认 + 3 条成交
    let (mt, _) = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut stream))
        .await
        .expect("等待确认回报超时");
    assert_eq!(mt, msg_type::EXEC_RPT_CASH_ACK);

    let mut total_qty = 0i64;
    for i in 0..3 {
        let (mt, body) = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut stream))
            .await
            .unwrap_or_else(|_| panic!("等待第 {} 笔成交回报超时", i + 1));
        assert_eq!(mt, msg_type::EXEC_RPT_CASH_TRADE);
        // LastQty 位置：4+8+3+6+6+8+4+2+2+8+8+16+10+16+1+1 = 103，LastPx 在 103..111，LastQty 在 111..119
        let last_qty = i64::from_be_bytes(body[111..119].try_into().unwrap());
        total_qty += last_qty;
    }
    assert_eq!(total_qty, 1000_00, "拆单成交数量之和应等于委托数量");

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试四：平台校验——现货平台（平台号 1）上收到属于固定收益平台（平台号 6）
/// 的债券回购委托（100201，ApplID=020），应回 20108 业务拒绝（表 3-3 / tgw_error.csv），
/// 委托不进入受理流程（订单计数为 0，业务拒绝计数为 1）。
#[tokio::test]
async fn test_platform_mismatch_business_reject() {
    let dir = std::env::temp_dir().join(format!("simx_test_plat_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());
    let gw = engine
        .save_gateway(test_gateway(
            18104,
            1, // 现货集中竞价交易平台
            StrategyConfig { mode: StrategyMode::FullSingle, ..Default::default() },
        ))
        .await
        .unwrap();
    engine.start_gateway(&gw.id).await.unwrap();
    let mut stream = connect_and_logon(18104).await;

    // 构造债券通用质押式回购新订单（100201）：公共 89 字节 + 扩展 19 字节
    let mut w = BodyWriter::new();
    w.str("020", 3); // ApplID：固定收益交易平台业务
    w.str("100001", 6); // SubmittingPBUID
    w.str("000001", 8); // SecurityID
    w.str("102", 4); // SecurityIDSource
    w.u16(1); // OwnerType
    w.str("01", 2); // ClearingFirm
    w.i64(protocol::now_timestamp()); // TransactTime
    w.str("", 8); // UserInfo
    w.str("PLATREJ01", 10); // ClOrdID
    w.str("0123456789AB", 12); // AccountID
    w.str("0001", 4); // BranchID
    w.str("", 4); // OrderRestrictions
    w.ch(b'1'); // Side
    w.ch(b'2'); // OrdType
    w.i64(100_00); // OrderQty
    w.i64(10_0000); // Price
    w.i64(0); // StopPx
    w.i64(0); // MinQty
    w.u16(0); // MaxPriceLevels
    w.ch(b'0'); // TimeInForce
    stream
        .write_all(&protocol::frame(msg_type::NEW_ORDER_BOND_REPO, &w.into_inner()))
        .await
        .unwrap();

    // 期待收到业务拒绝（MsgType=4），逐字段校验
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::BUSINESS_REJECT, "平台不符应回业务拒绝");
    assert_eq!(&body[0..3], b"020", "ApplID 应回填委托值");
    let ref_msg_type = u32::from_be_bytes(body[37..41].try_into().unwrap());
    assert_eq!(ref_msg_type, msg_type::NEW_ORDER_BOND_REPO, "RefMsgType 应是被拒消息类型");
    assert_eq!(&body[41..51], b"PLATREJ01 ", "BusinessRejectRefID 应回填 ClOrdID");
    let reason = u16::from_be_bytes(body[51..53].try_into().unwrap());
    assert_eq!(reason, 20108, "拒单码应为 20108");
    let lossy = String::from_utf8_lossy(&body[53..103]);
    let text = lossy.trim_end_matches(char::from(0));
    let text = text.trim_end();
    assert!(text.contains("平台非法"), "拒单原因应参照 tgw_error.csv：{}", text);

    // 统计校验：委托未入受理流程
    let snap = engine.snapshot().await;
    let p = &snap.gateways[0].platforms[0];
    assert_eq!(p.stats.orders, 0, "平台不符的订单不应计入订单数");
    assert_eq!(p.stats.business_rejects, 1, "应计入业务拒绝数");

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试五：回报同步（5.2）——无历史回报时不重发也不回任何结束标记
/// （按实测真实交易所行为，网关发完历史回报后不回“回报结束(7)”），
/// 读应超时；含非法分区号时整条同步消息丢弃，回 20106 业务拒绝（规范 3.15）。
#[tokio::test]
async fn test_report_sync_no_reply_without_history() {
    let dir = std::env::temp_dir().join(format!("simx_test_sync_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());
    let gw = engine
        .save_gateway(test_gateway(
            18105,
            1,
            StrategyConfig { mode: StrategyMode::FullSingle, ..Default::default() },
        ))
        .await
        .unwrap();
    engine.start_gateway(&gw.id).await.unwrap();
    let mut stream = connect_and_logon(18105).await;

    // 回报同步请求：1 个分区（分区号 1，期望记录号 1）
    let mut w = BodyWriter::new();
    w.u32(1); // NoPartitions
    w.i32(1); // PartitionNo
    w.i64(1); // ReportIndex
    stream
        .write_all(&protocol::frame(msg_type::REPORT_SYNC, &w.into_inner()))
        .await
        .unwrap();

    // 无历史回报：不重发任何报文，也不回“回报结束”等结束标记，
    // 短暂等待后读应超时
    let early = tokio::time::timeout(Duration::from_millis(400), read_frame(&mut stream)).await;
    assert!(
        early.is_err(),
        "无历史回报时同步请求不应触发任何响应（真实交易所不发回报结束消息）"
    );

    // 非法分区号：整条同步消息丢弃，回 20106 业务拒绝
    let mut w = BodyWriter::new();
    w.u32(1);
    w.i32(99); // 不存在的分区号
    w.i64(1);
    stream
        .write_all(&protocol::frame(msg_type::REPORT_SYNC, &w.into_inner()))
        .await
        .unwrap();
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::BUSINESS_REJECT, "非法分区号应回业务拒绝");
    let ref_msg_type = u32::from_be_bytes(body[37..41].try_into().unwrap());
    assert_eq!(ref_msg_type, msg_type::REPORT_SYNC, "RefMsgType 应是被拒的同步消息类型");
    let reason = u16::from_be_bytes(body[51..53].try_into().unwrap());
    assert_eq!(reason, 20106, "拒单码应为 20106");

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试六（重发）：回报同步按 begin 重发历史回报——先报一笔单产生两条回报
/// （确认记录号 1、成交记录号 2），断开重连后同步 begin=2，应原样重发
/// 记录号 >= 2 的历史回报（保留原记录号）；发完不回“回报结束(7)”。
#[tokio::test]
async fn test_report_sync_resends_history_by_begin() {
    let dir = std::env::temp_dir().join(format!("simx_test_rsync_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());
    let gw = engine
        .save_gateway(test_gateway(
            18108,
            1,
            StrategyConfig { mode: StrategyMode::FullSingle, ..Default::default() },
        ))
        .await
        .unwrap();
    engine.start_gateway(&gw.id).await.unwrap();

    // 第一段连接：报一笔单，收确认(200102, 记录号1) + 成交(200115, 记录号2)
    let mut s1 = connect_and_logon(18108).await;
    s1.write_all(&new_order_frame("RESEND001", 500_00, 12_3400, b'1'))
        .await
        .unwrap();
    let (mt, body) = read_frame(&mut s1).await;
    assert_eq!(mt, msg_type::EXEC_RPT_CASH_ACK);
    assert_eq!(
        i64::from_be_bytes(body[4..12].try_into().unwrap()),
        1,
        "第一条回报记录号应为 1"
    );
    let (mt, body) = read_frame(&mut s1).await;
    assert_eq!(mt, msg_type::EXEC_RPT_CASH_TRADE);
    assert_eq!(
        i64::from_be_bytes(body[4..12].try_into().unwrap()),
        2,
        "第二条回报记录号应为 2"
    );
    drop(s1); // 断开
    tokio::time::sleep(Duration::from_millis(500)).await; // 等旧会话清理完（单连接限制）

    // 重连 + 同步 begin=2：只重发记录号 2 的成交回报，之后不再有任何报文
    let mut s2 = connect_and_logon(18108).await;
    let mut w = BodyWriter::new();
    w.u32(1);
    w.i32(1);
    w.i64(2); // 期望从记录号 2 开始
    s2.write_all(&protocol::frame(msg_type::REPORT_SYNC, &w.into_inner()))
        .await
        .unwrap();
    let (mt, body) = read_frame(&mut s2).await;
    assert_eq!(mt, msg_type::EXEC_RPT_CASH_TRADE, "应重发记录号 2 的成交回报");
    assert_eq!(
        i64::from_be_bytes(body[4..12].try_into().unwrap()),
        2,
        "重发帧应保留原记录号 2"
    );
    // 发完历史回报后不回“回报结束”等结束标记（真实交易所行为）
    let after = tokio::time::timeout(Duration::from_millis(400), read_frame(&mut s2)).await;
    assert!(after.is_err(), "重发完后不应再收到任何报文（无回报结束消息）");

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试七（多分区重发 + 分区内连续）：双分区平台（分区号 1、2），
/// 两个证券分别哈希落到不同分区，各产生 2 条回报；重连后按分区同步
/// begin=1，应每个分区独立重发（记录号各自从 1 连续），互不串扰。
#[tokio::test]
async fn test_report_sync_resends_per_partition_contiguous() {
    let dir = std::env::temp_dir().join(format!("simx_test_pp_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());
    let mut gw = test_gateway(
        18109,
        1,
        StrategyConfig { mode: StrategyMode::FullSingle, ..Default::default() },
    );
    gw.platforms[0].partition_nos = "1,2".into();
    let gw = engine.save_gateway(gw).await.unwrap();
    engine.start_gateway(&gw.id).await.unwrap();

    // 用与后端相同的哈希（partition_for）挑两个落不同分区的证券
    let pcfg = &engine.snapshot().await.gateways[0].config.platforms[0];
    let mut sec_a = String::new();
    let mut sec_b = String::new();
    let mut pa = 0;
    let mut pb = 0;
    for i in 1..=60 {
        let sec = format!("{:06}", i);
        let p = pcfg.partition_for(&sec);
        if sec_a.is_empty() {
            sec_a = sec;
            pa = p;
        } else if p != pa && sec_b.is_empty() {
            sec_b = sec;
            pb = p;
            break;
        }
    }
    assert!(!sec_b.is_empty(), "应能找到分属两个分区的证券");
    assert_ne!(pa, pb, "两个证券必须落在不同分区");

    // 第一段连接：两个证券各报一笔，各自产生 2 条回报（分区内记录号 1、2）
    let mut s1 = connect_and_logon(18109).await;
    s1.write_all(&new_order_frame_sec("PA0000001", &sec_a, 500_00, 12_3400, b'1'))
        .await
        .unwrap();
    s1.write_all(&new_order_frame_sec("PB0000001", &sec_b, 500_00, 12_3400, b'1'))
        .await
        .unwrap();
    let mut got = std::collections::HashMap::new();
    for _ in 0..4 {
        let (mt, body) = read_frame(&mut s1).await;
        let p = i32::from_be_bytes(body[0..4].try_into().unwrap());
        let ri = i64::from_be_bytes(body[4..12].try_into().unwrap());
        let e = got.entry(p).or_insert_with(Vec::new);
        e.push((mt, ri));
    }
    // 每个分区应收到记录号 1、2 各一条（连续）
    for (p, list) in &got {
        let mut idx: Vec<i64> = list.iter().map(|x| x.1).collect();
        idx.sort_unstable();
        assert_eq!(idx, vec![1, 2], "分区 {} 的记录号应连续 1、2", p);
    }
    assert_eq!(got.len(), 2, "两个分区都应收到回报");
    drop(s1);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 重连 + 两个分区一起同步 begin=1：各分区独立重发 2 条 + 回报结束
    let mut s2 = connect_and_logon(18109).await;
    let mut w = BodyWriter::new();
    w.u32(2);
    w.i32(pa);
    w.i64(1);
    w.i32(pb);
    w.i64(1);
    s2.write_all(&protocol::frame(msg_type::REPORT_SYNC, &w.into_inner()))
        .await
        .unwrap();
    // 各分区按请求顺序重发 4 条回报（发完不回“回报结束”等结束标记）
    let mut resent = std::collections::HashMap::new();
    for _ in 0..4 {
        let (mt, body) = read_frame(&mut s2).await;
        let p = i32::from_be_bytes(body[0..4].try_into().unwrap());
        let ri = i64::from_be_bytes(body[4..12].try_into().unwrap());
        let e = resent.entry(p).or_insert_with(Vec::new);
        e.push((mt, ri));
    }
    for (p, list) in &resent {
        let mut idx: Vec<i64> = list.iter().map(|x| x.1).collect();
        idx.sort_unstable();
        assert_eq!(idx, vec![1, 2], "分区 {} 重发的记录号应连续 1、2", p);
    }
    assert_eq!(resent.len(), 2, "两个分区都应被重发");
    // 发完 4 条历史回报后不应再有报文（真实交易所不发回报结束消息）
    let after = tokio::time::timeout(Duration::from_millis(400), read_frame(&mut s2)).await;
    assert!(after.is_err(), "重发完后不应再收到任何报文（无回报结束消息）");

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试八（关闭捕获）：平台未开启“展示收发报文/持久化”时没有捕获缓存，
/// 回报同步不重发任何历史回报（重发功能随捕获开关一起关闭），
/// 也不回任何结束标记。
#[tokio::test]
async fn test_report_sync_skipped_without_capture() {
    let dir = std::env::temp_dir().join(format!("simx_test_nocap_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());
    let mut gw = test_gateway(
        18110,
        1,
        StrategyConfig { mode: StrategyMode::FullSingle, ..Default::default() },
    );
    // 关闭两个捕获开关：记录器不创建，重发缓存为空
    gw.platforms[0].show_packets = false;
    gw.platforms[0].persist_packets = false;
    let gw = engine.save_gateway(gw).await.unwrap();
    engine.start_gateway(&gw.id).await.unwrap();

    // 第一段连接：报一笔单，收 2 条回报
    let mut s1 = connect_and_logon(18110).await;
    s1.write_all(&new_order_frame("NOCAP0001", 500_00, 12_3400, b'1'))
        .await
        .unwrap();
    let (mt, _) = read_frame(&mut s1).await;
    assert_eq!(mt, msg_type::EXEC_RPT_CASH_ACK);
    let (mt, _) = read_frame(&mut s1).await;
    assert_eq!(mt, msg_type::EXEC_RPT_CASH_TRADE);
    drop(s1);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 重连 + 同步 begin=1：无捕获缓存不重发，也不回任何结束标记，读应超时
    let mut s2 = connect_and_logon(18110).await;
    let mut w = BodyWriter::new();
    w.u32(1);
    w.i32(1);
    w.i64(1);
    s2.write_all(&protocol::frame(msg_type::REPORT_SYNC, &w.into_inner()))
        .await
        .unwrap();
    let early = tokio::time::timeout(Duration::from_millis(400), read_frame(&mut s2)).await;
    assert!(
        early.is_err(),
        "无捕获缓存时同步请求不应触发任何响应（不重发、不回结束标记）"
    );

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试九：5.6 交易会话状态——固定收益交易平台（平台号 6）登录成功后，
/// 除平台信息、平台状态外，还应收到交易会话状态消息（MsgType=10），
/// MarketSegmentID 第一位表示平台号，应为 6。
#[tokio::test]
async fn test_fixed_income_session_status() {
    let dir = std::env::temp_dir().join(format!("simx_test_sess_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());
    let gw = engine
        .save_gateway(test_gateway(
            18106,
            6, // 固定收益交易平台
            StrategyConfig { mode: StrategyMode::FullSingle, ..Default::default() },
        ))
        .await
        .unwrap();
    engine.start_gateway(&gw.id).await.unwrap();

    let mut stream = TcpStream::connect(("127.0.0.1", 18106)).await.expect("连接失败");
    let logon = Logon {
        sender_comp_id: "OMS_TEST".into(),
        target_comp_id: "SIMX_TGW".into(),
        heart_bt_int: 30,
        password: String::new(),
        default_appl_ver_id: "1.29".into(),
    };
    stream.write_all(&logon.encode()).await.unwrap();

    // 登录应答、平台信息、平台状态、交易会话状态（固定收益平台独有）
    let (mt, _) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::LOGON);
    let (mt, _) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::PLATFORM_INFO);
    let (mt, _) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::PLATFORM_STATE);
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::TRADING_SESSION_STATUS, "固定收益平台登录后应收到交易会话状态");
    // MarketID(8) 后即 MarketSegmentID(8)：首位为平台号 6
    assert_eq!(&body[8..9], b"6", "MarketSegmentID 首位应为平台号");
    // MarketID(8)+MarketSegmentID(8)+TradingSessionID(4)+TradingSessionSubID(4)+TradSesStatus(2)=26
    let start = i64::from_be_bytes(body[26..34].try_into().unwrap());
    let end = i64::from_be_bytes(body[34..42].try_into().unwrap());
    assert!(start > 0 && end > start, "交易会话起止时间应有效");

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试八（操作日志）：api::dispatch 处理的每条非轮询命令都带前端 IP 写入
/// <data_dir>/log/<YYYYMMDD>/ops.log，新建/删除网关均留痕。
#[tokio::test]
async fn test_oplog_records_dispatch_ops() {
    let dir = std::env::temp_dir().join(format!("simx_test_oplog_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());

    // 新建网关：应记“新建网关 XXX”
    let gw = simx_core::config::GatewayConfig {
        name: "日志测试网关".into(),
        ..Default::default()
    };
    let v = simx_core::api::dispatch(
        &engine,
        serde_json::json!({ "cmd": "save_gateway", "gateway": gw }),
        "192.168.0.1",
    )
    .await;
    assert_eq!(v["ok"], true, "保存网关应成功：{:?}", v);
    let date = chrono::Local::now().format("%Y%m%d");
    let path = dir.join("log").join(date.to_string()).join("ops.log");
    let content = std::fs::read_to_string(&path).expect("操作日志文件应存在");
    assert!(content.contains("192.168.0.1"), "日志应含前端 IP");
    assert!(content.contains("新建网关 日志测试网关"), "日志应含操作描述：{}", content);

    // 删除网关：应记“删除网关 日志测试网关”
    let id = v["data"]["id"].as_str().unwrap().to_string();
    let v2 = simx_core::api::dispatch(
        &engine,
        serde_json::json!({ "cmd": "delete_gateway", "id": id }),
        "192.168.0.1",
    )
    .await;
    assert_eq!(v2["ok"], true);
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("删除网关 日志测试网关"), "日志应含删除操作：{}", content);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试七：报价链路（4.6）——综合金融服务平台（平台号 2）上发协议交易报价
/// （100505，ApplID=056）应收到报价状态回报（200506，状态=接受）；
/// 若报价 ApplID 不属于本平台（如现货 010），则应回 20108 业务拒绝。
#[tokio::test]
async fn test_quote_platform2_accept_and_reject() {
    let dir = std::env::temp_dir().join(format!("simx_test_quote_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone());
    let gw = engine
        .save_gateway(test_gateway(
            18107,
            2, // 综合金融服务平台
            StrategyConfig { mode: StrategyMode::FullSingle, ..Default::default() },
        ))
        .await
        .unwrap();
    engine.start_gateway(&gw.id).await.unwrap();
    let mut stream = connect_and_logon(18107).await;

    // 构造协议交易报价（100505）：公共 106 字节 + 协议扩展 207 字节（表 4-65）
    let quote_frame = |appl_id: &str, quote_msg_id: &str| {
        let mut w = BodyWriter::new();
        w.str(appl_id, 3); // ApplID
        w.str("100001", 6); // SubmittingPBUID
        w.str("000001", 8); // SecurityID
        w.str("102", 4); // SecurityIDSource
        w.u16(1); // OwnerType
        w.str("01", 2); // ClearingFirm
        w.i64(protocol::now_timestamp()); // TransactTime
        w.str("", 8); // UserInfo
        w.str(quote_msg_id, 10); // QuoteMsgID
        w.str("0123456789AB", 12); // AccountID
        w.str("", 10); // QuoteReqID
        w.ch(b'1'); // QuoteType
        w.i64(12_3400); // BidPx
        w.i64(12_3500); // OfferPx
        w.i64(100_00); // BidSize
        w.i64(100_00); // OfferSize
        // ---- 4.6.1.2 协议交易扩展（表 4-65）----
        w.str("0001", 4); // BranchID
        w.str("", 10); // QuoteID
        w.str("", 16); // QuoteRespID
        w.ch(b'0'); // PrivateQuote
        w.i64(0); // ValidUntilTime
        w.ch(b'0'); // PriceType
        w.ch(b'1'); // CashMargin
        w.str("100002", 6); // CounterpartyPBUID
        w.str("", 160); // Memo
        protocol::frame(msg_type::QUOTE_AGREEMENT, &w.into_inner())
    };

    // 1) ApplID=056（协议交易报价，属于综合金融平台）→ 应被接受
    stream
        .write_all(&quote_frame("056", "QT00000001"))
        .await
        .unwrap();
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::QUOTE_STATUS_AGREEMENT, "报价应回报价状态回报");
    // QuoteStatus 位置：4+8+3+6+6+8+4+2+2+8+8+10+12+10 = 91
    let status = u16::from_be_bytes(body[91..93].try_into().unwrap());
    assert_eq!(status, 0, "报价状态应为接受");

    // 2) ApplID=010（现货集中竞价，属于平台 1）→ 应回 20108 业务拒绝
    stream
        .write_all(&quote_frame("010", "QT00000002"))
        .await
        .unwrap();
    let (mt, body) = read_frame(&mut stream).await;
    assert_eq!(mt, msg_type::BUSINESS_REJECT, "平台不符的报价应回业务拒绝");
    let ref_msg_type = u32::from_be_bytes(body[37..41].try_into().unwrap());
    assert_eq!(ref_msg_type, msg_type::QUOTE_AGREEMENT);
    assert_eq!(&body[41..51], b"QT00000002", "报价的业务层 ID 应为 QuoteMsgID（表 5-2）");
    let reason = u16::from_be_bytes(body[51..53].try_into().unwrap());
    assert_eq!(reason, 20108);

    let snap = engine.snapshot().await;
    let p = &snap.gateways[0].platforms[0];
    assert_eq!(p.stats.business_rejects, 1);

    engine.stop_gateway(&gw.id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
