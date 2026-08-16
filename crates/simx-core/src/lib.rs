//! simx-core：沪深交易所 Binary 模拟撮合引擎核心库
//!
//! # 模块地图（建议按此顺序阅读）
//!
//! 三类网关按目录分放：`sz/`（深圳统一网关）、`shjj/`（上海竞价网关）、
//! `shbond/`（上海新债券网关），每目录内含三个模块：
//! - protocol：Binary 协议编解码（报文长什么样、怎么拆装）
//! - session：TCP 会话管理（Logon/心跳/委托处理，连接生命周期）
//! - strategy：模拟回报策略（收到委托后“导演”什么回报剧本）
//!
//! 网关间共享的基础设施（与具体协议无关）：
//! - config：网关/平台/策略配置模型（界面上能配的都在这，含网关分类）
//! - stats：统计计数（委托/成交数、订单号发生器，三类网关共用）
//! - capture：连接收发报文捕获（16 进制记录 + 可选持久化到文件）
//! - orderbook：订单缓存（平台级，界面查看订单列表 + 撤单按状态判断）
//! - oplog：前端操作日志（带前端 IP 的操作留痕，按日期存 log/ 目录）
//! - engine：网关生命周期管理（总管家：启停/配置/快照，按分类分派会话）
//! - event：引擎事件（日志推送到前端的载体）
//! - api：统一 JSON 控制接口（Tauri 与远程 WebSocket 共用）
//!
//! 数据流向：前端命令 → api → engine → (启动监听) → session 接待柜台
//! → 收委托后由 strategy 生成回报 → 用 protocol 编码发回柜台；
//! 全程的日志通过 event 广播给前端，计数落在 stats。
//!
//! 这个库不含界面：桌面版由 src-tauri 嵌入它，远程服务版由
//! simx-server 托管它，两者共用同一套核心逻辑。

pub mod api;
pub mod capture;
pub mod config;
pub mod engine;
pub mod event;
pub mod oplog;
pub mod orderbook;
pub mod shbond;
pub mod shjj;
pub mod stats;
pub mod sz;
