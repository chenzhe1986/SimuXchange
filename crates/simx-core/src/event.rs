//! 引擎事件：引擎内部发生的事情（如收到委托、发出回报）实时推送给前端展示。
//!
//! 推送链路：会话代码调用 EngineEvent::log() 生成事件
//! → 投入 tokio broadcast 广播通道（多个订阅者各收一份）
//! → 本地模式：Tauri 把它 emit 给前端页面；远程模式：通过 WebSocket 推给浏览器

use serde::Serialize;

/// 引擎事件。序列化成 JSON 时带 `event` 字段区分类型（serde 的 tag 机制），
/// 目前只有 Log 一种，日后可扩展更多事件类型（如连接变化通知）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum EngineEvent {
    /// 一条日志，对应前端底部日志面板的一行
    #[serde(rename_all = "camelCase")]
    Log {
        /// 级别：info / warn / error
        level: String,
        /// 日志正文
        message: String,
        /// 时间戳（时:分:秒.毫秒）
        ts: String,
        /// 所属网关 id（空串表示全局事件）
        gateway_id: String,
        /// 所属平台 id（空串表示网关级事件）
        platform_id: String,
    },
}

impl EngineEvent {
    /// 快捷构造一条日志事件，自动填当前时间
    pub fn log(level: &str, gateway_id: &str, platform_id: &str, message: String) -> Self {
        EngineEvent::Log {
            level: level.into(),
            message,
            ts: chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
            gateway_id: gateway_id.into(),
            platform_id: platform_id.into(),
        }
    }
}

/// 柜台连接信息（展示在前端平台卡片的“连接列表”里）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnInfo {
    /// 连接序号（进程内递增，仅用于区分不同连接）
    pub id: u64,
    /// 对端地址（柜台机器的 IP:端口）
    pub peer: String,
    /// 登录方 SenderCompID（柜台在 Logon 消息里报的身份）
    pub comp_id: String,
    /// 是否已完成 Logon 登录握手
    pub logged_on: bool,
    /// 连接建立时间
    pub since: String,
}
