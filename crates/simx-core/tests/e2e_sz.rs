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
    let mut w = BodyWriter::new();
    w.str("010", 3); // ApplID
    w.str("100001", 6); // SubmittingPBUID
    w.str("000001", 8); // SecurityID
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
fn test_gateway(port: u16, strategy: StrategyConfig) -> GatewayConfig {
    GatewayConfig {
        id: String::new(),
        name: "测试网关".into(),
        platforms: vec![PlatformConfig {
            id: String::new(),
            name: "现货集中竞价交易平台".into(),
            listen_host: "127.0.0.1".into(),
            port,
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
    let gw = engine.save_gateway(test_gateway(18103, strategy)).await.unwrap();
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
