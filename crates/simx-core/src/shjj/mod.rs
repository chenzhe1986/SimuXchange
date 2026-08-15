//! 上海竞价网关（shjj）：上交所竞价平台 Binary 0.54 协议的全套实现。
//!
//! 目录内各模块职责：
//! - protocol：报文编解码（16 字节报文头：MsgType + MsgSeqNum + MsgBodyLen）
//! - session：TCP 会话生命周期（复用 sz::session 的 SessionCtx 与捕获逻辑）
//! - strategy：模拟回报策略（含 206/207 分区序号同步的回报缓存）

pub mod protocol;
pub mod session;
pub mod strategy;
