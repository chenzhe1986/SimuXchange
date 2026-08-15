//! 深交所 Binary 交易接口协议编解码。
//!
//! # 协议背景
//!
//! 柜台（OMS）与交易网关（TGW）之间通过 TCP 长连接传输二进制报文，
//! 报文字段布局由《深圳证券交易所 Binary 交易数据接口规范》约定。
//! 本文件负责结构体 ↔ 字节串的编码/解码。
//!
//! # 报文结构
//!
//! ```text
//! +----------------+------------------+----------------+----------------+
//! | MsgType (4字节) | BodyLength (4字节) | 消息体 (N字节)   | Checksum (4字节) |
//! |  消息类型        |  消息体长度         |  具体业务字段     |  校验和          |
//! +----------------+------------------+----------------+----------------+
//! ```
//!
//! 关键约定（来自规范）：
//! - 所有整数采用大端序（BIG-ENDIAN，高位字节在前，网络传输惯例）
//! - 校验和：从消息头开始到消息体结束所有字节求和 mod 256，用于检测传输错误
//! - 字符串定长：不足后补空格，超长截断，UTF-8 编码
//! - Price = Int64 N13(4)：价格放大 1 万倍存储，如 186400 表示 18.64 元
//! - Qty   = Int64 N15(2)：数量放大 100 倍存储，如 10000 表示 100 股
//! - LocalTimeStamp = Int64，格式 YYYYMMDDHHMMSSsss（年月日时分秒毫秒）
//!
//! 价格/数量以放大后的整数传输：浮点数存在精度误差，金融系统要求
//! 金额精确，故一律用整数表示。

use std::io;

use crate::capture::ParsedField;

/// 消息类型常量（报文头的 MsgType 字段，括号内为规范章节号）。
///
/// 分两类：
/// - 会话层（1~9）：登录、注销、心跳等“连接维护”消息，与具体业务无关
/// - 业务层（六位数）：委托、回报、撤单等真正的交易消息
pub mod msg_type {
    /// 登录（4.4.1）
    pub const LOGON: u32 = 1;
    /// 注销（4.4.2）
    pub const LOGOUT: u32 = 2;
    /// 心跳（4.4.3），BodyLength = 0
    pub const HEARTBEAT: u32 = 3;
    /// 业务拒绝（5.1）
    pub const BUSINESS_REJECT: u32 = 4;
    /// 回报同步（5.2）
    pub const REPORT_SYNC: u32 = 5;
    /// 平台状态（5.3）
    pub const PLATFORM_STATE: u32 = 6;
    /// 回报结束（5.4）
    pub const REPORT_FINISHED: u32 = 7;
    /// 平台信息（5.5）
    pub const PLATFORM_INFO: u32 = 9;
    /// 现货集中竞价交易业务新订单（4.5.1.1）
    pub const NEW_ORDER_CASH: u32 = 100_101;
    /// 撤单请求（4.5.2）
    pub const ORDER_CANCEL_REQUEST: u32 = 190_007;
    /// 撤单失败响应（4.5.3）
    pub const CANCEL_REJECT: u32 = 290_008;
    /// 现货集中竞价业务订单响应及撤单成功执行报告（4.5.4.1）
    pub const EXEC_RPT_CASH_ACK: u32 = 200_102;
    /// 现货集中竞价业务成交执行报告（4.5.5.1）
    pub const EXEC_RPT_CASH_TRADE: u32 = 200_115;
}

/// 执行类型 ExecType 取值（回报消息里表示“这条回报是什么事件”）。
/// 注意值是 ASCII 字符而非数字：b'0' 实际是字节 0x30。
pub mod exec_type {
    /// 新订单确认（委托已被交易所接受）
    pub const NEW: u8 = b'0';
    /// 撤单成功
    pub const CANCELLED: u8 = b'4';
    /// 订单拒绝
    pub const REJECT: u8 = b'8';
    /// 成交
    pub const TRADE: u8 = b'F';
}

/// 订单状态 OrdStatus 取值（表示订单当前的整体状态）
pub mod ord_status {
    /// 已报（未成交）
    pub const NEW: u8 = b'0';
    /// 部分成交
    pub const PARTIALLY_FILLED: u8 = b'1';
    /// 全部成交
    pub const FILLED: u8 = b'2';
    /// 已撤单
    pub const CANCELLED: u8 = b'4';
    /// 已拒绝
    pub const REJECTED: u8 = b'8';
}

/// 校验和：消息头 + 消息体所有字节求和 mod 256。
/// 规范中的 C 代码对 signed char 做符号扩展后累加，
/// 由于 b as i8 与 b 对 256 同余，结果等价于无符号字节和 mod 256。
pub fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32)) % 256
}

/// 组装完整报文：消息头（类型+长度）+ 消息体 + 校验和。
/// 所有往外发的消息最后都经过这个函数包装成字节串。
pub fn frame(msg_type: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(&checksum(&out).to_be_bytes());
    out
}

/// 消息体写入器：把各种类型的字段按顺序追加到字节缓冲区。
///
/// 用法：按规范中字段表的顺序依次调用 str/u16/i64 等方法，
/// 顺序和长度必须与规范完全一致，否则对端无法解析。
pub struct BodyWriter {
    buf: Vec<u8>,
}

impl BodyWriter {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(256) }
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    /// 定长字符串：截断到 len 字节（保证不把一个中文字符拦腰切断），不足补空格
    pub fn str(&mut self, s: &str, len: usize) {
        let mut bytes = s.as_bytes();
        if bytes.len() > len {
            let mut end = len;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            bytes = &s.as_bytes()[..end];
        }
        self.buf.extend_from_slice(bytes);
        self.buf.resize(self.buf.len() + (len - bytes.len()), b' ');
    }

    /// 单字符字段（规范中的 char[1]，如买卖方向、订单类型）
    pub fn ch(&mut self, c: u8) {
        self.buf.push(c);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
}

impl Default for BodyWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// 消息体读取器：BodyWriter 的逆操作，从字节串中按顺序读出各字段。
/// 内部维护一个读取位置 pos，每读一个字段就往后移动。
pub struct BodyReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BodyReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "消息体长度不足"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// 读取定长字符串并去掉尾部空格
    pub fn str(&mut self, len: usize) -> io::Result<String> {
        let raw = self.take(len)?;
        Ok(String::from_utf8_lossy(raw).trim_end().to_string())
    }

    pub fn ch(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
}

/// 当前本地时间戳，格式 YYYYMMDDHHMMSSsss（如20260731093000123）
pub fn now_timestamp() -> i64 {
    chrono::Local::now()
        .format("%Y%m%d%H%M%S%3f")
        .to_string()
        .parse()
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 会话层消息
// ---------------------------------------------------------------------------

/// 登录消息（MsgType=1），OMS→TGW 与 TGW→OMS 结构相同
#[derive(Debug, Clone, Default)]
pub struct Logon {
    pub sender_comp_id: String,   // char[20]
    pub target_comp_id: String,   // char[20]
    pub heart_bt_int: i32,        // 心跳间隔（秒）
    pub password: String,         // char[16]
    pub default_appl_ver_id: String, // char[32]
}

impl Logon {
    pub const BODY_LEN: usize = 20 + 20 + 4 + 16 + 32;

    pub fn decode(body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        Ok(Self {
            sender_comp_id: r.str(20)?,
            target_comp_id: r.str(20)?,
            heart_bt_int: r.i32()?,
            password: r.str(16)?,
            default_appl_ver_id: r.str(32)?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.str(&self.sender_comp_id, 20);
        w.str(&self.target_comp_id, 20);
        w.i32(self.heart_bt_int);
        w.str(&self.password, 16);
        w.str(&self.default_appl_ver_id, 32);
        frame(msg_type::LOGON, &w.into_inner())
    }
}

/// 注销消息（MsgType=2）
#[derive(Debug, Clone, Default)]
pub struct Logout {
    pub session_status: i32, // 会话状态
    pub text: String,        // char[200]
}

impl Logout {
    pub fn decode(body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        Ok(Self {
            session_status: r.i32()?,
            text: r.str(200)?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.i32(self.session_status);
        w.str(&self.text, 200);
        frame(msg_type::LOGOUT, &w.into_inner())
    }
}

/// 心跳消息（MsgType=3，BodyLength=0）
pub fn encode_heartbeat() -> Vec<u8> {
    frame(msg_type::HEARTBEAT, &[])
}

/// 平台状态消息（MsgType=6）
pub fn encode_platform_state(platform_id: u16, state: u16) -> Vec<u8> {
    let mut w = BodyWriter::new();
    w.u16(platform_id);
    w.u16(state);
    frame(msg_type::PLATFORM_STATE, &w.into_inner())
}

/// 平台信息消息（MsgType=9）
pub fn encode_platform_info(platform_id: u16, partitions: &[i32]) -> Vec<u8> {
    let mut w = BodyWriter::new();
    w.u16(platform_id);
    w.u32(partitions.len() as u32);
    for p in partitions {
        w.i32(*p);
    }
    frame(msg_type::PLATFORM_INFO, &w.into_inner())
}

/// 回报结束消息（MsgType=7）
pub fn encode_report_finished(partition_no: i32, report_index: i64, platform_id: u16) -> Vec<u8> {
    let mut w = BodyWriter::new();
    w.i32(partition_no);
    w.i64(report_index);
    w.u16(platform_id);
    frame(msg_type::REPORT_FINISHED, &w.into_inner())
}

// ---------------------------------------------------------------------------
// 业务消息：现货集中竞价交易（ApplID = 010）
// ---------------------------------------------------------------------------

/// 现货集中竞价交易业务新订单（MsgType=100101）
///
/// 公共字段 + 扩展字段（StopPx/MinQty/MaxPriceLevels/TimeInForce/CashMargin）
#[derive(Debug, Clone, Default)]
pub struct NewOrderCash {
    pub appl_id: String,            // char[3] 应用标识 "010"
    pub submitting_pbu_id: String,  // char[6] 申报交易单元
    pub security_id: String,        // char[8] 证券代码
    pub security_id_source: String, // char[4] 证券代码源 "102"
    pub owner_type: u16,            // 订单所有者类型
    pub clearing_firm: String,      // char[2] 结算机构代码
    pub transact_time: i64,         // 委托时间
    pub user_info: String,          // char[8] 用户私有信息
    pub cl_ord_id: String,          // char[10] 客户订单编号
    pub account_id: String,         // char[12] 证券账户
    pub branch_id: String,          // char[4] 营业部代码
    pub order_restrictions: String, // char[4] 订单限定
    pub side: u8,                   // 买卖方向 1=买 2=卖
    pub ord_type: u8,               // 订单类别 1=市价 2=限价 U=本方最优
    pub order_qty: i64,             // 订单数量 N15(2)
    pub price: i64,                 // 价格 N13(4)
    // ---- 扩展字段（100101）----
    pub stop_px: i64,               // 止损价
    pub min_qty: i64,               // 最低成交数量
    pub max_price_levels: u16,      // 最多成交价位数
    pub time_in_force: u8,          // 订单有效时间类型
    pub cash_margin: u8,            // 信用标识
}

impl NewOrderCash {
    pub fn decode(body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        let mut o = Self {
            appl_id: r.str(3)?,
            submitting_pbu_id: r.str(6)?,
            security_id: r.str(8)?,
            security_id_source: r.str(4)?,
            owner_type: r.u16()?,
            clearing_firm: r.str(2)?,
            transact_time: r.i64()?,
            user_info: r.str(8)?,
            cl_ord_id: r.str(10)?,
            account_id: r.str(12)?,
            branch_id: r.str(4)?,
            order_restrictions: r.str(4)?,
            side: r.ch()?,
            ord_type: r.ch()?,
            order_qty: r.i64()?,
            price: r.i64()?,
            ..Default::default()
        };
        // 扩展字段：容忍对端未发送的情况
        if r.remaining() >= 20 {
            o.stop_px = r.i64()?;
            o.min_qty = r.i64()?;
            o.max_price_levels = r.u16()?;
            o.time_in_force = r.ch()?;
            o.cash_margin = r.ch()?;
        }
        Ok(o)
    }
}

/// 现货集中竞价业务订单响应执行报告（MsgType=200102）
///
/// 用于订单确认（ExecType=0）、撤单成功（ExecType=4）、订单拒绝（ExecType=8）
#[derive(Debug, Clone, Default)]
pub struct ExecRptCashAck {
    pub partition_no: i32,          // 平台分区号
    pub report_index: i64,          // 回报记录号
    pub appl_id: String,            // char[3]
    pub reporting_pbu_id: String,   // char[6] 回报交易单元
    pub submitting_pbu_id: String,  // char[6]
    pub security_id: String,        // char[8]
    pub security_id_source: String, // char[4]
    pub owner_type: u16,
    pub clearing_firm: String,      // char[2]
    pub transact_time: i64,         // 回报时间
    pub user_info: String,          // char[8]
    pub order_id: String,           // char[16] 交易所订单编号
    pub cl_ord_id: String,          // char[10]
    pub orig_cl_ord_id: String,     // char[10]
    pub exec_id: String,            // char[16] 执行编号
    pub exec_type: u8,              // 执行类型
    pub ord_status: u8,             // 订单状态
    pub ord_rej_reason: u16,        // 撤单/拒绝原因代码
    pub leaves_qty: i64,            // 订单剩余数量
    pub cum_qty: i64,               // 累计执行数量
    pub side: u8,
    pub ord_type: u8,
    pub order_qty: i64,
    pub price: i64,
    pub account_id: String,         // char[12]
    pub branch_id: String,          // char[4]
    pub order_restrictions: String, // char[4]
    // ---- 扩展字段（200102）----
    pub stop_px: i64,
    pub min_qty: i64,
    pub max_price_levels: u16,
    pub time_in_force: u8,
    pub cash_margin: u8,
}

impl ExecRptCashAck {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.i32(self.partition_no);
        w.i64(self.report_index);
        w.str(&self.appl_id, 3);
        w.str(&self.reporting_pbu_id, 6);
        w.str(&self.submitting_pbu_id, 6);
        w.str(&self.security_id, 8);
        w.str(&self.security_id_source, 4);
        w.u16(self.owner_type);
        w.str(&self.clearing_firm, 2);
        w.i64(self.transact_time);
        w.str(&self.user_info, 8);
        w.str(&self.order_id, 16);
        w.str(&self.cl_ord_id, 10);
        w.str(&self.orig_cl_ord_id, 10);
        w.str(&self.exec_id, 16);
        w.ch(self.exec_type);
        w.ch(self.ord_status);
        w.u16(self.ord_rej_reason);
        w.i64(self.leaves_qty);
        w.i64(self.cum_qty);
        w.ch(self.side);
        w.ch(self.ord_type);
        w.i64(self.order_qty);
        w.i64(self.price);
        w.str(&self.account_id, 12);
        w.str(&self.branch_id, 4);
        w.str(&self.order_restrictions, 4);
        w.i64(self.stop_px);
        w.i64(self.min_qty);
        w.u16(self.max_price_levels);
        w.ch(self.time_in_force);
        w.ch(self.cash_margin);
        frame(msg_type::EXEC_RPT_CASH_ACK, &w.into_inner())
    }
}

/// 现货集中竞价业务成交执行报告（MsgType=200115，ExecType=F）
#[derive(Debug, Clone, Default)]
pub struct ExecRptCashTrade {
    pub partition_no: i32,
    pub report_index: i64,
    pub appl_id: String,            // char[3]
    pub reporting_pbu_id: String,   // char[6]
    pub submitting_pbu_id: String,  // char[6]
    pub security_id: String,        // char[8]
    pub security_id_source: String, // char[4]
    pub owner_type: u16,
    pub clearing_firm: String,      // char[2]
    pub transact_time: i64,
    pub user_info: String,          // char[8]
    pub order_id: String,           // char[16]
    pub cl_ord_id: String,          // char[10]
    pub exec_id: String,            // char[16]
    pub exec_type: u8,              // F=Trade
    pub ord_status: u8,             // 1=部分成交 2=全部成交
    pub last_px: i64,               // 成交价 N13(4)
    pub last_qty: i64,              // 成交数量 N15(2)
    pub leaves_qty: i64,
    pub cum_qty: i64,
    pub side: u8,
    pub account_id: String,         // char[12]
    pub branch_id: String,          // char[4]
    // ---- 扩展字段（200115）----
    pub cash_margin: u8,
}

impl ExecRptCashTrade {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.i32(self.partition_no);
        w.i64(self.report_index);
        w.str(&self.appl_id, 3);
        w.str(&self.reporting_pbu_id, 6);
        w.str(&self.submitting_pbu_id, 6);
        w.str(&self.security_id, 8);
        w.str(&self.security_id_source, 4);
        w.u16(self.owner_type);
        w.str(&self.clearing_firm, 2);
        w.i64(self.transact_time);
        w.str(&self.user_info, 8);
        w.str(&self.order_id, 16);
        w.str(&self.cl_ord_id, 10);
        w.str(&self.exec_id, 16);
        w.ch(self.exec_type);
        w.ch(self.ord_status);
        w.i64(self.last_px);
        w.i64(self.last_qty);
        w.i64(self.leaves_qty);
        w.i64(self.cum_qty);
        w.ch(self.side);
        w.str(&self.account_id, 12);
        w.str(&self.branch_id, 4);
        w.ch(self.cash_margin);
        frame(msg_type::EXEC_RPT_CASH_TRADE, &w.into_inner())
    }
}

/// 业务拒绝消息（MsgType=4）
#[derive(Debug, Clone, Default)]
pub struct BusinessReject {
    pub appl_id: String,              // char[3]
    pub transact_time: i64,
    pub submitting_pbu_id: String,    // char[6]
    pub security_id: String,          // char[8]
    pub security_id_source: String,   // char[4]
    pub ref_seq_num: i64,             // 被拒绝消息的消息序号
    pub ref_msg_type: u32,            // 被拒绝的消息类型
    pub business_reject_ref_id: String, // char[10]
    pub business_reject_reason: u16,
    pub business_reject_text: String, // char[50]
}

impl BusinessReject {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.str(&self.appl_id, 3);
        w.i64(self.transact_time);
        w.str(&self.submitting_pbu_id, 6);
        w.str(&self.security_id, 8);
        w.str(&self.security_id_source, 4);
        w.i64(self.ref_seq_num);
        w.u32(self.ref_msg_type);
        w.str(&self.business_reject_ref_id, 10);
        w.u16(self.business_reject_reason);
        w.str(&self.business_reject_text, 50);
        frame(msg_type::BUSINESS_REJECT, &w.into_inner())
    }
}

/// 撤单请求（MsgType=190007）
#[derive(Debug, Clone, Default)]
pub struct OrderCancelRequest {
    pub appl_id: String,
    pub submitting_pbu_id: String,
    pub security_id: String,
    pub security_id_source: String,
    pub owner_type: u16,
    pub clearing_firm: String,
    pub transact_time: i64,
    pub user_info: String,
    pub cl_ord_id: String,
    pub orig_cl_ord_id: String,
    pub side: u8,
    pub order_id: String,
    pub order_qty: i64,
}

impl OrderCancelRequest {
    pub fn decode(body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        Ok(Self {
            appl_id: r.str(3)?,
            submitting_pbu_id: r.str(6)?,
            security_id: r.str(8)?,
            security_id_source: r.str(4)?,
            owner_type: r.u16()?,
            clearing_firm: r.str(2)?,
            transact_time: r.i64()?,
            user_info: r.str(8)?,
            cl_ord_id: r.str(10)?,
            orig_cl_ord_id: r.str(10)?,
            side: r.ch()?,
            order_id: r.str(16)?,
            order_qty: r.i64()?,
        })
    }
}

/// 撤单失败响应（MsgType=290008）
#[derive(Debug, Clone, Default)]
pub struct CancelReject {
    pub partition_no: i32,
    pub report_index: i64,
    pub appl_id: String,
    pub reporting_pbu_id: String,
    pub submitting_pbu_id: String,
    pub security_id: String,
    pub security_id_source: String,
    pub owner_type: u16,
    pub clearing_firm: String,
    pub transact_time: i64,
    pub user_info: String,
    pub cl_ord_id: String,
    pub orig_cl_ord_id: String,
    pub side: u8,
    pub ord_status: u8,
    pub cxl_rej_reason: u16,
    pub reject_text: String, // char[16]
    pub order_id: String,
}

impl CancelReject {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.i32(self.partition_no);
        w.i64(self.report_index);
        w.str(&self.appl_id, 3);
        w.str(&self.reporting_pbu_id, 6);
        w.str(&self.submitting_pbu_id, 6);
        w.str(&self.security_id, 8);
        w.str(&self.security_id_source, 4);
        w.u16(self.owner_type);
        w.str(&self.clearing_firm, 2);
        w.i64(self.transact_time);
        w.str(&self.user_info, 8);
        w.str(&self.cl_ord_id, 10);
        w.str(&self.orig_cl_ord_id, 10);
        w.ch(self.side);
        w.ch(self.ord_status);
        w.u16(self.cxl_rej_reason);
        w.str(&self.reject_text, 16);
        w.str(&self.order_id, 16);
        frame(msg_type::CANCEL_REJECT, &w.into_inner())
    }
}

/// 消息类型中文名（报文解析展示与日志用）
pub fn msg_type_name(mt: u32) -> &'static str {
    match mt {
        msg_type::LOGON => "登录 Logon",
        msg_type::LOGOUT => "注销 Logout",
        msg_type::HEARTBEAT => "心跳 Heartbeat",
        msg_type::BUSINESS_REJECT => "业务拒绝 BusinessReject",
        msg_type::REPORT_SYNC => "回报同步 ReportSync",
        msg_type::PLATFORM_STATE => "平台状态 PlatformState",
        msg_type::REPORT_FINISHED => "回报结束 ReportFinished",
        msg_type::PLATFORM_INFO => "平台信息 PlatformInfo",
        msg_type::NEW_ORDER_CASH => "新订单申报 NewOrderCash",
        msg_type::ORDER_CANCEL_REQUEST => "撤单请求 OrderCancelRequest",
        msg_type::CANCEL_REJECT => "撤单失败 CancelReject",
        msg_type::EXEC_RPT_CASH_ACK => "订单执行报告 ExecRptCashAck",
        msg_type::EXEC_RPT_CASH_TRADE => "成交执行报告 ExecRptCashTrade",
        _ => "未知消息",
    }
}

/// 按交易所规范字段名解析一条报文，供捕获展示/持久化使用。
/// 第一条固定为 MsgType（中文名 + 消息号），其余按消息体字段顺序逐项读取；
/// 未知消息类型或字段读取失败时返回空列表（调用方只展示原始报文）。
pub fn describe_fields(mt: u32, body: &[u8]) -> Vec<ParsedField> {
    describe_body(mt, body).unwrap_or_default()
}

/// 拼一个解析字段（名称 + 可读值；数字/字符等类型自动转字符串）
fn f(name: impl Into<String>, value: impl ToString) -> ParsedField {
    ParsedField { name: name.into(), value: value.to_string() }
}

/// 深交所时间戳 YYYYMMDDHHMMSSsss → “2026-07-31 09:30:00.123”
fn fmt_time(v: i64) -> String {
    if v <= 0 {
        return "0".into();
    }
    let s = format!("{:017}", v);
    format!(
        "{}-{}-{} {}:{}:{}.{}",
        &s[0..4], &s[4..6], &s[6..8], &s[8..10], &s[10..12], &s[12..14], &s[14..17]
    )
}

/// 放大整数 → 自然单位字符串（去尾零），如 123400/10000 → “12.34”
fn fmt_scaled(v: i64, unit: i64) -> String {
    let neg = v < 0;
    let av = v.unsigned_abs();
    let int = av / unit as u64;
    let mut frac = format!("{:0width$}", av % unit as u64, width = unit.to_string().len() - 1);
    while frac.ends_with('0') {
        frac.pop();
    }
    let s = if frac.is_empty() {
        int.to_string()
    } else {
        format!("{}.{}", int, frac)
    };
    if neg {
        format!("-{}", s)
    } else {
        s
    }
}

/// 买卖方向：1=买 2=卖（其余原样显示）
fn side_label(b: u8) -> String {
    match b {
        b'1' => "1 (买)".into(),
        b'2' => "2 (卖)".into(),
        b => (b as char).to_string(),
    }
}

/// 订单类别：1=市价 2=限价 U=本方最优（其余原样显示）
fn ord_type_label(b: u8) -> String {
    match b {
        b'1' => "1 (市价)".into(),
        b'2' => "2 (限价)".into(),
        b'U' => "U (本方最优)".into(),
        b => (b as char).to_string(),
    }
}

/// 执行类型：0=新订单确认 4=撤单成功 8=订单拒绝 F=成交
fn exec_type_label(b: u8) -> String {
    match b {
        b'0' => "0 (新订单确认)".into(),
        b'4' => "4 (撤单成功)".into(),
        b'8' => "8 (订单拒绝)".into(),
        b'F' => "F (成交)".into(),
        b => (b as char).to_string(),
    }
}

/// 订单状态：0=已报 1=部分成交 2=全部成交 4=已撤单 8=已拒绝
fn ord_status_label(b: u8) -> String {
    match b {
        b'0' => "0 (已报)".into(),
        b'1' => "1 (部分成交)".into(),
        b'2' => "2 (全部成交)".into(),
        b'4' => "4 (已撤单)".into(),
        b'8' => "8 (已拒绝)".into(),
        b => (b as char).to_string(),
    }
}

/// 应用标识：010=现货集中竞价（其余原样显示）
fn appl_id_label(s: &str) -> String {
    match s {
        "010" => "010 (现货集中竞价)".into(),
        other => other.to_string(),
    }
}

/// 实际逐字段读取逻辑：按消息类型分派，读失败（长度不符）即整体放弃
fn describe_body(mt: u32, body: &[u8]) -> io::Result<Vec<ParsedField>> {
    let mut r = BodyReader::new(body);
    let mut out = vec![f("MsgType", format!("{} ({})", msg_type_name(mt), mt))];
    match mt {
        msg_type::LOGON => {
            out.push(f("SenderCompID", r.str(20)?));
            out.push(f("TargetCompID", r.str(20)?));
            out.push(f("HeartBtInt", format!("{} 秒", r.i32()?)));
            out.push(f("Password", r.str(16)?));
            out.push(f("DefaultApplVerID", r.str(32)?));
        }
        msg_type::LOGOUT => {
            out.push(f("SessionStatus", r.i32()?));
            out.push(f("Text", r.str(200)?));
        }
        msg_type::HEARTBEAT => {}
        msg_type::BUSINESS_REJECT => {
            out.push(f("ApplID", appl_id_label(&r.str(3)?)));
            out.push(f("TransactTime", fmt_time(r.i64()?)));
            out.push(f("SubmittingPBUId", r.str(6)?));
            out.push(f("SecurityID", r.str(8)?));
            out.push(f("SecurityIDSource", r.str(4)?));
            out.push(f("RefSeqNum", r.i64()?));
            out.push(f("RefMsgType", r.u32()?));
            out.push(f("BusinessRejectRefID", r.str(10)?));
            out.push(f("BusinessRejectReason", r.u16()?));
            out.push(f("BusinessRejectText", r.str(50)?));
        }
        // 回报同步：模拟器不解析其消息体，只展示消息名
        msg_type::REPORT_SYNC => {}
        msg_type::PLATFORM_STATE => {
            out.push(f("PlatformID", r.u16()?));
            out.push(f("State", r.u16()?));
        }
        msg_type::REPORT_FINISHED => {
            out.push(f("PartitionNo", r.i32()?));
            out.push(f("ReportIndex", r.i64()?));
            out.push(f("PlatformID", r.u16()?));
        }
        msg_type::PLATFORM_INFO => {
            let platform_id = r.u16()?;
            let n = r.u32()?;
            out.push(f("PlatformID", platform_id));
            out.push(f("NoPartitions", n));
            for i in 0..n {
                out.push(f(format!("Partition[{}]", i + 1), r.i32()?));
            }
        }
        msg_type::NEW_ORDER_CASH => {
            out.push(f("ApplID", appl_id_label(&r.str(3)?)));
            out.push(f("SubmittingPBUId", r.str(6)?));
            out.push(f("SecurityID", r.str(8)?));
            out.push(f("SecurityIDSource", r.str(4)?));
            out.push(f("OwnerType", r.u16()?));
            out.push(f("ClearingFirm", r.str(2)?));
            out.push(f("TransactTime", fmt_time(r.i64()?)));
            out.push(f("UserInfo", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("AccountID", r.str(12)?));
            out.push(f("BranchID", r.str(4)?));
            out.push(f("OrderRestrictions", r.str(4)?));
            out.push(f("Side", side_label(r.ch()?)));
            out.push(f("OrdType", ord_type_label(r.ch()?)));
            out.push(f("OrderQty", format!("{} 股", fmt_scaled(r.i64()?, 100))));
            out.push(f("Price", format!("{} 元", fmt_scaled(r.i64()?, 10000))));
            // 扩展字段：对端可能未发送，剩余不足则跳过
            if r.remaining() >= 20 {
                out.push(f("StopPx", format!("{} 元", fmt_scaled(r.i64()?, 10000))));
                out.push(f("MinQty", format!("{} 股", fmt_scaled(r.i64()?, 100))));
                out.push(f("MaxPriceLevels", r.u16()?));
                out.push(f("TimeInForce", r.ch()?));
                out.push(f("CashMargin", r.ch()?));
            }
        }
        msg_type::ORDER_CANCEL_REQUEST => {
            out.push(f("ApplID", appl_id_label(&r.str(3)?)));
            out.push(f("SubmittingPBUId", r.str(6)?));
            out.push(f("SecurityID", r.str(8)?));
            out.push(f("SecurityIDSource", r.str(4)?));
            out.push(f("OwnerType", r.u16()?));
            out.push(f("ClearingFirm", r.str(2)?));
            out.push(f("TransactTime", fmt_time(r.i64()?)));
            out.push(f("UserInfo", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("OrigClOrdID", r.str(10)?));
            out.push(f("Side", side_label(r.ch()?)));
            out.push(f("OrderID", r.str(16)?));
            out.push(f("OrderQty", format!("{} 股", fmt_scaled(r.i64()?, 100))));
        }
        msg_type::CANCEL_REJECT => {
            out.push(f("PartitionNo", r.i32()?));
            out.push(f("ReportIndex", r.i64()?));
            out.push(f("ApplID", appl_id_label(&r.str(3)?)));
            out.push(f("ReportingPBUId", r.str(6)?));
            out.push(f("SubmittingPBUId", r.str(6)?));
            out.push(f("SecurityID", r.str(8)?));
            out.push(f("SecurityIDSource", r.str(4)?));
            out.push(f("OwnerType", r.u16()?));
            out.push(f("ClearingFirm", r.str(2)?));
            out.push(f("TransactTime", fmt_time(r.i64()?)));
            out.push(f("UserInfo", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("OrigClOrdID", r.str(10)?));
            out.push(f("Side", side_label(r.ch()?)));
            out.push(f("OrdStatus", ord_status_label(r.ch()?)));
            out.push(f("CxlRejReason", r.u16()?));
            out.push(f("RejectText", r.str(16)?));
            out.push(f("OrderID", r.str(16)?));
        }
        msg_type::EXEC_RPT_CASH_ACK => {
            out.push(f("PartitionNo", r.i32()?));
            out.push(f("ReportIndex", r.i64()?));
            out.push(f("ApplID", appl_id_label(&r.str(3)?)));
            out.push(f("ReportingPBUId", r.str(6)?));
            out.push(f("SubmittingPBUId", r.str(6)?));
            out.push(f("SecurityID", r.str(8)?));
            out.push(f("SecurityIDSource", r.str(4)?));
            out.push(f("OwnerType", r.u16()?));
            out.push(f("ClearingFirm", r.str(2)?));
            out.push(f("TransactTime", fmt_time(r.i64()?)));
            out.push(f("UserInfo", r.str(8)?));
            out.push(f("OrderID", r.str(16)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("OrigClOrdID", r.str(10)?));
            out.push(f("ExecID", r.str(16)?));
            out.push(f("ExecType", exec_type_label(r.ch()?)));
            out.push(f("OrdStatus", ord_status_label(r.ch()?)));
            out.push(f("OrdRejReason", r.u16()?));
            out.push(f("LeavesQty", format!("{} 股", fmt_scaled(r.i64()?, 100))));
            out.push(f("CumQty", format!("{} 股", fmt_scaled(r.i64()?, 100))));
            out.push(f("Side", side_label(r.ch()?)));
            out.push(f("OrdType", ord_type_label(r.ch()?)));
            out.push(f("OrderQty", format!("{} 股", fmt_scaled(r.i64()?, 100))));
            out.push(f("Price", format!("{} 元", fmt_scaled(r.i64()?, 10000))));
            out.push(f("AccountID", r.str(12)?));
            out.push(f("BranchID", r.str(4)?));
            out.push(f("OrderRestrictions", r.str(4)?));
            // 扩展字段（200102）：我方发送的回报总是带全，防御性检查剩余长度
            if r.remaining() >= 20 {
                out.push(f("StopPx", format!("{} 元", fmt_scaled(r.i64()?, 10000))));
                out.push(f("MinQty", format!("{} 股", fmt_scaled(r.i64()?, 100))));
                out.push(f("MaxPriceLevels", r.u16()?));
                out.push(f("TimeInForce", r.ch()?));
                out.push(f("CashMargin", r.ch()?));
            }
        }
        msg_type::EXEC_RPT_CASH_TRADE => {
            out.push(f("PartitionNo", r.i32()?));
            out.push(f("ReportIndex", r.i64()?));
            out.push(f("ApplID", appl_id_label(&r.str(3)?)));
            out.push(f("ReportingPBUId", r.str(6)?));
            out.push(f("SubmittingPBUId", r.str(6)?));
            out.push(f("SecurityID", r.str(8)?));
            out.push(f("SecurityIDSource", r.str(4)?));
            out.push(f("OwnerType", r.u16()?));
            out.push(f("ClearingFirm", r.str(2)?));
            out.push(f("TransactTime", fmt_time(r.i64()?)));
            out.push(f("UserInfo", r.str(8)?));
            out.push(f("OrderID", r.str(16)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("ExecID", r.str(16)?));
            out.push(f("ExecType", exec_type_label(r.ch()?)));
            out.push(f("OrdStatus", ord_status_label(r.ch()?)));
            out.push(f("LastPx", format!("{} 元", fmt_scaled(r.i64()?, 10000))));
            out.push(f("LastQty", format!("{} 股", fmt_scaled(r.i64()?, 100))));
            out.push(f("LeavesQty", format!("{} 股", fmt_scaled(r.i64()?, 100))));
            out.push(f("CumQty", format!("{} 股", fmt_scaled(r.i64()?, 100))));
            out.push(f("Side", side_label(r.ch()?)));
            out.push(f("AccountID", r.str(12)?));
            out.push(f("BranchID", r.str(4)?));
            out.push(f("CashMargin", r.ch()?));
        }
        // 其他消息类型：不逐字段解析（调用方只展示原始报文）
        _ => {}
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_new_order_fields() {
        // 复用测试构造的委托消息体，验证解析出关键字段
        let mut w = BodyWriter::new();
        w.str("010", 3);
        w.str("100001", 6);
        w.str("000001", 8);
        w.str("102", 4);
        w.u16(1);
        w.str("01", 2);
        w.i64(20260731093000123);
        w.str("UI", 8);
        w.str("CL0000001", 10);
        w.str("0123456789AB", 12);
        w.str("0001", 4);
        w.str("", 4);
        w.ch(b'1');
        w.ch(b'2');
        w.i64(100_00);
        w.i64(12_3400);
        w.i64(0);
        w.i64(0);
        w.u16(0);
        w.ch(b'0');
        w.ch(b'1');
        let body = w.into_inner();
        let fields = describe_fields(msg_type::NEW_ORDER_CASH, &body);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"MsgType"));
        assert!(names.contains(&"SecurityID"));
        assert!(names.contains(&"ClOrdID"));
        // 数量/价格换算自然单位并带单位
        let qty = fields.iter().find(|f| f.name == "OrderQty").unwrap();
        assert_eq!(qty.value, "100 股");
        let px = fields.iter().find(|f| f.name == "Price").unwrap();
        assert_eq!(px.value, "12.34 元");
        // 买卖方向带中文含义
        let side = fields.iter().find(|f| f.name == "Side").unwrap();
        assert_eq!(side.value, "1 (买)");
    }

    #[test]
    fn describe_unknown_type_keeps_msgtype_only() {
        let fields = describe_fields(999_999, b"xx");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "MsgType");
        assert!(fields[0].value.contains("未知"));
    }

    #[test]
    fn scaled_formatting() {
        assert_eq!(fmt_scaled(12_3400, 10000), "12.34");
        assert_eq!(fmt_scaled(100_00, 100), "100");
        assert_eq!(fmt_scaled(-5_0000, 10000), "-5");
        assert_eq!(fmt_scaled(0, 10000), "0");
        assert_eq!(fmt_scaled(100_0005, 10000), "100.0005");
    }

    #[test]
    fn test_checksum() {
        // 简单字节和 mod 256
        assert_eq!(checksum(&[1, 2, 3]), 6);
        assert_eq!(checksum(&[255, 255]), (255u32 + 255) % 256);
    }

    #[test]
    fn test_frame_roundtrip() {
        let body = vec![0x01u8, 0x02, 0x03];
        let f = frame(msg_type::HEARTBEAT, &body);
        assert_eq!(&f[0..4], &3u32.to_be_bytes());
        assert_eq!(&f[4..8], &3u32.to_be_bytes());
        let cks = u32::from_be_bytes(f[f.len() - 4..].try_into().unwrap());
        assert_eq!(cks, checksum(&f[..f.len() - 4]));
    }

    #[test]
    fn test_logon_roundtrip() {
        let l = Logon {
            sender_comp_id: "OMS001".into(),
            target_comp_id: "TGW001".into(),
            heart_bt_int: 30,
            password: "pass".into(),
            default_appl_ver_id: "1.29".into(),
        };
        let f = l.encode();
        let body = &f[8..f.len() - 4];
        assert_eq!(body.len(), Logon::BODY_LEN);
        let d = Logon::decode(body).unwrap();
        assert_eq!(d.sender_comp_id, "OMS001");
        assert_eq!(d.heart_bt_int, 30);
        assert_eq!(d.default_appl_ver_id, "1.29");
    }

    #[test]
    fn test_new_order_decode() {
        // 手工构造一笔委托
        let mut w = BodyWriter::new();
        w.str("010", 3);
        w.str("100001", 6);
        w.str("000001", 8);
        w.str("102", 4);
        w.u16(1);
        w.str("01", 2);
        w.i64(20260731093000123);
        w.str("UI", 8);
        w.str("CL0000001", 10);
        w.str("0123456789AB", 12);
        w.str("0001", 4);
        w.str("", 4);
        w.ch(b'1');
        w.ch(b'2');
        w.i64(100_00); // 100 股
        w.i64(12_3400); // 12.34 元
        w.i64(0);
        w.i64(0);
        w.u16(0);
        w.ch(b'0');
        w.ch(b'1');
        let body = w.into_inner();
        let o = NewOrderCash::decode(&body).unwrap();
        assert_eq!(o.security_id, "000001");
        assert_eq!(o.cl_ord_id, "CL0000001");
        assert_eq!(o.side, b'1');
        assert_eq!(o.order_qty, 100_00);
        assert_eq!(o.price, 12_3400);
        assert_eq!(o.cash_margin, b'1');
    }
}
