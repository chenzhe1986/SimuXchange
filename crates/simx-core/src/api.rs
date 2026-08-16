//! 统一控制接口：本地（Tauri command）与远程（WebSocket）共用同一套 JSON 命令协议
//!
//! # 设计动机
//!
//! 前端有两种方式连接后端：桌面版走 Tauri 进程内调用，远程模式走
//! WebSocket。若两通道各自实现一套处理逻辑，每加一个功能都要改两处、
//! 容易遗漏。因此把“命令 → 调用引擎对应方法”的统一分发抽到 dispatch，
//! 两条通道只负责把收到的 JSON 交给它，处理逻辑只写一遍。
//!
//! 协议格式：
//! - 请求：{ "cmd": "get_snapshot" } / { "cmd": "save_gateway", "gateway": {...} } ...
//! - 响应：{ "ok": true, "data": ... } 或 { "ok": false, "error": "..." }

use crate::config::{GatewayConfig, StrategyConfig};
use crate::engine::Engine;
use crate::sz::session::ManualReportKind;
use serde::Deserialize;
use serde_json::{json, Value};

/// 前端可发送的全部命令。
///
/// serde 注解解释：
/// - tag = "cmd"：JSON 里用 "cmd" 字段的值区分是哪个命令
/// - rename_all = "snake_case"：枚举名 GetSnapshot 对应 JSON 里的 "get_snapshot"
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// 全量快照
    GetSnapshot,
    /// 新建/更新网关
    SaveGateway { gateway: GatewayConfig },
    /// 删除网关
    DeleteGateway { id: String },
    /// 启动网关
    StartGateway { id: String },
    /// 停止网关
    StopGateway { id: String },
    /// 热更新某平台的模拟回报策略与回报延迟（网关运行中实时生效，
    /// 同时写入持久化配置，重启后仍然保留）
    #[serde(rename_all = "camelCase")]
    UpdateStrategy {
        gateway_id: String,
        platform_id: String,
        strategy: StrategyConfig,
    },
    /// 重置统计
    ResetStats { id: String },
    /// 拉取某连接的收发报文（报文弹窗轮询）；after_seq 为增量拉取游标
    #[serde(rename_all = "camelCase")]
    GetConnPackets {
        gateway_id: String,
        platform_id: String,
        conn_id: u64,
        #[serde(default)]
        after_seq: u64,
    },
    /// 拉取某平台的订单缓存列表（订单弹窗轮询；最新的在前）
    #[serde(rename_all = "camelCase")]
    GetOrders { gateway_id: String, platform_id: String },
    /// 拉取某平台全部连接的收发报文（平台级报文弹窗轮询；
    /// 跨连接按 seq 合并排序）；after_seq 为增量拉取游标
    #[serde(rename_all = "camelCase")]
    GetPlatformPackets {
        gateway_id: String,
        platform_id: String,
        #[serde(default)]
        after_seq: u64,
    },
    /// 手动回复一笔在途订单（确认/成交/拒单/撤单成功）；
    /// qty/price 为自然单位（股/元），不传时用缓存值兜底；
    /// front_reject=true 时拒单改发业务拒绝消息（Business Reject），
    /// 否则发执行报告（ExecutionReport）
    #[serde(rename_all = "camelCase")]
    SendReport {
        gateway_id: String,
        platform_id: String,
        cl_ord_id: String,
        kind: ManualReportKind,
        #[serde(default)]
        qty: Option<f64>,
        #[serde(default)]
        price: Option<f64>,
        #[serde(default)]
        reason: Option<i32>,
        #[serde(default)]
        front_reject: bool,
    },
}

/// 拼“成功”响应
fn ok(data: Value) -> Value {
    json!({ "ok": true, "data": data })
}

/// 拼“失败”响应（携带错误文本，前端弹提示用）
fn err(msg: impl Into<String>) -> Value {
    json!({ "ok": false, "error": msg.into() })
}

/// 命令分发入口：把 JSON 解析成 Command，转调引擎对应方法，
/// 再把结果包成统一的 ok/err 响应。无论成败都返回 JSON，不抛异常。
///
/// client_ip 为发起命令的前端地址（远程模式 = WebSocket 对端 IP；
/// 本地桌面端 = 127.0.0.1），随每条非轮询命令写入操作日志（oplog），
/// 便于追溯谁在什么时候做了什么操作。
pub async fn dispatch(engine: &Engine, payload: Value, client_ip: &str) -> Value {
    let cmd: Command = match serde_json::from_value(payload) {
        Ok(c) => c,
        Err(e) => return err(format!("无效命令: {}", e)),
    };
    match cmd {
        // 快照/报文/订单列表是前端每秒轮询的只读命令，记日志只会刷屏，跳过
        Command::GetSnapshot => match serde_json::to_value(engine.snapshot().await) {
            Ok(v) => ok(v),
            Err(e) => err(e.to_string()),
        },
        Command::SaveGateway { gateway } => {
            // id 为空 = 新建（后端会自动生成），否则是编辑保存
            let op = if gateway.id.is_empty() { "新建网关" } else { "保存网关" };
            engine.log_op(
                client_ip,
                &format!("{} {}（{} 个平台）", op, gateway.name, gateway.platforms.len()),
            );
            match engine.save_gateway(gateway).await {
                Ok(gw) => ok(serde_json::to_value(gw).unwrap_or(Value::Null)),
                Err(e) => err(e),
            }
        }
        Command::DeleteGateway { id } => {
            let name = engine.gateway_name(&id).await.unwrap_or_else(|| id.clone());
            engine.log_op(client_ip, &format!("删除网关 {}", name));
            match engine.delete_gateway(&id).await {
                Ok(()) => ok(Value::Null),
                Err(e) => err(e),
            }
        }
        Command::StartGateway { id } => {
            let name = engine.gateway_name(&id).await.unwrap_or_else(|| id.clone());
            engine.log_op(client_ip, &format!("启动网关 {}", name));
            match engine.start_gateway(&id).await {
                Ok(()) => ok(Value::Null),
                Err(e) => err(e),
            }
        }
        Command::StopGateway { id } => {
            let name = engine.gateway_name(&id).await.unwrap_or_else(|| id.clone());
            engine.log_op(client_ip, &format!("停止网关 {}", name));
            match engine.stop_gateway(&id).await {
                Ok(()) => ok(Value::Null),
                Err(e) => err(e),
            }
        }
        Command::UpdateStrategy {
            gateway_id,
            platform_id,
            strategy,
        } => {
            let (gw, plat) = engine
                .platform_name(&gateway_id, &platform_id)
                .await
                .unwrap_or_else(|| (gateway_id.clone(), platform_id.clone()));
            engine.log_op(
                client_ip,
                &format!("热更新网关 {} 平台 {} 的模拟回报策略", gw, plat),
            );
            match engine
                .update_strategy(&gateway_id, &platform_id, strategy)
                .await
            {
                Ok(()) => ok(Value::Null),
                Err(e) => err(e),
            }
        }
        Command::ResetStats { id } => {
            let name = engine.gateway_name(&id).await.unwrap_or_else(|| id.clone());
            engine.log_op(client_ip, &format!("重置网关 {} 统计", name));
            match engine.reset_stats(&id).await {
                Ok(()) => ok(Value::Null),
                Err(e) => err(e),
            }
        }
        Command::GetConnPackets {
            gateway_id,
            platform_id,
            conn_id,
            after_seq,
        } => match engine
            .conn_packets(&gateway_id, &platform_id, conn_id, after_seq)
            .await
        {
            Ok(page) => match serde_json::to_value(page) {
                Ok(v) => ok(v),
                Err(e) => err(e.to_string()),
            },
            Err(e) => err(e),
        },
        Command::GetOrders {
            gateway_id,
            platform_id,
        } => match engine.orders(&gateway_id, &platform_id).await {
            Ok(list) => match serde_json::to_value(list) {
                Ok(v) => ok(v),
                Err(e) => err(e.to_string()),
            },
            Err(e) => err(e),
        },
        Command::GetPlatformPackets {
            gateway_id,
            platform_id,
            after_seq,
        } => match engine
            .platform_packets(&gateway_id, &platform_id, after_seq)
            .await
        {
            Ok(page) => match serde_json::to_value(page) {
                Ok(v) => ok(v),
                Err(e) => err(e.to_string()),
            },
            Err(e) => err(e),
        },
        Command::SendReport {
            gateway_id,
            platform_id,
            cl_ord_id,
            kind,
            qty,
            price,
            reason,
            front_reject,
        } => {
            let (gw, plat) = engine
                .platform_name(&gateway_id, &platform_id)
                .await
                .unwrap_or_else(|| (gateway_id.clone(), platform_id.clone()));
            let kind_label = match kind {
                ManualReportKind::Ack => "确认",
                ManualReportKind::Trade => "成交",
                ManualReportKind::Reject => "拒单",
                ManualReportKind::Cancel => "撤单成功",
            };
            engine.log_op(
                client_ip,
                &format!(
                    "手动回复订单 {}（{}）：网关 {} 平台 {}",
                    cl_ord_id, kind_label, gw, plat
                ),
            );
            match engine
                .send_report(
                    &gateway_id,
                    &platform_id,
                    &cl_ord_id,
                    kind,
                    qty,
                    price,
                    reason,
                    front_reject,
                )
                .await
            {
                Ok(desc) => ok(json!({ "desc": desc })),
                Err(e) => err(e),
            }
        }
    }
}
