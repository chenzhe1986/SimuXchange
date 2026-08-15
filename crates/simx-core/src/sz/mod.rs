//! 深圳统一网关（sz）：深交所 Binary 协议的全套实现。
//!
//! 目录内各模块职责：
//! - protocol：报文编解码（结构体 ↔ 字节串，8 字节报文头）
//! - session：TCP 会话生命周期（Logon/心跳/委托处理，含共享的 SessionCtx）
//! - strategy：模拟回报策略（收到委托后生成回报计划）

pub mod protocol;
pub mod session;
pub mod strategy;
