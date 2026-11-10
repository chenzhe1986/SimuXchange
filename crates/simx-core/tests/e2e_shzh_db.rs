//! 上海综合网关（EzSTEP 数据库接口）端到端测试：真实 SQL Server。
//!
//! # 运行方式
//!
//! 设置以下环境变量后执行（未设置 Host/DB/User 时测试直接跳过，不影响 CI）：
//!
//! ```powershell
//! $env:SIMX_TEST_MSSQL_HOST="127.0.0.1"
//! $env:SIMX_TEST_MSSQL_PORT="1433"
//! $env:SIMX_TEST_MSSQL_DB="ezstep_test"
//! $env:SIMX_TEST_MSSQL_USER="sa"
//! $env:SIMX_TEST_MSSQL_PASSWORD="******"
//! cargo test -p simx-core --test e2e_shzh_db -- --nocapture
//! ```
//!
//! 测试会创建两张**唯一命名**的临时表（`simx_req_*` / `simx_exec_*`），
//! 结束后删除；不会碰现有接口表。账号需要有建表/删表权限。
//!
//! 覆盖链路：启动平台 → 柜台写入 STEP 新委托行 → 网关回写确认 →
//! 柜台读到 resptext/localrespnum → 成交表出现执行报告 → 撤单（终态拒单）。

use std::time::Duration;

use simx_core::config::{
    DbConfig, GatewayCategory, GatewayConfig, PlatformConfig, StrategyConfig, StrategyMode,
};
use simx_core::engine::Engine;
use simx_core::orderbook::OrderStatus;
use simx_core::shzh::protocol::{
    tag, Party, StepBuilder, StepMessage, MSG_CANCEL, MSG_NEW_ORDER,
};
use simx_core::sz::session::ManualReportKind;
use tiberius::{AuthMethod, Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type DbClient = Client<Compat<TcpStream>>;

/// 从环境变量读测试库配置；缺少关键项返回 None（测试跳过）
fn test_db_config() -> Option<DbConfig> {
    let host = std::env::var("SIMX_TEST_MSSQL_HOST").ok()?;
    let database = std::env::var("SIMX_TEST_MSSQL_DB").ok()?;
    let user = std::env::var("SIMX_TEST_MSSQL_USER").ok()?;
    let password = std::env::var("SIMX_TEST_MSSQL_PASSWORD").unwrap_or_default();
    let port = std::env::var("SIMX_TEST_MSSQL_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1433);
    Some(DbConfig {
        host,
        port,
        database,
        user,
        password,
        req_resp_table: String::new(), // 测试里替换成唯一表名
        exec_report_table: String::new(),
        poll_interval_ms: 20,
    })
}

async fn connect(cfg: &DbConfig) -> DbClient {
    let mut config = Config::new();
    config.host(cfg.host.trim());
    config.port(cfg.port);
    config.database(cfg.database.trim());
    config.authentication(AuthMethod::sql_server(
        cfg.user.trim(),
        cfg.password.as_str(),
    ));
    config.trust_cert();
    // 测试库地址可能含 schema；连接只用 host:port
    let addr = format!("{}:{}", cfg.host.trim(), cfg.port);
    let tcp = TcpStream::connect(&addr).await.expect("连接测试 SQL Server");
    tcp.set_nodelay(true).ok();
    Client::connect(config, tcp.compat_write())
        .await
        .expect("登录测试 SQL Server")
}

async fn ddl(client: &mut DbClient, sql: &str) {
    client
        .simple_query(sql.to_string())
        .await
        .expect("执行 DDL")
        .into_results()
        .await
        .expect("DDL 结果");
}

async fn insert_req(
    client: &mut DbClient,
    table: &str,
    reqnum: i32,
    reqid: &str,
    reff: &str,
    text: &str,
) {
    let sql = format!(
        "INSERT INTO [{}] (reqnum, reqid, reff, securityid, pbu, recordtimestamp, \
         ordstatus, remark, localrespnum, reqtext, resptext) \
         VALUES (@P1, @P2, @P3, '510300', 'PBU01', '20260101-09:30:00.000', 'R', '', 0, @P4, '')",
        table
    );
    client
        .execute(&sql, &[&reqnum, &reqid, &reff, &text])
        .await
        .expect("插入委托行");
}

/// 反复查询直到谓词返回 true 或超时；返回命中的行（超时返回 None）
async fn wait_rows(
    client: &mut DbClient,
    sql: String,
    timeout_ms: u64,
    ok: impl Fn(&[tiberius::Row]) -> bool,
) -> Option<Vec<tiberius::Row>> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Ok(stream) = client.simple_query(sql.clone()).await {
            if let Ok(rows) = stream.into_first_result().await {
                if ok(&rows) {
                    return Some(rows);
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn sample_order_step(cl_ord_id: &str) -> String {
    sample_order_step_qty(cl_ord_id, 1000.0)
}

fn sample_order_step_qty(cl_ord_id: &str, qty: f64) -> String {
    StepBuilder::new(MSG_NEW_ORDER)
        .str(tag::CL_ORD_ID, cl_ord_id)
        .str(tag::SECURITY_ID, "510300")
        .str(tag::SIDE, "1")
        .str(tag::ORD_TYPE, "2")
        .price(tag::PRICE, 4.123)
        .int(tag::ORDER_QTY, qty.round() as i64)
        .parties(&[
            Party::new("A123456789", 5),
            Party::new("PBU01", 1),
            Party::new("888", 4001),
        ])
        .build()
}

fn sample_cancel_step(cl_ord_id: &str, orig: &str) -> String {
    StepBuilder::new(MSG_CANCEL)
        .str(tag::CL_ORD_ID, cl_ord_id)
        .str(tag::ORIG_CL_ORD_ID, orig)
        .str(tag::SECURITY_ID, "510300")
        .str(tag::SIDE, "1")
        .int(tag::ORDER_QTY, 1000)
        .parties(&[
            Party::new("A123456789", 5),
            Party::new("PBU01", 1),
        ])
        .build()
}

#[tokio::test]
async fn shzh_db_full_flow() {
    let Some(mut db_cfg) = test_db_config() else {
        eprintln!("跳过：未设置 SIMX_TEST_MSSQL_HOST/DB/USER 环境变量");
        return;
    };
    let uniq = format!("{}_{}", std::process::id(), chrono::Local::now().format("%H%M%S"));
    let req_table = format!("simx_req_{}", uniq);
    let exec_table = format!("simx_exec_{}", uniq);

    let mut raw = connect(&db_cfg).await;
    ddl(
        &mut raw,
        &format!(
            "CREATE TABLE [{}] (reqnum int NOT NULL PRIMARY KEY, reqid char(3) NOT NULL, \
             reff char(10) NOT NULL, securityid char(8) NULL, pbu char(5) NULL, \
             recordtimestamp char(21) NULL, ordstatus char(1) NOT NULL, remark char(5) NULL, \
             localrespnum int NOT NULL DEFAULT 0, reqtext varchar(1024) NULL, resptext varchar(1024) NULL)",
            req_table
        ),
    )
    .await;
    ddl(
        &mut raw,
        &format!(
            "CREATE TABLE [{}] (execnum int NOT NULL PRIMARY KEY, bcasttype char(2) NOT NULL, \
             setid char(3) NOT NULL, seqnum int NOT NULL, recordtimestamp char(21) NULL, \
             execreporttext varchar(1024) NULL)",
            exec_table
        ),
    )
    .await;

    // 启动引擎与综合平台
    let data_dir = std::env::temp_dir().join(format!("simx_shzh_test_{}", uniq));
    std::fs::create_dir_all(&data_dir).expect("建临时数据目录");
    let engine = Engine::new(data_dir.clone());
    db_cfg.req_resp_table = req_table.clone();
    db_cfg.exec_report_table = exec_table.clone();
    let platform = PlatformConfig {
        id: "p1".into(),
        name: "综合测试平台".into(),
        platform_type: 2,
        listen_host: String::new(),
        port: 0,
        db: Some(db_cfg.clone()),
        comp_id: String::new(),
        check_password: false,
        password: String::new(),
        partition_no: 0,
        partition_nos: String::new(),
        show_packets: false,
        persist_packets: false,
        cache_orders: true,
        strategy: StrategyConfig {
            mode: StrategyMode::FullSingle,
            ..Default::default()
        },
        created_at: String::new(),
        updated_at: String::new(),
    };
    let gw = GatewayConfig {
        id: "g1".into(),
        name: "综合测试网关".into(),
        category: GatewayCategory::Shzh,
        platforms: vec![platform],
        ..Default::default()
    };
    engine.save_gateway(gw).await.expect("保存网关配置");
    engine.start_gateway("g1").await.expect("启动综合网关");

    // 写入新委托 → 等确认回报
    let order_text = sample_order_step("ORDER00001");
    insert_req(&mut raw, &req_table, 1, "PFP", "ORDER00001", &order_text).await;
    let rows = wait_rows(
        &mut raw,
        format!(
            "SELECT ordstatus, localrespnum, resptext FROM [{}] WHERE reqnum = 1",
            req_table
        ),
        5000,
        |rows| {
            let Some(row) = rows.first() else { return false };
            let resp: Option<&str> = row.get("resptext");
            let num: Option<i32> = row.get("localrespnum");
            let status: Option<&str> = row.get("ordstatus");
            resp.is_some_and(|t| !t.trim().is_empty())
                && num.unwrap_or(0) > 0
                && status != Some("R")
        },
    )
    .await
    .expect("5 秒内未等到确认回报写入 reqresp");

    // 校验确认报文内容
    let row = rows.first().expect("确认行存在");
    let resp: &str = row.get("resptext").expect("resptext");
    let msg = StepMessage::parse(resp).expect("确认 STEP 可解析");
    assert_eq!(msg.msg_type, "8");
    assert_eq!(msg.get(tag::EXEC_TYPE), Some("0"));
    assert_eq!(msg.get(tag::ORD_STATUS), Some("0"));
    assert_eq!(msg.get(tag::CL_ORD_ID), Some("ORDER00001"));

    // 等成交写入 execreport
    let rows = wait_rows(
        &mut raw,
        format!(
            "SELECT bcasttype, setid, seqnum, execreporttext FROM [{}]",
            exec_table
        ),
        5000,
        |rows| !rows.is_empty(),
    )
    .await
    .expect("5 秒内未等到成交写入 execreport");

    let row = rows.first().expect("成交行存在");
    let bcast: &str = row.get("bcasttype").expect("bcasttype");
    let setid: &str = row.get("setid").expect("setid");
    let seq: i32 = row.get("seqnum").expect("seqnum");
    assert_eq!(bcast.trim(), "19", "PFP 的 bcasttype 应为 19");
    assert_eq!(setid.trim(), "103", "PFP 的 setid 应为 103");
    assert_eq!(seq, 1);
    let text: &str = row.get("execreporttext").expect("execreporttext");
    let trade = StepMessage::parse(text).expect("成交 STEP 可解析");
    assert_eq!(trade.get(tag::EXEC_TYPE), Some("F"));
    assert_eq!(trade.get(tag::LAST_QTY), Some("1000"));
    assert_eq!(trade.get(tag::LEAVES_QTY), Some("0"));

    // 订单缓存：订单应为全部成交
    let orders = engine.orders("g1", "p1").await.expect("查订单缓存");
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].cl_ord_id, "ORDER00001");
    assert!(orders[0].order_id.len() > 0, "确认后应回填交易所订单号");

    // 撤单：原单已全成（终态），默认撤单模式应回撤单拒绝(9)
    let cancel_text = sample_cancel_step("CANCEL0001", "ORDER00001");
    insert_req(
        &mut raw,
        &req_table,
        2,
        "PFP",
        "CANCEL0001",
        &cancel_text,
    )
    .await;
    let rows = wait_rows(
        &mut raw,
        format!(
            "SELECT resptext FROM [{}] WHERE reqnum = 2",
            req_table
        ),
        5000,
        |rows| {
            rows.first()
                .and_then(|r| r.get::<&str, _>("resptext"))
                .is_some_and(|t| !t.trim().is_empty())
        },
    )
    .await
    .expect("5 秒内未等到撤单响应");
    let text: &str = rows.first().expect("撤单行").get("resptext").expect("resptext");
    let msg = StepMessage::parse(text).expect("撤单 STEP 可解析");
    assert_eq!(msg.msg_type, "9", "终态订单撤单应回 OrderCancelReject(9)");
    assert_eq!(msg.get(tag::CXL_REJ_REASON), Some("1"));

    // ---- 撤单成功：切到只确认让原单保持在途，再发撤单 ----
    engine
        .update_strategy(
            "g1",
            "p1",
            StrategyConfig {
                mode: StrategyMode::AckOnly,
                ..Default::default()
            },
        )
        .await
        .expect("热更新为只回确认");
    let order2 = sample_order_step("ORDER00002");
    insert_req(&mut raw, &req_table, 3, "PFP", "ORDER00002", &order2).await;
    let _ = wait_rows(
        &mut raw,
        format!(
            "SELECT resptext FROM [{}] WHERE reqnum = 3",
            req_table
        ),
        5000,
        |rows| {
            rows.first()
                .and_then(|r| r.get::<&str, _>("resptext"))
                .is_some_and(|t| !t.trim().is_empty())
        },
    )
    .await
    .expect("ORDER00002 确认回报");

    let cancel2 = sample_cancel_step("CANCEL0002", "ORDER00002");
    insert_req(&mut raw, &req_table, 4, "PFP", "CANCEL0002", &cancel2).await;
    let rows = wait_rows(
        &mut raw,
        format!("SELECT resptext FROM [{}] WHERE reqnum = 4", req_table),
        5000,
        |rows| {
            rows.first()
                .and_then(|r| r.get::<&str, _>("resptext"))
                .is_some_and(|t| !t.trim().is_empty())
        },
    )
    .await
    .expect("撤单成功响应");
    let text: &str = rows.first().expect("撤单行").get("resptext").expect("resptext");
    let msg = StepMessage::parse(text).expect("撤单成功 STEP 可解析");
    assert_eq!(msg.msg_type, "8");
    assert_eq!(msg.get(tag::EXEC_TYPE), Some("6"));
    assert_eq!(msg.get(tag::ORD_STATUS), Some("6"));
    assert_eq!(msg.get(tag::ORIG_CL_ORD_ID), Some("ORDER00002"));
    assert_eq!(msg.get(tag::LAST_QTY), Some("1000"));
    let orders = engine.orders("g1", "p1").await.expect("查订单缓存");
    let o2 = orders
        .iter()
        .find(|o| o.cl_ord_id == "ORDER00002")
        .expect("ORDER00002 应在缓存");
    assert_eq!(o2.status, OrderStatus::Cancelled);

    // ---- 关闭自动回报 + 界面手动确认/成交 ----
    engine
        .update_strategy(
            "g1",
            "p1",
            StrategyConfig {
                mode: StrategyMode::NoAutoReply,
                ..Default::default()
            },
        )
        .await
        .expect("热更新为关闭自动回报");
    let order3 = sample_order_step("ORDER00003");
    insert_req(&mut raw, &req_table, 5, "PFP", "ORDER00003", &order3).await;
    let _ = wait_rows(
        &mut raw,
        format!(
            "SELECT ordstatus, localrespnum FROM [{}] WHERE reqnum = 5",
            req_table
        ),
        5000,
        |rows| {
            let Some(r) = rows.first() else { return false };
            let st: Option<&str> = r.get("ordstatus");
            let num: Option<i32> = r.get("localrespnum");
            st == Some("O") && num.unwrap_or(-1) == 0
        },
    )
    .await
    .expect("关闭自动回报应把行标记为已处理且不分配响应序号");

    let desc = engine
        .send_report(
            "g1",
            "p1",
            "ORDER00003",
            ManualReportKind::Ack,
            None,
            None,
            None,
            false,
        )
        .await
        .expect("手动确认应写库成功");
    assert!(!desc.is_empty(), "手动确认应返回描述");
    let _ = wait_rows(
        &mut raw,
        format!(
            "SELECT localrespnum, resptext FROM [{}] WHERE reqnum = 5",
            req_table
        ),
        5000,
        |rows| {
            let Some(r) = rows.first() else { return false };
            let num: Option<i32> = r.get("localrespnum");
            let resp: Option<&str> = r.get("resptext");
            num.unwrap_or(0) > 0 && resp.is_some_and(|t| !t.trim().is_empty())
        },
    )
    .await
    .expect("手动确认应回写 reqresp");
    let rows = raw
        .simple_query(format!(
            "SELECT resptext FROM [{}] WHERE reqnum = 5",
            req_table
        ))
        .await
        .expect("查手动确认")
        .into_first_result()
        .await
        .expect("手动确认结果");
    let text: &str = rows.first().expect("订单行").get("resptext").expect("resptext");
    let msg = StepMessage::parse(text).expect("手动确认 STEP 可解析");
    assert_eq!(msg.get(tag::EXEC_TYPE), Some("0"));

    engine
        .send_report(
            "g1",
            "p1",
            "ORDER00003",
            ManualReportKind::Trade,
            Some(500.0),
            Some(4.2),
            None,
            false,
        )
        .await
        .expect("手动成交应写库成功");
    let _ = wait_rows(
        &mut raw,
        format!("SELECT COUNT(*) AS n FROM [{}]", exec_table),
        5000,
        |rows| {
            rows.first()
                .and_then(|r| r.get::<i32, _>("n"))
                .unwrap_or(0)
                >= 2
        },
    )
    .await
    .expect("手动成交应插入 execreport");
    let rows = raw
        .simple_query(format!(
            "SELECT TOP (1) execreporttext FROM [{}] ORDER BY execnum DESC",
            exec_table
        ))
        .await
        .expect("查手动成交")
        .into_first_result()
        .await
        .expect("手动成交结果");
    let text: &str = rows
        .first()
        .expect("成交行")
        .get("execreporttext")
        .expect("execreporttext");
    let msg = StepMessage::parse(text).expect("手动成交 STEP 可解析");
    assert_eq!(msg.get(tag::EXEC_TYPE), Some("F"));
    assert_eq!(msg.get(tag::LAST_QTY), Some("500"));
    assert_eq!(msg.get(tag::LEAVES_QTY), Some("500"));

    // 清理
    engine.stop_gateway("g1").await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;
    ddl(&mut raw, &format!("DROP TABLE [{}]", req_table)).await;
    ddl(&mut raw, &format!("DROP TABLE [{}]", exec_table)).await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// ETF 申赎：用一份上交所 PCF 清单验证现金申赎（EEC）与实物申赎（IEC）的
/// 执行报告记录（类型1 ETF 成交 / 类型2 成份券 / 类型3 资金替代）。
///
/// 清单：510300，最小申赎单位 900000；成份券
/// - 000001：允许现金替代，每篮子 1600 股，替代金额 17408，溢价/折价 10%；
/// - 600000：必须现金替代，替代金额 4950；
/// - 000002：禁止现金替代，每篮子 1100 股（实物申赎时过户）。
#[tokio::test]
async fn shzh_db_etf_pcf_flow() {
    let Some(mut db_cfg) = test_db_config() else {
        eprintln!("跳过：未设置 SIMX_TEST_MSSQL_HOST/DB/USER 环境变量");
        return;
    };
    let uniq = format!(
        "{}_{}",
        std::process::id(),
        chrono::Local::now().format("%H%M%S%f")
    );
    let req_table = format!("simx_req_{}", uniq);
    let exec_table = format!("simx_exec_{}", uniq);
    let pcf_dir = std::env::temp_dir().join(format!("simx_sse_pcf_{}", uniq));
    std::fs::create_dir_all(&pcf_dir).expect("建 PCF 临时目录");
    std::fs::write(
        pcf_dir.join("ssepcf_510300_20261109.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SSEPortfolioCompositionFile>
  <FundInstrumentID>510300</FundInstrumentID>
  <CreationRedemptionUnit>900000</CreationRedemptionUnit>
  <CreationRedemptionMechanism>1</CreationRedemptionMechanism>
  <RecordNumber>3</RecordNumber>
  <ComponentList>
    <Component>
      <InstrumentID>000001</InstrumentID>
      <InstrumentName>平安银行</InstrumentName>
      <Quantity>1600</Quantity>
      <SubstitutionFlag>1</SubstitutionFlag>
      <CreationPremiumRate>0.1</CreationPremiumRate>
      <RedemptionDiscountRate>0.1</RedemptionDiscountRate>
      <SubstitutionCashAmount>17408</SubstitutionCashAmount>
      <UnderlyingSecurityID>102</UnderlyingSecurityID>
    </Component>
    <Component>
      <InstrumentID>600000</InstrumentID>
      <InstrumentName>浦发银行</InstrumentName>
      <Quantity>0</Quantity>
      <SubstitutionFlag>2</SubstitutionFlag>
      <SubstitutionCashAmount>4950</SubstitutionCashAmount>
      <UnderlyingSecurityID>101</UnderlyingSecurityID>
    </Component>
    <Component>
      <InstrumentID>000002</InstrumentID>
      <InstrumentName>万科A</InstrumentName>
      <Quantity>1100</Quantity>
      <SubstitutionFlag>0</SubstitutionFlag>
      <UnderlyingSecurityID>102</UnderlyingSecurityID>
    </Component>
  </ComponentList>
</SSEPortfolioCompositionFile>"#,
    )
    .expect("写 PCF 文件");

    let mut raw = connect(&db_cfg).await;
    ddl(
        &mut raw,
        &format!(
            "CREATE TABLE [{}] (reqnum int NOT NULL PRIMARY KEY, reqid char(3) NOT NULL, \
             reff char(10) NOT NULL, securityid char(8) NULL, pbu char(5) NULL, \
             recordtimestamp char(21) NULL, ordstatus char(1) NOT NULL, remark char(5) NULL, \
             localrespnum int NOT NULL DEFAULT 0, reqtext varchar(1024) NULL, resptext varchar(1024) NULL)",
            req_table
        ),
    )
    .await;
    ddl(
        &mut raw,
        &format!(
            "CREATE TABLE [{}] (execnum int NOT NULL PRIMARY KEY, bcasttype char(2) NOT NULL, \
             setid char(3) NOT NULL, seqnum int NOT NULL, recordtimestamp char(21) NULL, \
             execreporttext varchar(1024) NULL)",
            exec_table
        ),
    )
    .await;

    let data_dir = std::env::temp_dir().join(format!("simx_shzh_etf_{}", uniq));
    std::fs::create_dir_all(&data_dir).expect("建临时数据目录");
    let engine = Engine::new(data_dir.clone());
    db_cfg.req_resp_table = req_table.clone();
    db_cfg.exec_report_table = exec_table.clone();
    let platform = PlatformConfig {
        id: "p1".into(),
        name: "综合ETF测试平台".into(),
        platform_type: 2,
        listen_host: String::new(),
        port: 0,
        db: Some(db_cfg.clone()),
        comp_id: String::new(),
        check_password: false,
        password: String::new(),
        partition_no: 0,
        partition_nos: String::new(),
        show_packets: false,
        persist_packets: false,
        cache_orders: true,
        strategy: StrategyConfig {
            mode: StrategyMode::FullSingle,
            etf_pcf_dir: pcf_dir.to_string_lossy().into_owned(),
            ..Default::default()
        },
        created_at: String::new(),
        updated_at: String::new(),
    };
    let gw = GatewayConfig {
        id: "g1".into(),
        name: "综合ETF测试网关".into(),
        category: GatewayCategory::Shzh,
        platforms: vec![platform],
        ..Default::default()
    };
    engine.save_gateway(gw).await.expect("保存 ETF 网关");
    engine.start_gateway("g1").await.expect("启动 ETF 网关");

    // ---- 现金申购 EEC：期望 ack + 类型1 + 类型3 ----
    insert_req(
        &mut raw,
        &req_table,
        1,
        "EEC",
        "ETF00001",
        &sample_order_step_qty("ETF00001", 900000.0),
    )
    .await;
    let _ = wait_rows(
        &mut raw,
        format!("SELECT COUNT(*) AS n FROM [{}]", exec_table),
        5000,
        |rows| {
            rows.first()
                .and_then(|r| r.get::<i32, _>("n"))
                .unwrap_or(0)
                >= 2
        },
    )
    .await
    .expect("EEC 应写入两条成交回报");
    let rows = raw
        .simple_query(format!(
            "SELECT execreporttext FROM [{}] ORDER BY execnum",
            exec_table
        ))
        .await
        .expect("查 EEC 成交")
        .into_first_result()
        .await
        .expect("EEC 结果");
    let eec: Vec<StepMessage> = rows
        .iter()
        .filter_map(|r| r.get::<&str, _>("execreporttext"))
        .map(|t| StepMessage::parse(t).expect("EEC STEP"))
        .filter(|m| m.get(tag::CL_ORD_ID) == Some("ETF00001"))
        .collect();
    assert_eq!(eec.len(), 2, "现金申购应为类型1 + 类型3 两条");
    assert_eq!(eec[0].get(tag::TRADE_REPORT_TYPE), Some("1"));
    assert_eq!(eec[0].get(tag::LAST_QTY), Some("900000"));
    assert_eq!(eec[0].get(tag::SIDE), Some("1"));
    assert_eq!(eec[1].get(tag::TRADE_REPORT_TYPE), Some("3"));
    assert_eq!(eec[1].get(tag::TOTAL_VALUE_TRADED), Some("24098.800"));
    assert_eq!(eec[1].get(tag::NO_LEGS), Some("1"));
    assert_eq!(eec[1].get(tag::MARGIN_AMT_TYPE), Some("9"));

    // ---- 实物申购 IEC：期望 ack + 类型1 + 2 只成份券类型2 + 类型3 ----
    insert_req(
        &mut raw,
        &req_table,
        2,
        "IEC",
        "ETF00002",
        &sample_order_step_qty("ETF00002", 900000.0),
    )
    .await;
    let _ = wait_rows(
        &mut raw,
        format!("SELECT COUNT(*) AS n FROM [{}]", exec_table),
        5000,
        |rows| {
            rows.first()
                .and_then(|r| r.get::<i32, _>("n"))
                .unwrap_or(0)
                >= 6
        },
    )
    .await
    .expect("IEC 应再写入四条成交回报");
    let rows = raw
        .simple_query(format!(
            "SELECT execreporttext FROM [{}] ORDER BY execnum",
            exec_table
        ))
        .await
        .expect("查 IEC 成交")
        .into_first_result()
        .await
        .expect("IEC 结果");
    let iec: Vec<StepMessage> = rows
        .iter()
        .filter_map(|r| r.get::<&str, _>("execreporttext"))
        .map(|t| StepMessage::parse(t).expect("IEC STEP"))
        .filter(|m| m.get(tag::CL_ORD_ID) == Some("ETF00002"))
        .collect();
    assert_eq!(iec.len(), 4, "实物申购应为类型1 + 2 只成份券 + 类型3");
    assert_eq!(iec[0].get(tag::TRADE_REPORT_TYPE), Some("1"));
    assert_eq!(iec[0].get(tag::LAST_QTY), Some("900000"));
    assert_eq!(iec[0].get(tag::SIDE), Some("1"));
    let components: Vec<&StepMessage> = iec
        .iter()
        .filter(|m| m.get(tag::TRADE_REPORT_TYPE) == Some("2"))
        .collect();
    assert_eq!(components.len(), 2);
    assert_eq!(components[0].get(tag::SECURITY_ALT_ID), Some("000001"));
    assert_eq!(components[0].get(tag::LAST_QTY), Some("1600"));
    assert_eq!(components[0].get(tag::SIDE), Some("2"));
    assert_eq!(components[1].get(tag::SECURITY_ALT_ID), Some("000002"));
    assert_eq!(components[1].get(tag::LAST_QTY), Some("1100"));
    let cash = iec
        .iter()
        .find(|m| m.get(tag::TRADE_REPORT_TYPE) == Some("3"))
        .expect("实物申购应有资金替代记录");
    assert_eq!(cash.get(tag::TOTAL_VALUE_TRADED), Some("24098.800"));
    let groups: Vec<&str> = cash
        .fields
        .iter()
        .filter(|(t, _)| *t == tag::MARGIN_AMT_TYPE)
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(groups, vec!["1", "2"]);
    assert_eq!(cash.get(tag::NUM_TRADES), Some("4"));

    engine.stop_gateway("g1").await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;
    ddl(&mut raw, &format!("DROP TABLE [{}]", req_table)).await;
    ddl(&mut raw, &format!("DROP TABLE [{}]", exec_table)).await;
    let _ = std::fs::remove_dir_all(&pcf_dir);
    let _ = std::fs::remove_dir_all(&data_dir);
}
