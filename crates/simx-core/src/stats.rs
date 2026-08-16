//! 统计计数
//!
//! # 为什么用“原子变量”（AtomicU64/AtomicI64）？
//!
//! 多个柜台连接可能同时给同一个平台的计数器加 1。普通整数在并发
//! 累加时会丢数（两个任务同时读到 5，各自加 1 后都写回 6，实际应该
//! 是 7）。原子变量由 CPU 硬件保证“读-改-写”一气呵成，不需要加锁
//! 也不会算错，且比锁快得多。Ordering::Relaxed 表示只需要“计数正确”，
//! 不要求多个计数器之间的更新顺序，这对统计场景足够了。

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// 平台级统计（原子计数，供会话线程并发更新）。
/// 前 7 个是给人看的统计数，后 3 个是协议需要的内部序号发生器。
#[derive(Debug, Default)]
pub struct PlatformStats {
    /// 委托数
    pub orders: AtomicU64,
    /// 确认数（ExecType=0）
    pub acks: AtomicU64,
    /// 成交回报数（ExecType=F）
    pub trades: AtomicU64,
    /// 错单数（拒单，ExecType=8）
    pub order_rejects: AtomicU64,
    /// 业务拒绝数（MsgType=4）
    pub business_rejects: AtomicU64,
    /// 撤单请求数
    pub cancels: AtomicU64,
    /// 历史累计连接数
    pub total_connections: AtomicU64,
    /// 各分区回报记录号（key=分区号：深市 PartitionNo / 沪市 SetID；
    /// value=该分区当前最大回报记录号）。
    /// 回报记录号要求“分区内从 1 连续编号”（深交所规范 3.15 节 /
    /// 上交所按分区回报定位），回报同步重发也按分区校验，
    /// 因此从单一的全局计数器改为按分区各自累计
    pub partition_report_index: Mutex<HashMap<i32, i64>>,
    /// 订单编号序列（交易所分配的 OrderID）
    pub order_seq: AtomicI64,
    /// 执行编号序列（每条回报的 ExecID）
    pub exec_seq: AtomicI64,
}

impl PlatformStats {
    /// 取某分区下一个回报记录号（从 1 开始，分区内连续递增）。
    /// 由 writer 任务在真实发送回报时调用（分区号从报文里解析）。
    pub fn next_report_index_for(&self, partition: i32) -> i64 {
        let mut m = self.partition_report_index.lock();
        let n = m.entry(partition).or_insert(0);
        *n += 1;
        *n
    }

    /// 某分区当前最大回报记录号（该分区尚未发过回报时为 0）。
    /// 回报同步响应（沪 207 EndReportIndex / 深 回报结束消息）用它告知柜台
    /// 该分区已发到第几条。
    pub fn partition_end_index(&self, partition: i32) -> i64 {
        self.partition_report_index
            .lock()
            .get(&partition)
            .copied()
            .unwrap_or(0)
    }

    /// 取下一个订单编号：协议要求固定 16 位，不足前面补 0
    pub fn next_order_id(&self) -> String {
        let n = self.order_seq.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{:016}", n)
    }

    /// 取下一个执行编号：同样固定 16 位补 0
    pub fn next_exec_id(&self) -> String {
        let n = self.exec_seq.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{:016}", n)
    }

    /// 把当前计数抽成一份普通数字的快照，供序列化发给前端展示
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            orders: self.orders.load(Ordering::Relaxed),
            acks: self.acks.load(Ordering::Relaxed),
            trades: self.trades.load(Ordering::Relaxed),
            order_rejects: self.order_rejects.load(Ordering::Relaxed),
            business_rejects: self.business_rejects.load(Ordering::Relaxed),
            cancels: self.cancels.load(Ordering::Relaxed),
            total_connections: self.total_connections.load(Ordering::Relaxed),
        }
    }

    /// 统计归零（前端“重置统计”按钮）。
    /// 注意不重置三个序号发生器：订单号/执行号重复会让柜台困惑
    pub fn reset(&self) {
        self.orders.store(0, Ordering::Relaxed);
        self.acks.store(0, Ordering::Relaxed);
        self.trades.store(0, Ordering::Relaxed);
        self.order_rejects.store(0, Ordering::Relaxed);
        self.business_rejects.store(0, Ordering::Relaxed);
        self.cancels.store(0, Ordering::Relaxed);
        self.total_connections.store(0, Ordering::Relaxed);
    }
}

/// 统计快照（序列化给前端）。camelCase 重命名是为了匹配
/// 前端 JavaScript 的命名习惯（order_rejects → orderRejects）
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsSnapshot {
    pub orders: u64,
    pub acks: u64,
    pub trades: u64,
    pub order_rejects: u64,
    pub business_rejects: u64,
    pub cancels: u64,
    pub total_connections: u64,
}
