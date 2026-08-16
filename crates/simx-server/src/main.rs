//! simx-server：独立部署的模拟撮合后端（Linux/Windows 均可运行）
//!
//! # 程序定位
//!
//! 桌面版（src-tauri）把引擎嵌在界面里，窗口关闭引擎即停。
//! 若需引擎长期驻留服务器（如机房 Linux 主机），用本程序：它把
//! 同一个 simx-core 引擎包成 WebSocket 服务，桌面端通过网络遥控
//! （在 simx.config.json 配置远程地址即可切换）。
//!
//! 前端通过 WebSocket 连接 /ws，消息格式：
//! - 请求：{ "id": 1, "payload": { "cmd": "get_snapshot", ... } }
//! - 响应：{ "id": 1, "resp": { "ok": true, "data": ... } }
//! - 事件推送（无 id）：{ "event": "log", ... }
//!
//! id 的作用：WebSocket 是双向自由收发的，响应不一定紧跟请求，
//! 前端靠 id 把响应对回到发起的那次请求上。
//!
//! 用法：simx-server [--listen 0.0.0.0:9800] [--auto-start] [--update-url <地址>]

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use simx_core::engine::Engine;
use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    // 默认监听所有网卡的 9800 端口
    let mut listen = "0.0.0.0:9800".to_string();
    // 数据目录固定为程序（exe）所在目录：网关配置与启停状态
    // （gateways.json，was_running 字段）、报文文件 packets/ 都在这里，
    // 与桌面端“配置放 exe 旁边”的约定一致，整个目录拷走即迁移
    let data_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    // --auto-start：进程启动后自动恢复上次运行中的网关（无需界面手动启动）
    let mut auto_start = false;
    // --update-url：更新服务器根地址（启动时检查一次新版本，仅打印提示不自升级）
    let mut update_url: Option<String> = None;

    // 手写的简易命令行解析：需求只有几个参数，不值得引入解析库
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" if i + 1 < args.len() => {
                listen = args[i + 1].clone();
                i += 2;
            }
            "--auto-start" => {
                auto_start = true;
                i += 1;
            }
            "--update-url" if i + 1 < args.len() => {
                update_url = Some(args[i + 1].clone());
                i += 2;
            }
            "-h" | "--help" => {
                println!("用法: simx-server [--listen 0.0.0.0:9800] [--auto-start] [--update-url <地址>]");
                return;
            }
            _ => i += 1,
        }
    }

    // 数据目录可能还不存在（首次部署容易忘），提前建好，
    // 否则后面保存 gateways.json 会报“找不到路径”
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        panic!("创建数据目录 {} 失败: {}", data_dir.display(), e);
    }

    // 创建引擎（会从 data_dir/gateways.json 加载历史配置）
    let engine = Engine::new(data_dir.clone());
    println!("SimuXchange 后端已启动");
    println!("  控制接口 : ws://{}/ws", listen);
    println!("  数据目录 : {}", data_dir.display());

    // --update-url：启动时向更新服务器检查一次新版本（只提示，不自动替换）
    if let Some(url) = &update_url {
        check_update(url);
    }

    // --auto-start：恢复上次运行中的网关（失败逐个列出，不影响服务启动）
    if auto_start {
        let failures = engine.auto_start_previous().await;
        if failures.is_empty() {
            println!("  已自动恢复上次运行中的网关");
        } else {
            println!("  自动启动网关失败 {} 个：", failures.len());
            for (id, e) in &failures {
                println!("    [{}] {}", id, e);
            }
        }
    }

    // 两个 HTTP 路由：/ws 升级为 WebSocket；/health 供运维探活检查。
    // with_state 把引擎挂进路由状态，每个请求处理函数都能拿到它
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(engine);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| panic!("监听 {} 失败: {}", listen, e));
    // into_make_service_with_connect_info：让每个请求处理器能拿到客户端
    // SocketAddr（操作日志要记前端 IP，没有它拿不到）
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

/// 启动时向更新服务器检查一次新版本（供 --update-url 参数调用）。
///
/// 只打印提示，不自动下载替换：后端进程长时间驻留，替换运行中的二进制
/// 容易出错，交给运维按提示手动更新。
fn check_update(update_url: &str) {
    let current = env!("CARGO_PKG_VERSION");
    let url = format!("{}/version.json", update_url.trim_end_matches('/'));
    match ureq::get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .call()
    {
        Ok(resp) => {
            // into_string 是 ureq 核心 API（不依赖 json feature，默认 features 已关闭）
            let text = resp.into_string().unwrap_or_default();
            let manifest: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            let latest = manifest.get("version").and_then(|v| v.as_str()).unwrap_or("");
            if !latest.is_empty() && version_cmp(latest, current) == std::cmp::Ordering::Greater {
                println!("  发现新版本 {}（当前 {}），请到更新服务器下载部署", latest, current);
            } else if latest.is_empty() {
                println!("  检查更新：服务器上的 version.json 格式无效");
            } else {
                println!("  已是最新版本 {}", current);
            }
        }
        Err(e) => println!("  检查更新失败（服务器不可达）: {}", e),
    }
}

/// 比较两个版本号（"1.0.0" 式数字点分），供 check_update 判断大小
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let pa: Vec<u32> = a.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    let pb: Vec<u32> = b.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    for i in 0..pa.len().max(pb.len()) {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y {
            return x.cmp(&y);
        }
    }
    std::cmp::Ordering::Equal
}

/// HTTP 请求升级为 WebSocket 连接（浏览器/客户端发起握手时触发）
async fn ws_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
    State(engine): State<Engine>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, engine, peer))
}

/// 伺候一个 WebSocket 客户端的完整生命周期。
///
/// 内部分三个并发任务：
/// 1. 事件推送任务：订阅引擎日志事件，转成 JSON 投递到发送队列
/// 2. 统一发送任务：从队列取消息逐条写出（与 session.rs 的写通道
///    同理：多方都想发消息，经单一队列保证不互相穿插）
/// 3. 主循环：收命令 → 交给 api::dispatch 处理 → 带原 id 回复
///
/// peer 为前端对端地址：连接建立/断开和每条操作日志都记它的 IP，
/// 便于追溯是谁（哪台机器）在操作模拟网关。
async fn handle_ws(socket: WebSocket, engine: Engine, peer: SocketAddr) {
    let ip = peer.ip().to_string();
    engine.log_op(&ip, &format!("前端连接建立（{}）", peer));
    let (mut tx, mut rx) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(1024);

    // 事件推送任务：引擎日志 → JSON → 发送队列
    let mut events = engine.subscribe();
    let ev_out = out_tx.clone();
    let ev_task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => {
                    if let Ok(s) = serde_json::to_string(&ev) {
                        if ev_out.send(s).await.is_err() {
                            break;
                        }
                    }
                }
                // Lagged：日志产生得太快、消费跟不上时会丢弃旧消息，
                // 跳过继续即可（丢几条日志不影响功能）
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    // 统一发送任务：命令响应和事件推送都经这里写出，避免互相穿插
    let send_task = tokio::spawn(async move {
        while let Some(s) = out_rx.recv().await {
            if tx.send(Message::Text(s)).await.is_err() {
                break;
            }
        }
    });

    // 主循环：接收前端命令并回复（连接断开时 rx.next() 返回 None 退出）
    while let Some(Ok(msg)) = rx.next().await {
        if let Message::Text(text) = msg {
            let v: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue, // 不是合法 JSON，忽略
            };
            // 取出请求 id（原样回填）和实际命令体 payload
            let id = v.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let payload = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);
            let resp = simx_core::api::dispatch(&engine, payload, &ip).await;
            let reply = serde_json::json!({ "id": id, "resp": resp });
            if out_tx.send(reply.to_string()).await.is_err() {
                break;
            }
        }
    }

    // 连接已断：停掉两个后台任务，避免泄漏；操作日志记一条断开
    ev_task.abort();
    send_task.abort();
    engine.log_op(&ip, &format!("前端连接断开（{}）", peer));
}
