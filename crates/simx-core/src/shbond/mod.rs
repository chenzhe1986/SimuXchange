//! 上海新债券网关（shbond）：上交所新债券平台 Binary 1.90 协议的全套实现。
//!
//! 报文结构与 shjj（竞价）完全一致，仅业务参数不同（PlatformID=2、SetID=801、
//! BizID 区分债券现券竞价/质押式回购、协议版本 1.90、OrdType 仅限价）。
//!
//! 目录内各模块职责：
//! - protocol：报文编解码（与 shjj 同构，仅默认参数不同）
//! - session：TCP 会话生命周期（复用 sz::session 的 SessionCtx 与捕获逻辑）
//! - strategy：模拟回报策略（含 206/207 分区序号同步的回报缓存）

pub mod protocol;
pub mod session;
pub mod strategy;
