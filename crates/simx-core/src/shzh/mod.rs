//! 上海综合网关（上交所综合业务平台 EzSTEP 数据库接口）。
//!
//! 与 sz/shjj/shbond 三个 TCP 网关不同，本网关不监听端口：柜台把委托写进
//! SQL Server 的 reqresp（请求及响应接口表），网关轮询读取后按模拟回报策略
//! 回写 resptext（确认/撤单响应）或向 execreport（执行报告表）插入成交记录。
//!
//! 子模块：
//! - protocol：STEP 消息编解码（9=/35= 消息头 + tag=value + SOH 分隔）
//! - strategy：按平台配置的回报策略生成确认/成交/拒单/撤单 STEP 消息
//! - db：SQL Server 异步访问（tiberius 纯 Rust TDS 驱动）
//! - session：平台级轮询任务（读委托表 → 执行策略 → 写回接口表）
pub mod db;
pub mod protocol;
pub mod session;
pub mod strategy;
