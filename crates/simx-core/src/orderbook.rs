//! 订单缓存：平台开启“缓存订单”后，把收到的委托与回报状态记下来，
//! 供界面查看订单列表，也让撤单处理能按订单真实状态回复成功/失败。
//!
//! # 状态机（与真实交易所的订单状态一致）
//!
//! ```text
//! 委托到达 ──► 已报(New) ──► 全部成交(Filled)
//!      │           │              ▲
//!      │           ├──► 部分成交(Partial) ──► 全部成交
//!      │           └──► 已撤(Cancelled)        （撤单成功）
//!      └──► 已拒(Rejected)（拒单策略）
//! ```
//!
//! “在途”= 已报 或 部分成交（还能撤）；其余状态（全成/已拒/已撤）
//! 为终态，撤单一律失败。状态更新时机：回报真正发出去的那一刻
//! （延迟回报没发出前，订单仍按旧状态可撤）。

use serde::Serialize;
use std::sync::{Arc, Mutex as StdMutex};

/// 订单状态（前端按此显示中文标签）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderStatus {
    /// 已报（收到委托，尚未成交）
    New,
    /// 部分成交（还有剩余可继续成交/可撤）
    Partial,
    /// 全部成交（终态）
    Filled,
    /// 已撤单（终态，撤单成功）
    Cancelled,
    /// 已拒绝（终态，拒单）
    Rejected,
}

impl OrderStatus {
    /// 是否在途：在途订单收到撤单请求时应撤单成功；
    /// 终态订单（全成/已拒/已撤）应撤单失败
    pub fn is_inflight(self) -> bool {
        matches!(self, OrderStatus::New | OrderStatus::Partial)
    }

    /// 是否终态（全成/已拒/已撤）：终态后状态不可再变回在途
    pub fn is_terminal(self) -> bool {
        !self.is_inflight()
    }
}

/// 一条缓存的订单（界面订单弹窗的一行）。
/// 数量/价格统一换算成自然单位（股/元），前端直接展示，无需关心协议放大倍数。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderEntry {
    /// 委托编号（订单唯一键；撤单请求的 OrigClOrdID 指向它）
    pub cl_ord_id: String,
    /// 交易所分配的订单号（收到确认回报时回填，此前为空）
    pub order_id: String,
    /// 证券代码
    pub security_id: String,
    /// 方向中文（"买"/"卖"）
    pub side: String,
    /// 委托价（元）
    pub price: f64,
    /// 委托量（股）
    pub qty: f64,
    /// 累计成交量（股）
    pub cum_qty: f64,
    /// 剩余量（股）
    pub leaves_qty: f64,
    /// 订单状态
    pub status: OrderStatus,
    /// 委托类型（协议原值，撤单成功回报回填用）
    pub ord_type: u8,
    /// 证券账户（撤单成功回报回填用；三套协议委托里都有）
    pub account: String,
    /// 营业部代码（撤单成功回报回填用；深市委托里有）
    pub branch: String,
    /// 收到委托的时间（HH:MM:SS）
    pub ts: String,
    /// 所属连接序号（手动回复成交/拒单/撤单时定位回报发给哪个连接）
    pub conn_id: u64,
    /// 回报交易单元：沪市为 Pbu（登录 CompID 前 8 位）、深市为申报交易单元。
    /// 手动回复回报必须回填，柜台按它定位分区（沪市 Pbu 为空会导致回报异常）
    pub pbu: String,
    /// 业务标识（沪市 BizID 回填用；深市无此概念，恒为 0）
    pub biz_id: u32,
    /// 业务 PBU（沪市 BizPbu 回填用；深市无此概念，恒为空）
    pub biz_pbu: String,
    /// 订单所有者类型（回报回填用；深市为 u16，沪市为 u8）
    pub owner_type: u16,
    /// 信用标签（沪市回填用；深市无，恒为空）
    pub credit_tag: String,
    /// 结算会员代码（回报回填用）
    pub clearing_firm: String,
    /// 用户私有信息（回报按规范回填上行值）
    pub user_info: String,
}

/// 一条回报发出后要同步到订单缓存的状态更新。
/// 由策略模块在生成回报计划时携带（订单状态跟着真实发送节奏走）。
#[derive(Debug, Clone)]
pub struct OrderUpdate {
    /// 交易所订单号；空串表示本次回报不涉及订单号（如业务拒绝）
    pub order_id: String,
    /// 累计成交量（股）
    pub cum_qty: f64,
    /// 剩余量（股）
    pub leaves_qty: f64,
    /// 发送该回报后的订单状态
    pub status: OrderStatus,
}

/// 订单缓存上限：超出后丢弃最旧的（防止长时间运行内存无限增长）
const MAX_ORDERS: usize = 2000;

/// 平台级订单缓存（一个平台一份，各连接共享）。
/// 内部是 Arc + 互斥锁：与连接表/统计器同样的并发模型，
/// 读（查询/撤单判断）写（添加/状态更新）都先取锁。
#[derive(Clone)]
pub struct OrderBook {
    inner: Arc<StdMutex<Vec<OrderEntry>>>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    /// 收到委托时登记一条新订单（状态=已报）
    pub fn add(&self, entry: OrderEntry) {
        let mut orders = self.inner.lock().unwrap();
        orders.push(entry);
        // 超出上限：从头部删掉最旧的（头部是最早的委托）
        if orders.len() > MAX_ORDERS {
            let excess = orders.len() - MAX_ORDERS;
            orders.drain(0..excess);
        }
    }

    /// 按委托编号查订单（找不到返回 None；返回的是副本，不影响内部数据）
    pub fn find(&self, cl_ord_id: &str) -> Option<OrderEntry> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|o| o.cl_ord_id == cl_ord_id)
            .cloned()
    }

    /// 一条回报发出后同步状态：回填订单号、累计/剩余量、状态。
    /// 终态保护：订单已到终态（全成/已撤/已拒）后，迟到的在途回报
    /// （如同步补发的确认）不再把状态改回在途。
    pub fn apply(&self, cl_ord_id: &str, upd: &OrderUpdate) {
        let mut orders = self.inner.lock().unwrap();
        if let Some(o) = orders.iter_mut().find(|o| o.cl_ord_id == cl_ord_id) {
            if o.status.is_terminal() && upd.status.is_inflight() {
                return;
            }
            if !upd.order_id.is_empty() {
                o.order_id = upd.order_id.clone();
            }
            o.cum_qty = upd.cum_qty;
            o.leaves_qty = upd.leaves_qty;
            o.status = upd.status;
        }
    }

    /// 尝试撤单：订单存在且为在途（已报/部分成交）→ 置为已撤并清空剩余，返回 true；
    /// 订单不存在或已是终态（全成/已拒/已撤）→ 不改动，返回 false
    pub fn cancel_inflight(&self, cl_ord_id: &str) -> bool {
        let mut orders = self.inner.lock().unwrap();
        match orders.iter_mut().find(|o| o.cl_ord_id == cl_ord_id) {
            Some(o) if o.status.is_inflight() => {
                o.status = OrderStatus::Cancelled;
                o.leaves_qty = 0.0;
                true
            }
            _ => false,
        }
    }

    /// 全部订单（最新的在前，界面直接展示）
    pub fn all(&self) -> Vec<OrderEntry> {
        let mut orders = self.inner.lock().unwrap().clone();
        orders.reverse();
        orders
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(cl: &str, status: OrderStatus) -> OrderEntry {
        OrderEntry {
            cl_ord_id: cl.into(),
            order_id: String::new(),
            security_id: "000001".into(),
            side: "买".into(),
            price: 10.0,
            qty: 1000.0,
            cum_qty: 0.0,
            leaves_qty: 1000.0,
            status,
            ord_type: b'2',
            account: "B880000001".into(),
            branch: "0001".into(),
            ts: "09:30:00".into(),
            conn_id: 1,
            pbu: String::new(),
            biz_id: 0,
            biz_pbu: String::new(),
            owner_type: 0,
            credit_tag: String::new(),
            clearing_firm: String::new(),
            user_info: String::new(),
        }
    }

    #[test]
    fn inflight_status_rules() {
        assert!(OrderStatus::New.is_inflight());
        assert!(OrderStatus::Partial.is_inflight());
        assert!(!OrderStatus::Filled.is_inflight());
        assert!(!OrderStatus::Cancelled.is_inflight());
        assert!(!OrderStatus::Rejected.is_inflight());
    }

    #[test]
    fn cancel_inflight_succeeds_and_cancelled_fails() {
        let book = OrderBook::new();
        book.add(entry("CL1", OrderStatus::New));
        book.add(entry("CL2", OrderStatus::Filled));
        book.add(entry("CL3", OrderStatus::Cancelled));
        book.add(entry("CL4", OrderStatus::Rejected));
        book.add(entry("CL5", OrderStatus::Partial));

        // 在途订单可撤
        assert!(book.cancel_inflight("CL1"));
        assert!(book.cancel_inflight("CL5"));
        let o = book.find("CL1").unwrap();
        assert_eq!(o.status, OrderStatus::Cancelled);
        assert_eq!(o.leaves_qty, 0.0);

        // 终态订单撤单失败（状态不变）
        assert!(!book.cancel_inflight("CL2"));
        assert!(!book.cancel_inflight("CL3"));
        assert!(!book.cancel_inflight("CL4"));
        assert_eq!(book.find("CL2").unwrap().status, OrderStatus::Filled);

        // 不存在的订单也失败
        assert!(!book.cancel_inflight("NO_SUCH"));
    }

    #[test]
    fn apply_updates_fields_and_newest_first() {
        let book = OrderBook::new();
        book.add(entry("CL1", OrderStatus::New));
        book.apply(
            "CL1",
            &OrderUpdate {
                order_id: "0000000000001234".into(),
                cum_qty: 300.0,
                leaves_qty: 700.0,
                status: OrderStatus::Partial,
            },
        );
        let o = book.find("CL1").unwrap();
        assert_eq!(o.order_id, "0000000000001234");
        assert_eq!(o.cum_qty, 300.0);
        assert_eq!(o.status, OrderStatus::Partial);

        // all() 最新的在前
        book.add(entry("CL2", OrderStatus::New));
        let all = book.all();
        assert_eq!(all[0].cl_ord_id, "CL2");
        assert_eq!(all[1].cl_ord_id, "CL1");
    }

    #[test]
    fn apply_never_reverts_terminal_to_inflight() {
        let book = OrderBook::new();
        book.add(entry("CL1", OrderStatus::New));
        // 撤单成功 → 已撤（终态）
        assert!(book.cancel_inflight("CL1"));
        // 迟到的确认回报（在途更新）不应把状态改回已报
        book.apply(
            "CL1",
            &OrderUpdate {
                order_id: "0000000000005678".into(),
                cum_qty: 0.0,
                leaves_qty: 1000.0,
                status: OrderStatus::New,
            },
        );
        let o = book.find("CL1").unwrap();
        assert_eq!(o.status, OrderStatus::Cancelled);
        assert_eq!(o.leaves_qty, 0.0);
    }

    #[test]
    fn overflow_drops_oldest() {
        let book = OrderBook::new();
        for i in 0..(MAX_ORDERS + 50) {
            book.add(entry(&format!("CL{}", i), OrderStatus::New));
        }
        let all = book.all();
        assert_eq!(all.len(), MAX_ORDERS);
        // 最旧的 50 条被丢弃，最新的还在
        assert!(book.find("CL0").is_none());
        assert!(book.find(&format!("CL{}", MAX_ORDERS + 49)).is_some());
    }
}
