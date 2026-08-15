//! 深交所 Binary 协议报文的编解码（结构体 ↔ 字节串）。
//!
//! # 协议背景
//!
//! 柜台（OMS）与交易网关（TGW）之间通过 TCP 长连接传输二进制报文，
//! 报文字段布局由《深圳证券交易所 Binary 交易数据接口规范》约定。
//! 本文件负责结构体 ↔ 字节串的编码/解码，代码组织顺序与规范第 4.5 节
//! “业务消息-新订单处理”保持一致：
//!
//! ```text
//! 4.5.1 新订单（New Order）           —— 4.5.1.1 ~ 4.5.1.19 共 19 种业务
//! 4.5.2 撤单请求（Order Cancel Request）
//! 4.5.3 撤单失败响应（Cancel Reject）
//! 4.5.4 订单响应及撤单成功执行报告     —— 4.5.4.1 ~ 4.5.4.20 按业务带扩展字段
//! 4.5.5 订单成交执行报告               —— 4.5.5.1 ~ 4.5.5.8 按业务带扩展字段
//! ```
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
//!
//! # 消息类型编号规律
//!
//! - 新订单：1xxx01（xxx = 业务代码，如 100101 = 现货竞价、100201 = 债券回购）
//! - 确认执行报告：2xxx02（如 200102 = 现货竞价确认）
//! - 成交执行报告：2xxx15（如 200115 = 现货竞价成交）
//! - 撤单请求：190007、撤单失败响应：290008（各业务共用）
//! - ApplID 前两位 = 业务代码，第三位 = 该业务下的委托申报代码（如表 3-3）

use std::io;

use crate::capture::ParsedField;

/// 消息类型常量（报文头的 MsgType 字段，括号内为规范章节号）。
///
/// 分三类：
/// - 会话层（1~9）：登录、注销、心跳等“连接维护”消息，与具体业务无关
/// - 业务层（六位数）：委托、回报、撤单等真正的交易消息
pub mod msg_type {
    // ---- 会话层（4.4 / 5.x）----
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

    // ---- 4.5.1 新订单（New Order，MsgType = 1xxx01），按规范小节顺序 ----
    /// 4.5.1.1 现货集中竞价交易业务新订单（100101，ApplID=010）
    pub const NEW_ORDER_CASH: u32 = 100_101;
    /// 4.5.1.2 债券通用质押式回购交易业务新订单（100201，ApplID=020）
    pub const NEW_ORDER_BOND_REPO: u32 = 100_201;
    /// 表 3-3 债券分销订单申报（100301，ApplID=030，无扩展字段）
    pub const NEW_ORDER_BOND_DIST: u32 = 100_301;
    /// 4.5.1.3 期权集中竞价交易业务新订单（100401，ApplID=040）
    pub const NEW_ORDER_OPTION_AUCTION: u32 = 100_401;
    /// 4.5.1.4 协议交易业务新订单（100501，ApplID=051 定价 / 052 点击成交）
    pub const NEW_ORDER_AGREEMENT_TRADE: u32 = 100_501;
    /// 4.5.1.5 盘后定价大宗交易业务新订单（100601，ApplID=060 收盘价 / 061 VWAP）
    pub const NEW_ORDER_BLOCK_TRADE: u32 = 100_601;
    /// 4.5.1.6 转融通证券出借业务新订单（100701，ApplID=070 非约定申报）
    pub const NEW_ORDER_SEC_LENDING: u32 = 100_701;
    /// 4.5.1.19 ETF 实时申购赎回业务新订单（101201，ApplID=120）
    pub const NEW_ORDER_ETF_SUB_RED: u32 = 101_201;
    /// 表 3-3 网上发行认购订单申报（101301，ApplID=130/131/132，无扩展字段）
    pub const NEW_ORDER_ISSUE: u32 = 101_301;
    /// 表 3-3 配股认购订单申报（101401，ApplID=140，无扩展字段）
    pub const NEW_ORDER_RIGHTS: u32 = 101_401;
    /// 4.5.1.7 债券转股回售业务新订单（101501，ApplID=150 转股 / 151 回售 / 152 回售撤销）
    pub const NEW_ORDER_BOND_CONVERT: u32 = 101_501;
    /// 4.5.1.8 期权行权业务新订单（101601，ApplID=160）
    pub const NEW_ORDER_OPTION_EXERCISE: u32 = 101_601;
    /// 4.5.1.9 开放式基金申购赎回业务新订单（101701，ApplID=170）
    pub const NEW_ORDER_FUND_SUB_RED: u32 = 101_701;
    /// 4.5.1.10 要约收购业务新订单（101801，ApplID=180 预受要约 / 181 解除预受）
    pub const NEW_ORDER_TENDER_OFFER: u32 = 101_801;
    /// 表 3-3 债券通用质押式回购质押/解押订单申报（101901，ApplID=190/191，无扩展字段）
    pub const NEW_ORDER_REPO_PLEDGE: u32 = 101_901;
    /// 表 3-3 黄金 ETF 实物申购赎回订单申报（102201，ApplID=220，无扩展字段）
    pub const NEW_ORDER_GOLD_ETF: u32 = 102_201;
    /// 表 3-3 权证行权订单申报（102301，ApplID=230，无扩展字段）
    pub const NEW_ORDER_WARRANT: u32 = 102_301;
    /// 4.5.1.11 转处置业务新订单（102701，ApplID=270 扣券 / 271 还券）
    pub const NEW_ORDER_DISPOSAL: u32 = 102_701;
    /// 4.5.1.12 垫券还券业务新订单（102801，ApplID=280 垫券 / 281 还券）
    pub const NEW_ORDER_LEND_RETURN: u32 = 102_801;
    /// 4.5.1.13 待清偿扣划业务新订单（102901，ApplID=290 客户 / 291 自营）
    pub const NEW_ORDER_DEDUCTION: u32 = 102_901;
    /// 表 3-3 分级基金实时分拆/合并订单申报（103101，ApplID=310/311，无扩展字段）
    pub const NEW_ORDER_SPLIT_MERGE: u32 = 103_101;
    /// 表 3-3 债券质押式三方回购入库/出库订单申报（103301，ApplID=330/331，无扩展字段）
    pub const NEW_ORDER_3P_REPO: u32 = 103_301;
    /// 4.5.1.15 期权普通与备兑仓互转业务新订单（103501，ApplID=350/351）
    pub const NEW_ORDER_OPTION_CONVERT: u32 = 103_501;
    /// 4.5.1.16 盘后定价交易业务新订单（103701，ApplID=370）
    pub const NEW_ORDER_AFTER_HOURS: u32 = 103_701;
    /// 4.5.1.17(1) 债券现券交易匹配成交新订单（104101，ApplID=410）
    pub const NEW_ORDER_BOND_CASH: u32 = 104_101;
    /// 4.5.1.17(2) 债券现券交易竞买成交新订单（104128，ApplID=417）
    pub const NEW_ORDER_BOND_BID: u32 = 104_128;
    /// 4.5.1.18 跨银行间实物债券 ETF 实物申购赎回业务新订单（104701，ApplID=470）
    pub const NEW_ORDER_INTERBANK_ETF: u32 = 104_701;
    /// 4.5.1.14 港股通业务新订单（106301，ApplID=630）
    pub const NEW_ORDER_HK_CONNECT: u32 = 106_301;

    // ---- 4.5.2 撤单请求（各业务共用）----
    /// 撤单请求（4.5.2）
    pub const ORDER_CANCEL_REQUEST: u32 = 190_007;

    // ---- 4.5.3 撤单失败响应（各业务共用）----
    /// 撤单失败响应（4.5.3）
    pub const CANCEL_REJECT: u32 = 290_008;

    // ---- 4.5.4 订单响应及撤单成功执行报告（MsgType = 2xxx02），按规范小节顺序 ----
    /// 4.5.4.1 现货集中竞价业务执行报告（200102，ApplID=010）
    pub const EXEC_RPT_CASH_ACK: u32 = 200_102;
    /// 4.5.4.2 债券通用质押式回购业务执行报告（200202，ApplID=020）
    pub const EXEC_RPT_BOND_REPO_ACK: u32 = 200_202;
    /// 表 4-30 债券分销执行报告（200302，ApplID=030，无扩展字段）
    pub const EXEC_RPT_BOND_DIST_ACK: u32 = 200_302;
    /// 4.5.4.3 期权集中竞价业务执行报告（200402，ApplID=040）
    pub const EXEC_RPT_OPTION_ACK: u32 = 200_402;
    /// 4.5.4.4 协议交易业务执行报告（200502，ApplID=051/052）
    pub const EXEC_RPT_AGREEMENT_ACK: u32 = 200_502;
    /// 4.5.4.5 盘后定价大宗业务执行报告（200602，ApplID=060/061）
    pub const EXEC_RPT_BLOCK_ACK: u32 = 200_602;
    /// 4.5.4.6 转融通证券出借业务执行报告（200702，ApplID=070）
    pub const EXEC_RPT_SEC_LENDING_ACK: u32 = 200_702;
    /// 4.5.4.7 ETF 实时申购赎回业务执行报告（201202，ApplID=120）
    pub const EXEC_RPT_ETF_ACK: u32 = 201_202;
    /// 表 4-30 网上发行认购执行报告（201302，ApplID=130/131/132，无扩展字段）
    pub const EXEC_RPT_ISSUE_ACK: u32 = 201_302;
    /// 表 4-30 配股认购执行报告（201402，ApplID=140，无扩展字段）
    pub const EXEC_RPT_RIGHTS_ACK: u32 = 201_402;
    /// 4.5.4.8 债券转股回售业务执行报告（201502，ApplID=150/151/152）
    pub const EXEC_RPT_BOND_CONVERT_ACK: u32 = 201_502;
    /// 4.5.4.9 期权行权业务执行报告（201602，ApplID=160）
    pub const EXEC_RPT_OPTION_EXERCISE_ACK: u32 = 201_602;
    /// 4.5.4.10 开放式基金申购赎回业务执行报告（201702，ApplID=170）
    pub const EXEC_RPT_FUND_ACK: u32 = 201_702;
    /// 4.5.4.11 要约收购业务执行报告（201802，ApplID=180/181）
    pub const EXEC_RPT_TENDER_ACK: u32 = 201_802;
    /// 表 4-30 债券通用质押式回购质押/解押执行报告（201902，ApplID=190/191，无扩展字段）
    pub const EXEC_RPT_REPO_PLEDGE_ACK: u32 = 201_902;
    /// 表 4-30 黄金 ETF 实物申购赎回执行报告（202202，ApplID=220，无扩展字段）
    pub const EXEC_RPT_GOLD_ETF_ACK: u32 = 202_202;
    /// 表 4-30 权证行权执行报告（202302，ApplID=230，无扩展字段）
    pub const EXEC_RPT_WARRANT_ACK: u32 = 202_302;
    /// 4.5.4.12 转处置业务执行报告（202702，ApplID=270/271）
    pub const EXEC_RPT_DISPOSAL_ACK: u32 = 202_702;
    /// 4.5.4.13 垫券还券业务执行报告（202802，ApplID=280/281）
    pub const EXEC_RPT_LEND_RETURN_ACK: u32 = 202_802;
    /// 4.5.4.14 待清偿扣划业务执行报告（202902，ApplID=290/291）
    pub const EXEC_RPT_DEDUCTION_ACK: u32 = 202_902;
    /// 4.5.4.15 分级基金实时分拆合并业务执行报告（203102，ApplID=310/311）
    pub const EXEC_RPT_SPLIT_ACK: u32 = 203_102;
    /// 表 4-30 债券质押式三方回购入库/出库执行报告（203302，ApplID=330/331，无扩展字段）
    pub const EXEC_RPT_3P_REPO_ACK: u32 = 203_302;
    /// 4.5.4.17 期权普通与备兑仓互转业务执行报告（203502，ApplID=350/351）
    pub const EXEC_RPT_OPTION_CONVERT_ACK: u32 = 203_502;
    /// 4.5.4.18 盘后定价交易业务执行报告（203702，ApplID=370）
    pub const EXEC_RPT_AFTER_HOURS_ACK: u32 = 203_702;
    /// 4.5.4.19(1) 债券现券交易匹配成交执行报告（204102，ApplID=410）
    pub const EXEC_RPT_BOND_CASH_ACK: u32 = 204_102;
    /// 4.5.4.19(2) 债券现券交易竞买成交执行报告（204129，ApplID=417）
    pub const EXEC_RPT_BOND_BID_ACK: u32 = 204_129;
    /// 4.5.4.20 跨银行间实物债券 ETF 实物申购赎回执行报告（204702，ApplID=470）
    pub const EXEC_RPT_INTERBANK_ETF_ACK: u32 = 204_702;
    /// 4.5.4.16 港股通业务执行报告（206302，ApplID=630）
    pub const EXEC_RPT_HK_ACK: u32 = 206_302;

    // ---- 4.5.5 订单成交执行报告（MsgType = 2xxx15），按规范小节顺序 ----
    /// 4.5.5.1 现货集中竞价业务成交执行报告（200115，ApplID=010）
    pub const EXEC_RPT_CASH_TRADE: u32 = 200_115;
    /// 4.5.5.2 债券通用质押式回购业务成交执行报告（200215，ApplID=020）
    pub const EXEC_RPT_BOND_REPO_TRADE: u32 = 200_215;
    /// 表 4-53 债券分销成交执行报告（200315，ApplID=030，无扩展字段）
    pub const EXEC_RPT_BOND_DIST_TRADE: u32 = 200_315;
    /// 4.5.5.3 期权集中竞价业务成交执行报告（200415，ApplID=040）
    pub const EXEC_RPT_OPTION_TRADE: u32 = 200_415;
    /// 4.5.5.4 协议交易业务成交执行报告（200515，ApplID=051/052）
    pub const EXEC_RPT_AGREEMENT_TRADE: u32 = 200_515;
    /// 4.5.5.5 盘后定价大宗业务成交执行报告（200615，ApplID=060/061）
    pub const EXEC_RPT_BLOCK_TRADE: u32 = 200_615;
    /// 4.5.5.6 转融通证券出借业务成交执行报告（200715，ApplID=070）
    pub const EXEC_RPT_SEC_LENDING_TRADE: u32 = 200_715;
    /// 4.5.5.7 盘后定价交易业务成交执行报告（203715，ApplID=370）
    pub const EXEC_RPT_AFTER_HOURS_TRADE: u32 = 203_715;
    /// 4.5.5.8(1) 债券现券交易匹配成交执行报告（204115，ApplID=410）
    pub const EXEC_RPT_BOND_CASH_TRADE: u32 = 204_115;
    /// 4.5.5.8(2) 债券现券交易竞买成交执行报告（204130，ApplID=417）
    pub const EXEC_RPT_BOND_BID_TRADE: u32 = 204_130;
    /// 表 4-53 港股通成交执行报告（206315，ApplID=630，无扩展字段）
    pub const EXEC_RPT_HK_TRADE: u32 = 206_315;
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
// 会话层消息（4.4 管理消息 / 5.x 平台消息）
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

/// 业务拒绝消息（MsgType=4，5.1 节）。
/// 委托未通过基本合法性检查时，交易系统以本消息通知 OMS（无执行报告）。
#[derive(Debug, Clone, Default)]
pub struct BusinessReject {
    pub appl_id: String,              // char[3] 应用标识（被拒委托的 ApplID）
    pub transact_time: i64,           // 拒绝时间
    pub submitting_pbu_id: String,    // char[6] 申报交易单元（回填委托值）
    pub security_id: String,          // char[8] 证券代码（回填委托值）
    pub security_id_source: String,   // char[4] 证券代码源（回填委托值）
    pub ref_seq_num: i64,             // 被拒绝消息的消息序号（模拟器填 0）
    pub ref_msg_type: u32,            // 被拒绝的消息类型（如 100101）
    pub business_reject_ref_id: String, // char[10] 被拒绝消息对应的业务 ID（ClOrdID）
    pub business_reject_reason: u16,  // 拒绝原因代码
    pub business_reject_text: String, // char[50] 拒绝原因说明
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

// ---------------------------------------------------------------------------
// 4.5.1 新订单（New Order）
// ---------------------------------------------------------------------------

/// 新订单公共字段（表 4-4，MsgType=1xxx01 消息的前 17 个字段，各业务相同）。
///
/// 所有新订单消息（现货竞价、债券回购、期权、协议、港股通……）都以这
/// 17 个字段开头，其后才是各业务自己的扩展字段（4.5.1.x）。
#[derive(Debug, Clone, Default)]
pub struct OrderCommon {
    pub appl_id: String,            // char[3] 应用标识（如 "010" 现货竞价）
    pub submitting_pbu_id: String,  // char[6] 申报交易单元
    pub security_id: String,        // char[8] 证券代码
    pub security_id_source: String, // char[4] 证券代码源（默认 "102" 深交所）
    pub owner_type: u16,            // 订单所有者类型（1 个人/102 会员/103 机构…）
    pub clearing_firm: String,      // char[2] 结算机构代码
    pub transact_time: i64,         // 委托时间 YYYYMMDDHHMMSSsss
    pub user_info: String,          // char[8] 用户私有信息
    pub cl_ord_id: String,          // char[10] 客户订单编号（订单唯一键）
    pub account_id: String,         // char[12] 证券账户
    pub branch_id: String,          // char[4] 营业部代码
    pub order_restrictions: String, // char[4] 订单限定（如 1=程序化交易）
    pub side: u8,                   // 买卖方向：1=买 2=卖 G=借入 F=出借 D=申购 E=赎回
    pub ord_type: u8,               // 订单类别：1=市价 2=限价 U=本方最优
    pub order_qty: i64,             // 订单数量 N15(2)，放大 100 倍
    pub price: i64,                 // 价格 N13(4)，放大 10000 倍
}

impl OrderCommon {
    /// 从 reader 当前位置读取 17 个公共字段（表 4-4 字段顺序）。
    pub fn decode_from(r: &mut BodyReader) -> io::Result<Self> {
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
            account_id: r.str(12)?,
            branch_id: r.str(4)?,
            order_restrictions: r.str(4)?,
            side: r.ch()?,
            ord_type: r.ch()?,
            order_qty: r.i64()?,
            price: r.i64()?,
        })
    }

    /// 公共字段的字节长度（3+6+8+4+2+2+8+8+10+12+4+4+1+1+8+8 = 89 字节）
    pub const COMMON_LEN: usize = 89;
}

/// ETF 实时申购赎回执行报告/新订单中的“成份股明细”重复组（表 4-37 / 表 4-26 附近）。
/// 每行记录一只成份股的交付数量与现金替代金额。
#[derive(Debug, Clone, Default)]
pub struct UnderlyingSecurity {
    pub security_id: String,          // char[8] 成份股/子基金证券代码
    pub security_id_source: String,   // char[4] 证券代码源（101=上交所 102=深交所）
    pub delivery_qty: i64,            // Qty 交付数量
    pub subst_cash: i64,              // Amt 现金替代金额（分级基金无此字段）
}

/// ETF 实时申购赎回新订单/执行报告中的“其他市场账户信息”重复组（表 4-26 / 表 4-37）。
/// 仅跨市场股票 ETF 全实物申赎模式填写（沪市证券账户与交易单元）。
#[derive(Debug, Clone, Default)]
pub struct OtherAccount {
    pub market_id: String, // char[8] 市场代码（XSHG=上交所）
    pub pbu_id: String,    // char[6] 交易单元
    pub account_id: String, // char[12] 证券账户
}

/// 各业务新订单/执行报告的扩展字段统一存放处（超集）。
///
/// 不同业务只用到其中一部分字段，未用到的保持默认值（字符串为空、数值为 0）。
/// 字段按所属业务分组注释，编码/解码时按业务（消息类型）挑选对应字段，
/// 字段顺序严格遵循规范 4.5.1.x / 4.5.4.x / 4.5.5.x 的表格。
#[derive(Debug, Clone, Default)]
pub struct ExtendFields {
    // ---- 竞价类业务共用（现货 010 / 债券回购 020 / 期权 040 / 港股通 630 / 债券现券 410）----
    /// 止损价（Price，港股通预留固定填 0）
    pub stop_px: i64,
    /// 最低成交数量（Qty）
    pub min_qty: i64,
    /// 最多成交价位数（0 表示不限制）
    pub max_price_levels: u16,
    /// 订单有效时间类型（0=当日有效 3=IOC 9=At Crossing）
    pub time_in_force: u8,
    /// 信用标识（1=Cash 2=Open 3=Close）
    pub cash_margin: u8,

    // ---- 期权业务（040 期权竞价 / 160 期权行权 / 350 备兑互转）----
    /// 平仓标识（'O' 开仓 'C' 平仓）
    pub position_effect: u8,
    /// 备兑标签（0=Covered 备兑 1=UnCovered 非备兑）
    pub covered_or_uncovered: u8,
    /// 合约账户标识码（char[6]）
    pub contract_account_code: String,
    /// 第二交易所订单编号（char[16]，组合策略单边平仓时填组合流水号）
    pub secondary_order_id: String,

    // ---- 协议交易（051/052）----
    /// 约定号（char[8]，点击成交申报填写）
    pub confirm_id: String,

    // ---- 转融通出借（070）----
    /// 期限，单位为天数（uInt16）
    pub expiration_days: u16,
    /// 期限类型（uInt8，1=固定期限）
    pub expiration_type: u8,
    /// 股份性质（char[2]，00=无限售流通股 07=首发后可出借限售股）
    pub share_property: String,
    /// 到期日（LocalMktDate YYYYMMDD，回购到期/转融通到期成交回报用）
    pub maturity_date: u32,

    // ---- 开放式基金申购赎回（170）----
    /// 申购金额（Amt；LOF 现金申购时填金额，OrderQty 填 0）
    pub cash_order_qty: i64,

    // ---- 要约收购（180/181）----
    /// 收购人编码（char[6]）
    pub tenderer: String,

    // ---- 转处置（270/271）----
    /// 划入待处置券的交易单元（char[6]）
    pub disposal_pbu: String,
    /// 划入待处置券的证券账户（char[12]）
    pub disposal_account_id: String,

    // ---- 垫券还券（280/281）----
    /// 出借券交易单元（char[6]）
    pub lender_pbu: String,
    /// 出借券证券账户（char[12]）
    pub lender_account_id: String,

    // ---- 待清偿扣划（290/291）----
    /// 用于扣划证券的交易单元（char[6]）
    pub deduction_pbu: String,
    /// 用于扣划证券的证券账户（char[12]）
    pub deduction_account_id: String,

    // ---- 港股通（630）----
    /// 订单数量类型（char，1=零股 2=整手）
    pub lot_type: u8,
    /// 拒绝原因说明（char[16]，填联交所拒绝原因代码；仅执行报告）
    pub reject_text: String,
    /// 联交所拒绝原因说明长度（Length，uInt32）
    pub imc_reject_text_len: u32,
    /// 联交所拒绝原因说明（变长字符串，最长 150 字节）
    pub imc_reject_text: String,

    // ---- 债券现券竞买成交（417，表 4-24 / 表 4-50 / 表 4-62）----
    /// 本方交易商代码（char[6]）
    pub member_id: String,
    /// 本方交易主体类型（char[2]，01=自营 02=资管 03=机构经纪 04=个人经纪）
    pub investor_type: String,
    /// 本方交易主体代码（char[10]）
    pub investor_id: String,
    /// 本方客户名称（char[120]，可含中文）
    pub investor_name: String,
    /// 本方交易员代码（char[8]）
    pub trader_code: String,
    /// 竞买业务类别（uInt16，1=预约 2=发起 3=应价）
    pub bid_trans_type: u16,
    /// 竞买成交方式（uInt16，1=单一主体中标 2=多主体单一价格 3=多主体多重价格）
    pub bid_exec_inst_type: u16,
    /// 价格下限（Price）
    pub low_limit_price: i64,
    /// 价格上限（Price）
    pub high_limit_price: i64,
    /// 交易日期（LocalMktDate YYYYMMDD）
    pub trade_date: u32,
    /// 结算方式（uInt16，0=不指定 103=多边净额 104=逐笔全额）
    pub settl_type: u16,
    /// 结算周期（uInt8，0=T+0 1=T+1 2=T+2 3=T+3）
    pub settl_period: u8,
    /// 是否匿名（uInt8，0=显名 1=匿名）
    pub pre_trade_anonymity: u8,
    /// 备注（char[160]，可含中文）
    pub memo: String,

    // ---- 债券现券匹配成交回报的对手方信息（204115/204130，表 4-61/4-62）----
    /// 对手方交易商代码（char[6]）
    pub counterparty_member_id: String,
    /// 对手方交易主体类型（char[2]）
    pub counterparty_investor_type: String,
    /// 对手方交易主体代码（char[10]）
    pub counterparty_investor_id: String,
    /// 对手方客户名称（char[120]）
    pub counterparty_investor_name: String,
    /// 对手方交易员代码（char[8]）
    pub counterparty_trader_code: String,

    // ---- ETF 实时申购赎回（120，表 4-26 / 表 4-37）----
    /// 申赎不足成份股证券代码（char[8]，填第一只不足额的成份股代码）
    pub insufficient_security_id: String,
    /// 成份股记录数（NumInGroup，其后跟 NoSecurity 组重复组）
    pub no_security: u32,
    /// 成份股明细（重复组，每行 4 个字段）
    pub underlying_securities: Vec<UnderlyingSecurity>,
    /// 其他市场账户信息组数（NumInGroup）
    pub no_accounts: u32,
    /// 其他市场账户明细（重复组，每行 3 个字段）
    pub other_accounts: Vec<OtherAccount>,
}

/// 一笔已解析的新订单：消息类型 + 公共字段 + 业务扩展字段。
///
/// 这是 4.5.1 全部新订单业务（现货竞价、债券回购、期权、协议、港股通等
/// 27 种消息类型）的统一表示：解码时按消息类型读取对应业务的扩展字段，
/// 回报时按消息类型查询业务特征（strategy::biz_info）确定确认/成交报告的
/// 报文类型与扩展字段回填规则。
#[derive(Debug, Clone, Default)]
pub struct NewOrder {
    /// 原始消息类型（如 100101 现货竞价），决定业务归属与扩展字段布局
    pub msg_type: u32,
    /// 表 4-4 公共字段（各业务相同）
    pub common: OrderCommon,
    /// 各业务扩展字段（超集，仅本业务相关字段有值）
    pub extend: ExtendFields,
}

impl NewOrder {
    /// 按消息类型解码一笔新订单。
    ///
    /// 先读公共字段，再按文档 4.5.1.1 ~ 4.5.1.19 的顺序分派扩展字段。
    /// 扩展字段“容忍缺失”：对端只发公共字段（旧柜台）时剩余长度不足，
    /// 则跳过扩展字段，保证不解析失败。
    pub fn decode(mt: u32, body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        let common = OrderCommon::decode_from(&mut r)?;
        let mut o = NewOrder { msg_type: mt, common, extend: ExtendFields::default() };
        match mt {
            // 4.5.1.1 现货集中竞价（100101）表 4-6：StopPx/MinQty/MaxPriceLevels/TimeInForce/CashMargin
            msg_type::NEW_ORDER_CASH => {
                if r.remaining() >= 20 {
                    o.extend.stop_px = r.i64()?;
                    o.extend.min_qty = r.i64()?;
                    o.extend.max_price_levels = r.u16()?;
                    o.extend.time_in_force = r.ch()?;
                    o.extend.cash_margin = r.ch()?;
                }
            }
            // 4.5.1.2 债券通用质押式回购（100201）表 4-8：StopPx/MinQty/MaxPriceLevels/TimeInForce
            msg_type::NEW_ORDER_BOND_REPO => {
                if r.remaining() >= 19 {
                    o.extend.stop_px = r.i64()?;
                    o.extend.min_qty = r.i64()?;
                    o.extend.max_price_levels = r.u16()?;
                    o.extend.time_in_force = r.ch()?;
                }
            }
            // 表 3-3 债券分销（100301）：无扩展字段
            msg_type::NEW_ORDER_BOND_DIST => {}
            // 4.5.1.3 期权集中竞价（100401）表 4-9：
            // StopPx/MinQty/MaxPriceLevels/TimeInForce/PositionEffect/CoveredOrUncovered/
            // ContractAccountCode/SecondaryOrderID
            msg_type::NEW_ORDER_OPTION_AUCTION => {
                if r.remaining() >= 43 {
                    o.extend.stop_px = r.i64()?;
                    o.extend.min_qty = r.i64()?;
                    o.extend.max_price_levels = r.u16()?;
                    o.extend.time_in_force = r.ch()?;
                    o.extend.position_effect = r.ch()?;
                    o.extend.covered_or_uncovered = r.ch()?;
                    o.extend.contract_account_code = r.str(6)?;
                    o.extend.secondary_order_id = r.str(16)?;
                }
            }
            // 4.5.1.4 协议交易（100501）表 4-10：ConfirmID/CashMargin
            msg_type::NEW_ORDER_AGREEMENT_TRADE => {
                if r.remaining() >= 9 {
                    o.extend.confirm_id = r.str(8)?;
                    o.extend.cash_margin = r.ch()?;
                }
            }
            // 4.5.1.5 盘后定价大宗交易（100601）表 4-11：CashMargin
            msg_type::NEW_ORDER_BLOCK_TRADE => {
                if r.remaining() >= 1 {
                    o.extend.cash_margin = r.ch()?;
                }
            }
            // 4.5.1.6 转融通证券出借（100701）表 4-12：ExpirationDays/ExpirationType/ShareProperty
            msg_type::NEW_ORDER_SEC_LENDING => {
                if r.remaining() >= 5 {
                    o.extend.expiration_days = r.u16()?;
                    o.extend.expiration_type = r.ch()?;
                    o.extend.share_property = r.str(2)?;
                }
            }
            // 4.5.1.19 ETF 实时申购赎回（101201）表 4-26：
            // NoAccounts + 重复组[MarketID/PBUID/AccountID]
            msg_type::NEW_ORDER_ETF_SUB_RED => {
                if r.remaining() >= 4 {
                    o.extend.no_accounts = r.u32()?;
                    for _ in 0..o.extend.no_accounts {
                        o.extend.other_accounts.push(OtherAccount {
                            market_id: r.str(8)?,
                            pbu_id: r.str(6)?,
                            account_id: r.str(12)?,
                        });
                    }
                }
            }
            // 表 3-3 网上发行认购（101301）：无扩展字段
            msg_type::NEW_ORDER_ISSUE => {}
            // 表 3-3 配股认购（101401）：无扩展字段
            msg_type::NEW_ORDER_RIGHTS => {}
            // 4.5.1.7 债券转股回售（101501）表 4-13：ShareProperty
            msg_type::NEW_ORDER_BOND_CONVERT => {
                if r.remaining() >= 2 {
                    o.extend.share_property = r.str(2)?;
                }
            }
            // 4.5.1.8 期权行权（101601）表 4-14：ContractAccountCode
            msg_type::NEW_ORDER_OPTION_EXERCISE => {
                if r.remaining() >= 6 {
                    o.extend.contract_account_code = r.str(6)?;
                }
            }
            // 4.5.1.9 开放式基金申购赎回（101701）表 4-15：CashOrderQty
            msg_type::NEW_ORDER_FUND_SUB_RED => {
                if r.remaining() >= 8 {
                    o.extend.cash_order_qty = r.i64()?;
                }
            }
            // 4.5.1.10 要约收购（101801）表 4-16：Tenderer
            msg_type::NEW_ORDER_TENDER_OFFER => {
                if r.remaining() >= 6 {
                    o.extend.tenderer = r.str(6)?;
                }
            }
            // 表 3-3 债券通用质押式回购质押/解押（101901）：无扩展字段
            msg_type::NEW_ORDER_REPO_PLEDGE => {}
            // 表 3-3 黄金 ETF 实物申购赎回（102201）：无扩展字段
            msg_type::NEW_ORDER_GOLD_ETF => {}
            // 表 3-3 权证行权（102301）：无扩展字段
            msg_type::NEW_ORDER_WARRANT => {}
            // 4.5.1.11 转处置（102701）表 4-17：DisposalPBU/DisposalAccountID
            msg_type::NEW_ORDER_DISPOSAL => {
                if r.remaining() >= 18 {
                    o.extend.disposal_pbu = r.str(6)?;
                    o.extend.disposal_account_id = r.str(12)?;
                }
            }
            // 4.5.1.12 垫券还券（102801）表 4-18：LenderPBU/LenderAccountID
            msg_type::NEW_ORDER_LEND_RETURN => {
                if r.remaining() >= 18 {
                    o.extend.lender_pbu = r.str(6)?;
                    o.extend.lender_account_id = r.str(12)?;
                }
            }
            // 4.5.1.13 待清偿扣划（102901）表 4-19：DeductionPBU/DeductionAccountID
            msg_type::NEW_ORDER_DEDUCTION => {
                if r.remaining() >= 18 {
                    o.extend.deduction_pbu = r.str(6)?;
                    o.extend.deduction_account_id = r.str(12)?;
                }
            }
            // 表 3-3 分级基金实时分拆/合并（103101）：无扩展字段
            msg_type::NEW_ORDER_SPLIT_MERGE => {}
            // 表 3-3 债券质押式三方回购入库/出库（103301）：无扩展字段
            msg_type::NEW_ORDER_3P_REPO => {}
            // 4.5.1.15 期权普通与备兑仓互转（103501）表 4-21：ContractAccountCode
            msg_type::NEW_ORDER_OPTION_CONVERT => {
                if r.remaining() >= 6 {
                    o.extend.contract_account_code = r.str(6)?;
                }
            }
            // 4.5.1.16 盘后定价交易（103701）表 4-22：CashMargin
            msg_type::NEW_ORDER_AFTER_HOURS => {
                if r.remaining() >= 1 {
                    o.extend.cash_margin = r.ch()?;
                }
            }
            // 4.5.1.17(1) 债券现券匹配成交（104101）表 4-23：StopPx/MinQty/MaxPriceLevels/TimeInForce/CashMargin
            msg_type::NEW_ORDER_BOND_CASH => {
                if r.remaining() >= 20 {
                    o.extend.stop_px = r.i64()?;
                    o.extend.min_qty = r.i64()?;
                    o.extend.max_price_levels = r.u16()?;
                    o.extend.time_in_force = r.ch()?;
                    o.extend.cash_margin = r.ch()?;
                }
            }
            // 4.5.1.17(2) 债券现券竞买成交（104128）表 4-24：17 个字段
            msg_type::NEW_ORDER_BOND_BID => {
                if r.remaining() >= 359 {
                    o.extend.member_id = r.str(6)?;
                    o.extend.investor_type = r.str(2)?;
                    o.extend.investor_id = r.str(10)?;
                    o.extend.investor_name = r.str(120)?;
                    o.extend.trader_code = r.str(8)?;
                    o.extend.secondary_order_id = r.str(16)?;
                    o.extend.bid_trans_type = r.u16()?;
                    o.extend.bid_exec_inst_type = r.u16()?;
                    o.extend.low_limit_price = r.i64()?;
                    o.extend.high_limit_price = r.i64()?;
                    o.extend.min_qty = r.i64()?;
                    o.extend.trade_date = r.u32()?;
                    o.extend.settl_type = r.u16()?;
                    o.extend.settl_period = r.ch()?;
                    o.extend.pre_trade_anonymity = r.ch()?;
                    o.extend.cash_margin = r.ch()?;
                    o.extend.memo = r.str(160)?;
                }
            }
            // 4.5.1.18 跨银行间实物债券 ETF 实物申购赎回（104701）表 4-25：SecondaryOrderID
            msg_type::NEW_ORDER_INTERBANK_ETF => {
                if r.remaining() >= 16 {
                    o.extend.secondary_order_id = r.str(16)?;
                }
            }
            // 4.5.1.14 港股通（106301）表 4-20：StopPx/MinQty/MaxPriceLevels/TimeInForce/LotType
            msg_type::NEW_ORDER_HK_CONNECT => {
                if r.remaining() >= 20 {
                    o.extend.stop_px = r.i64()?;
                    o.extend.min_qty = r.i64()?;
                    o.extend.max_price_levels = r.u16()?;
                    o.extend.time_in_force = r.ch()?;
                    o.extend.lot_type = r.ch()?;
                }
            }
            // 其他消息类型：仅公共字段（未知业务容错）
            _ => {}
        }
        Ok(o)
    }

    /// 是否 4.5.1 新订单消息类型（主循环分发用）。
    /// 28 种消息类型 = 4.5.1.1~4.5.1.19 各业务 + 表 3-3 中无扩展字段的 1xxx01 业务
    /// （100301/101301/101401/101901/102201/102301/103101/103301）。
    /// 注：表 3-3 中 100405/100417/100509/100503/100505/100517/100510/100703/100803/
    /// 100903/101003/101103/101621/102099/102197/102489/102587/103003/103203/
    /// 103421/104103/104105/104110/104117/104128/104203/104303 等非 1xxx01 格式的
    /// 委托申报（意向/报价/询价/转托管/投票/密码服务等）不在 4.5 节新订单消息定义范围内，
    /// 本网关暂不实现。
    pub fn is_new_order(mt: u32) -> bool {
        matches!(
            mt,
            msg_type::NEW_ORDER_CASH
                | msg_type::NEW_ORDER_BOND_REPO
                | msg_type::NEW_ORDER_BOND_DIST
                | msg_type::NEW_ORDER_OPTION_AUCTION
                | msg_type::NEW_ORDER_AGREEMENT_TRADE
                | msg_type::NEW_ORDER_BLOCK_TRADE
                | msg_type::NEW_ORDER_SEC_LENDING
                | msg_type::NEW_ORDER_ETF_SUB_RED
                | msg_type::NEW_ORDER_ISSUE
                | msg_type::NEW_ORDER_RIGHTS
                | msg_type::NEW_ORDER_BOND_CONVERT
                | msg_type::NEW_ORDER_OPTION_EXERCISE
                | msg_type::NEW_ORDER_FUND_SUB_RED
                | msg_type::NEW_ORDER_TENDER_OFFER
                | msg_type::NEW_ORDER_REPO_PLEDGE
                | msg_type::NEW_ORDER_GOLD_ETF
                | msg_type::NEW_ORDER_WARRANT
                | msg_type::NEW_ORDER_DISPOSAL
                | msg_type::NEW_ORDER_LEND_RETURN
                | msg_type::NEW_ORDER_DEDUCTION
                | msg_type::NEW_ORDER_SPLIT_MERGE
                | msg_type::NEW_ORDER_3P_REPO
                | msg_type::NEW_ORDER_OPTION_CONVERT
                | msg_type::NEW_ORDER_AFTER_HOURS
                | msg_type::NEW_ORDER_BOND_CASH
                | msg_type::NEW_ORDER_BOND_BID
                | msg_type::NEW_ORDER_INTERBANK_ETF
                | msg_type::NEW_ORDER_HK_CONNECT
        )
    }
}

// ---------------------------------------------------------------------------
// 4.5.2 撤单请求（Order Cancel Request）
// ---------------------------------------------------------------------------

/// 撤单请求（MsgType=190007，表 4-27，各业务共用）。
/// 请求里带原始订单的编号/方向/数量，交易系统据此定位要撤的委托。
#[derive(Debug, Clone, Default)]
pub struct OrderCancelRequest {
    pub appl_id: String,            // char[3] 应用标识（原订单的 ApplID）
    pub submitting_pbu_id: String,  // char[6] 申报交易单元
    pub security_id: String,        // char[8] 证券代码
    pub security_id_source: String, // char[4] 证券代码源
    pub owner_type: u16,            // 订单所有者类型
    pub clearing_firm: String,      // char[2] 结算机构代码
    pub transact_time: i64,         // 委托时间
    pub user_info: String,          // char[8] 用户私有信息
    pub cl_ord_id: String,          // char[10] 本次撤单请求的客户订单编号
    pub orig_cl_ord_id: String,     // char[10] 原始订单客户订单编号
    pub side: u8,                   // 原始订单买卖方向
    pub order_id: String,           // char[16] 原始订单交易所订单编号
    pub order_qty: i64,             // 原始订单数量（交易所不检查该值）
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

// ---------------------------------------------------------------------------
// 4.5.3 撤单失败响应（Cancel Reject）
// ---------------------------------------------------------------------------

/// 撤单失败响应（MsgType=290008，表 4-28，各业务共用）。
/// 撤单请求无法执行时（找不到原单 / 原单已终态）回给 OMS。
#[derive(Debug, Clone, Default)]
pub struct CancelReject {
    pub partition_no: i32,          // 平台分区号
    pub report_index: i64,          // 回报记录号
    pub appl_id: String,            // char[3] 应用标识（回填撤单请求）
    pub reporting_pbu_id: String,   // char[6] 回报交易单元
    pub submitting_pbu_id: String,  // char[6] 申报交易单元
    pub security_id: String,        // char[8] 证券代码
    pub security_id_source: String, // char[4] 证券代码源
    pub owner_type: u16,            // 订单所有者类型
    pub clearing_firm: String,      // char[2] 结算机构代码
    pub transact_time: i64,         // 回报时间
    pub user_info: String,          // char[8] 用户私有信息
    pub cl_ord_id: String,          // char[10] 撤单请求编号
    pub orig_cl_ord_id: String,     // char[10] 原始订单编号
    pub side: u8,                   // 原始订单买卖方向
    pub ord_status: u8,             // 原始订单当前状态（找不到原单填 8=已拒绝）
    pub cxl_rej_reason: u16,        // 拒绝原因代码
    pub reject_text: String,        // char[16] 拒绝原因说明
    pub order_id: String,           // char[16] 原始订单交易所订单编号
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

// ---------------------------------------------------------------------------
// 4.5.4 订单响应及撤单成功执行报告（Execution Report，MsgType = 2xxx02）
// ---------------------------------------------------------------------------

/// 订单响应及撤单成功执行报告（表 4-29，各业务共用公共字段）。
///
/// 柜台每提交一笔委托，TGW 都要回一笔“确认执行报告”（ExecType=0 已报，
/// 或 ExecType=8 拒绝）；撤单成功时回 ExecType=4。27 种业务共用本结构，
/// 通过 `msg_type` 区分业务，扩展字段按 4.5.4.1 ~ 4.5.4.20 各表布局。
#[derive(Debug, Clone, Default)]
pub struct ExecRptAck {
    /// 执行报告消息类型（2xxx02，决定业务归属与扩展字段布局）
    pub msg_type: u32,
    pub partition_no: i32,          // 平台分区号
    pub report_index: i64,          // 回报记录号
    pub appl_id: String,            // char[3] 应用标识
    pub reporting_pbu_id: String,   // char[6] 回报交易单元
    pub submitting_pbu_id: String,  // char[6] 申报交易单元
    pub security_id: String,        // char[8] 证券代码
    pub security_id_source: String, // char[4] 证券代码源
    pub owner_type: u16,            // 订单所有者类型
    pub clearing_firm: String,      // char[2] 结算机构代码
    pub transact_time: i64,         // 回报时间 YYYYMMDDHHMMSSsss
    pub user_info: String,          // char[8] 用户私有信息
    pub order_id: String,           // char[16] 交易所订单编号
    pub cl_ord_id: String,          // char[10] 客户订单编号
    pub orig_cl_ord_id: String,     // char[10] 原始订单客户订单编号（撤单/拒绝时回填）
    pub exec_id: String,            // char[16] 执行编号
    pub exec_type: u8,              // 执行类型（0=已报 4=已撤 8=拒绝）
    pub ord_status: u8,             // 订单状态（0=已报 1=部分成交 2=全成 4=已撤 8=拒绝）
    pub ord_rej_reason: u16,        // 拒绝原因代码（港股通联交所拒绝时填 29998/29997）
    pub leaves_qty: i64,            // 订单剩余数量 N15(2)
    pub cum_qty: i64,               // 累计执行数量 N15(2)
    pub side: u8,                   // 买卖方向（回填委托）
    pub ord_type: u8,               // 订单类别（回填委托）
    pub order_qty: i64,             // 订单数量 N15(2)
    pub price: i64,                 // 价格 N13(4)
    pub account_id: String,         // char[12] 证券账户
    pub branch_id: String,          // char[4] 营业部代码
    pub order_restrictions: String, // char[4] 订单限定
    /// 各业务扩展字段（超集，仅本业务相关字段有值）
    pub extend: ExtendFields,
}

/// 公共字段字节长度：4+8+3+6+6+8+4+2+2+8+8+16+10+10+16+1+1+2+8+8+1+1+8+8+12+4+4 = 169
const EXEC_RPT_ACK_COMMON_LEN: usize = 169;

impl ExecRptAck {
    /// 编码成完整报文帧：公共字段 + 按消息类型分派的扩展字段。
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
        self.write_ack_extend(&mut w);
        frame(self.msg_type, &w.into_inner())
    }

    /// 按消息类型写确认报告扩展字段（4.5.4.1 ~ 4.5.4.20 顺序）。
    fn write_ack_extend(&self, w: &mut BodyWriter) {
        match self.msg_type {
            // 4.5.4.1 现货集中竞价（200102）表 4-31：StopPx/MinQty/MaxPriceLevels/TimeInForce/CashMargin
            msg_type::EXEC_RPT_CASH_ACK => {
                w.i64(self.extend.stop_px);
                w.i64(self.extend.min_qty);
                w.u16(self.extend.max_price_levels);
                w.ch(self.extend.time_in_force);
                w.ch(self.extend.cash_margin);
            }
            // 4.5.4.2 债券通用质押式回购（200202）表 4-32：StopPx/MinQty/MaxPriceLevels/TimeInForce
            msg_type::EXEC_RPT_BOND_REPO_ACK => {
                w.i64(self.extend.stop_px);
                w.i64(self.extend.min_qty);
                w.u16(self.extend.max_price_levels);
                w.ch(self.extend.time_in_force);
            }
            // 表 4-30 债券分销（200302）：无扩展字段
            msg_type::EXEC_RPT_BOND_DIST_ACK => {}
            // 4.5.4.3 期权集中竞价（200402）表 4-33：
            // StopPx/MinQty/MaxPriceLevels/TimeInForce/PositionEffect/CoveredOrUncovered/
            // ContractAccountCode/SecondaryOrderID
            msg_type::EXEC_RPT_OPTION_ACK => {
                w.i64(self.extend.stop_px);
                w.i64(self.extend.min_qty);
                w.u16(self.extend.max_price_levels);
                w.ch(self.extend.time_in_force);
                w.ch(self.extend.position_effect);
                w.ch(self.extend.covered_or_uncovered);
                w.str(&self.extend.contract_account_code, 6);
                w.str(&self.extend.secondary_order_id, 16);
            }
            // 4.5.4.4 协议交易（200502）表 4-34：ConfirmID/CashMargin
            msg_type::EXEC_RPT_AGREEMENT_ACK => {
                w.str(&self.extend.confirm_id, 8);
                w.ch(self.extend.cash_margin);
            }
            // 4.5.4.5 盘后定价大宗（200602）表 4-35：CashMargin
            msg_type::EXEC_RPT_BLOCK_ACK => {
                w.ch(self.extend.cash_margin);
            }
            // 4.5.4.6 转融通证券出借（200702）表 4-36：ExpirationDays/ExpirationType/ShareProperty
            msg_type::EXEC_RPT_SEC_LENDING_ACK => {
                w.u16(self.extend.expiration_days);
                w.ch(self.extend.expiration_type);
                w.str(&self.extend.share_property, 2);
            }
            // 4.5.4.7 ETF 实时申购赎回（201202）表 4-37：
            // InsufficientSecurityID + NoSecurity 重复组[4 字段] + NoAccounts 重复组[3 字段]
            msg_type::EXEC_RPT_ETF_ACK => {
                w.str(&self.extend.insufficient_security_id, 8);
                w.u32(self.extend.no_security);
                for u in &self.extend.underlying_securities {
                    w.str(&u.security_id, 8);
                    w.str(&u.security_id_source, 4);
                    w.i64(u.delivery_qty);
                    w.i64(u.subst_cash);
                }
                w.u32(self.extend.no_accounts);
                for a in &self.extend.other_accounts {
                    w.str(&a.market_id, 8);
                    w.str(&a.pbu_id, 6);
                    w.str(&a.account_id, 12);
                }
            }
            // 表 4-30 网上发行（201302）：无扩展字段
            msg_type::EXEC_RPT_ISSUE_ACK => {}
            // 表 4-30 配股（201402）：无扩展字段
            msg_type::EXEC_RPT_RIGHTS_ACK => {}
            // 4.5.4.8 债券转股回售（201502）表 4-38：ShareProperty
            msg_type::EXEC_RPT_BOND_CONVERT_ACK => {
                w.str(&self.extend.share_property, 2);
            }
            // 4.5.4.9 期权行权（201602）表 4-39：ContractAccountCode
            msg_type::EXEC_RPT_OPTION_EXERCISE_ACK => {
                w.str(&self.extend.contract_account_code, 6);
            }
            // 4.5.4.10 开放式基金申购赎回（201702）表 4-40：CashOrderQty
            msg_type::EXEC_RPT_FUND_ACK => {
                w.i64(self.extend.cash_order_qty);
            }
            // 4.5.4.11 要约收购（201802）表 4-41：Tenderer
            msg_type::EXEC_RPT_TENDER_ACK => {
                w.str(&self.extend.tenderer, 6);
            }
            // 表 4-30 回购质押/解押（201902）：无扩展字段
            msg_type::EXEC_RPT_REPO_PLEDGE_ACK => {}
            // 表 4-30 黄金 ETF（202202）：无扩展字段
            msg_type::EXEC_RPT_GOLD_ETF_ACK => {}
            // 表 4-30 权证行权（202302）：无扩展字段
            msg_type::EXEC_RPT_WARRANT_ACK => {}
            // 4.5.4.12 转处置（202702）表 4-42：DisposalPBU/DisposalAccountID
            msg_type::EXEC_RPT_DISPOSAL_ACK => {
                w.str(&self.extend.disposal_pbu, 6);
                w.str(&self.extend.disposal_account_id, 12);
            }
            // 4.5.4.13 垫券还券（202802）表 4-43：LenderPBU/LenderAccountID
            msg_type::EXEC_RPT_LEND_RETURN_ACK => {
                w.str(&self.extend.lender_pbu, 6);
                w.str(&self.extend.lender_account_id, 12);
            }
            // 4.5.4.14 待清偿扣划（202902）表 4-44：DeductionPBU/DeductionAccountID
            msg_type::EXEC_RPT_DEDUCTION_ACK => {
                w.str(&self.extend.deduction_pbu, 6);
                w.str(&self.extend.deduction_account_id, 12);
            }
            // 4.5.4.15 分级基金实时分拆合并（203102）表 4-45：
            // InsufficientSecurityID + NoSecurity 重复组[3 字段，无 SubstCash]
            msg_type::EXEC_RPT_SPLIT_ACK => {
                w.str(&self.extend.insufficient_security_id, 8);
                w.u32(self.extend.no_security);
                for u in &self.extend.underlying_securities {
                    w.str(&u.security_id, 8);
                    w.str(&u.security_id_source, 4);
                    w.i64(u.delivery_qty);
                }
            }
            // 表 4-30 债券质押式三方回购（203302）：无扩展字段
            msg_type::EXEC_RPT_3P_REPO_ACK => {}
            // 4.5.4.17 期权普通与备兑仓互转（203502）表 4-47：ContractAccountCode
            msg_type::EXEC_RPT_OPTION_CONVERT_ACK => {
                w.str(&self.extend.contract_account_code, 6);
            }
            // 4.5.4.18 盘后定价交易（203702）表 4-48：CashMargin
            msg_type::EXEC_RPT_AFTER_HOURS_ACK => {
                w.ch(self.extend.cash_margin);
            }
            // 4.5.4.19(1) 债券现券匹配成交（204102）表 4-49：与现货相同 5 字段
            msg_type::EXEC_RPT_BOND_CASH_ACK => {
                w.i64(self.extend.stop_px);
                w.i64(self.extend.min_qty);
                w.u16(self.extend.max_price_levels);
                w.ch(self.extend.time_in_force);
                w.ch(self.extend.cash_margin);
            }
            // 4.5.4.19(2) 债券现券竞买成交（204129）表 4-50：17 个字段
            msg_type::EXEC_RPT_BOND_BID_ACK => {
                w.str(&self.extend.member_id, 6);
                w.str(&self.extend.investor_type, 2);
                w.str(&self.extend.investor_id, 10);
                w.str(&self.extend.investor_name, 120);
                w.str(&self.extend.trader_code, 8);
                w.str(&self.extend.secondary_order_id, 16);
                w.u16(self.extend.bid_trans_type);
                w.u16(self.extend.bid_exec_inst_type);
                w.i64(self.extend.low_limit_price);
                w.i64(self.extend.high_limit_price);
                w.i64(self.extend.min_qty);
                w.u32(self.extend.trade_date);
                w.u16(self.extend.settl_type);
                w.ch(self.extend.settl_period);
                w.ch(self.extend.pre_trade_anonymity);
                w.ch(self.extend.cash_margin);
                w.str(&self.extend.memo, 160);
            }
            // 4.5.4.20 跨银行间实物债券 ETF（204702）表 4-51：SecondaryOrderID
            msg_type::EXEC_RPT_INTERBANK_ETF_ACK => {
                w.str(&self.extend.secondary_order_id, 16);
            }
            // 4.5.4.16 港股通（206302）表 4-46：
            // RejectText/StopPx/MinQty/MaxPriceLevels/TimeInForce/LotType/IMCRejectTextLen/IMCRejectText(变长)
            msg_type::EXEC_RPT_HK_ACK => {
                w.str(&self.extend.reject_text, 16);
                w.i64(self.extend.stop_px);
                w.i64(self.extend.min_qty);
                w.u16(self.extend.max_price_levels);
                w.ch(self.extend.time_in_force);
                w.ch(self.extend.lot_type);
                w.u32(self.extend.imc_reject_text_len);
                w.str(&self.extend.imc_reject_text, self.extend.imc_reject_text_len as usize);
            }
            // 未知消息类型：仅公共字段（容错）
            _ => {}
        }
    }

    /// 按消息类型解码确认报告（公共字段 + 扩展字段）。
    /// 供 describe_fields 解析与 roundtrip 测试使用。
    pub fn decode(mt: u32, body: &[u8]) -> io::Result<Self> {
        // 防御：公共字段长度是硬性要求，不足说明数据错乱/不是本消息类型
        if body.len() < EXEC_RPT_ACK_COMMON_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("确认报告公共字段长度不足: {} < {}", body.len(), EXEC_RPT_ACK_COMMON_LEN),
            ));
        }
        let mut r = BodyReader::new(body);
        let mut a = ExecRptAck {
            msg_type: mt,
            partition_no: r.i32()?,
            report_index: r.i64()?,
            appl_id: r.str(3)?,
            reporting_pbu_id: r.str(6)?,
            submitting_pbu_id: r.str(6)?,
            security_id: r.str(8)?,
            security_id_source: r.str(4)?,
            owner_type: r.u16()?,
            clearing_firm: r.str(2)?,
            transact_time: r.i64()?,
            user_info: r.str(8)?,
            order_id: r.str(16)?,
            cl_ord_id: r.str(10)?,
            orig_cl_ord_id: r.str(10)?,
            exec_id: r.str(16)?,
            exec_type: r.ch()?,
            ord_status: r.ch()?,
            ord_rej_reason: r.u16()?,
            leaves_qty: r.i64()?,
            cum_qty: r.i64()?,
            side: r.ch()?,
            ord_type: r.ch()?,
            order_qty: r.i64()?,
            price: r.i64()?,
            account_id: r.str(12)?,
            branch_id: r.str(4)?,
            order_restrictions: r.str(4)?,
            extend: ExtendFields::default(),
        };
        a.read_ack_extend(&mut r)?;
        Ok(a)
    }

    /// 按消息类型读确认报告扩展字段（与 write_ack_extend 对称）。
    fn read_ack_extend(&mut self, r: &mut BodyReader) -> io::Result<()> {
        // 扩展字段“容忍缺失”：剩余长度不足时跳过（对端只发公共字段的容错）
        match self.msg_type {
            msg_type::EXEC_RPT_CASH_ACK | msg_type::EXEC_RPT_BOND_CASH_ACK => {
                if r.remaining() >= 20 {
                    self.extend.stop_px = r.i64()?;
                    self.extend.min_qty = r.i64()?;
                    self.extend.max_price_levels = r.u16()?;
                    self.extend.time_in_force = r.ch()?;
                    self.extend.cash_margin = r.ch()?;
                }
            }
            msg_type::EXEC_RPT_BOND_REPO_ACK => {
                if r.remaining() >= 19 {
                    self.extend.stop_px = r.i64()?;
                    self.extend.min_qty = r.i64()?;
                    self.extend.max_price_levels = r.u16()?;
                    self.extend.time_in_force = r.ch()?;
                }
            }
            msg_type::EXEC_RPT_OPTION_ACK => {
                if r.remaining() >= 43 {
                    self.extend.stop_px = r.i64()?;
                    self.extend.min_qty = r.i64()?;
                    self.extend.max_price_levels = r.u16()?;
                    self.extend.time_in_force = r.ch()?;
                    self.extend.position_effect = r.ch()?;
                    self.extend.covered_or_uncovered = r.ch()?;
                    self.extend.contract_account_code = r.str(6)?;
                    self.extend.secondary_order_id = r.str(16)?;
                }
            }
            msg_type::EXEC_RPT_AGREEMENT_ACK => {
                if r.remaining() >= 9 {
                    self.extend.confirm_id = r.str(8)?;
                    self.extend.cash_margin = r.ch()?;
                }
            }
            msg_type::EXEC_RPT_BLOCK_ACK | msg_type::EXEC_RPT_AFTER_HOURS_ACK => {
                if r.remaining() >= 1 {
                    self.extend.cash_margin = r.ch()?;
                }
            }
            msg_type::EXEC_RPT_SEC_LENDING_ACK => {
                if r.remaining() >= 5 {
                    self.extend.expiration_days = r.u16()?;
                    self.extend.expiration_type = r.ch()?;
                    self.extend.share_property = r.str(2)?;
                }
            }
            msg_type::EXEC_RPT_ETF_ACK => {
                if r.remaining() >= 12 {
                    self.extend.insufficient_security_id = r.str(8)?;
                    self.extend.no_security = r.u32()?;
                    for _ in 0..self.extend.no_security {
                        self.extend.underlying_securities.push(UnderlyingSecurity {
                            security_id: r.str(8)?,
                            security_id_source: r.str(4)?,
                            delivery_qty: r.i64()?,
                            subst_cash: r.i64()?,
                        });
                    }
                    self.extend.no_accounts = r.u32()?;
                    for _ in 0..self.extend.no_accounts {
                        self.extend.other_accounts.push(OtherAccount {
                            market_id: r.str(8)?,
                            pbu_id: r.str(6)?,
                            account_id: r.str(12)?,
                        });
                    }
                }
            }
            msg_type::EXEC_RPT_BOND_CONVERT_ACK => {
                if r.remaining() >= 2 {
                    self.extend.share_property = r.str(2)?;
                }
            }
            msg_type::EXEC_RPT_OPTION_EXERCISE_ACK | msg_type::EXEC_RPT_OPTION_CONVERT_ACK => {
                if r.remaining() >= 6 {
                    self.extend.contract_account_code = r.str(6)?;
                }
            }
            msg_type::EXEC_RPT_FUND_ACK => {
                if r.remaining() >= 8 {
                    self.extend.cash_order_qty = r.i64()?;
                }
            }
            msg_type::EXEC_RPT_TENDER_ACK => {
                if r.remaining() >= 6 {
                    self.extend.tenderer = r.str(6)?;
                }
            }
            msg_type::EXEC_RPT_DISPOSAL_ACK => {
                if r.remaining() >= 18 {
                    self.extend.disposal_pbu = r.str(6)?;
                    self.extend.disposal_account_id = r.str(12)?;
                }
            }
            msg_type::EXEC_RPT_LEND_RETURN_ACK => {
                if r.remaining() >= 18 {
                    self.extend.lender_pbu = r.str(6)?;
                    self.extend.lender_account_id = r.str(12)?;
                }
            }
            msg_type::EXEC_RPT_DEDUCTION_ACK => {
                if r.remaining() >= 18 {
                    self.extend.deduction_pbu = r.str(6)?;
                    self.extend.deduction_account_id = r.str(12)?;
                }
            }
            msg_type::EXEC_RPT_SPLIT_ACK => {
                if r.remaining() >= 12 {
                    self.extend.insufficient_security_id = r.str(8)?;
                    self.extend.no_security = r.u32()?;
                    for _ in 0..self.extend.no_security {
                        self.extend.underlying_securities.push(UnderlyingSecurity {
                            security_id: r.str(8)?,
                            security_id_source: r.str(4)?,
                            delivery_qty: r.i64()?,
                            subst_cash: 0,
                        });
                    }
                }
            }
            msg_type::EXEC_RPT_BOND_BID_ACK => {
                if r.remaining() >= 359 {
                    self.extend.member_id = r.str(6)?;
                    self.extend.investor_type = r.str(2)?;
                    self.extend.investor_id = r.str(10)?;
                    self.extend.investor_name = r.str(120)?;
                    self.extend.trader_code = r.str(8)?;
                    self.extend.secondary_order_id = r.str(16)?;
                    self.extend.bid_trans_type = r.u16()?;
                    self.extend.bid_exec_inst_type = r.u16()?;
                    self.extend.low_limit_price = r.i64()?;
                    self.extend.high_limit_price = r.i64()?;
                    self.extend.min_qty = r.i64()?;
                    self.extend.trade_date = r.u32()?;
                    self.extend.settl_type = r.u16()?;
                    self.extend.settl_period = r.ch()?;
                    self.extend.pre_trade_anonymity = r.ch()?;
                    self.extend.cash_margin = r.ch()?;
                    self.extend.memo = r.str(160)?;
                }
            }
            msg_type::EXEC_RPT_INTERBANK_ETF_ACK => {
                if r.remaining() >= 16 {
                    self.extend.secondary_order_id = r.str(16)?;
                }
            }
            msg_type::EXEC_RPT_HK_ACK => {
                if r.remaining() >= 40 {
                    self.extend.reject_text = r.str(16)?;
                    self.extend.stop_px = r.i64()?;
                    self.extend.min_qty = r.i64()?;
                    self.extend.max_price_levels = r.u16()?;
                    self.extend.time_in_force = r.ch()?;
                    self.extend.lot_type = r.ch()?;
                    self.extend.imc_reject_text_len = r.u32()?;
                    let n = self.extend.imc_reject_text_len as usize;
                    if r.remaining() >= n {
                        self.extend.imc_reject_text = r.str(n)?;
                    }
                }
            }
            // 其余消息类型：无扩展字段
            _ => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 4.5.5 订单成交执行报告（Execution Report，MsgType = 2xxx15）
// ---------------------------------------------------------------------------

/// 订单成交执行报告（表 4-52，各业务共用公共字段）。
///
/// 委托成交后 TGW 向柜台发送的逐笔成交回报（ExecType=F 成交）。
/// 表 4-53 列出的 11 种业务使用本结构，通过 `msg_type` 区分业务，
/// 扩展字段按 4.5.5.1 ~ 4.5.5.8 各表布局。
#[derive(Debug, Clone, Default)]
pub struct ExecRptTrade {
    /// 成交执行报告消息类型（2xxx15，决定业务归属与扩展字段布局）
    pub msg_type: u32,
    pub partition_no: i32,          // 平台分区号
    pub report_index: i64,          // 回报记录号
    pub appl_id: String,            // char[3] 应用标识
    pub reporting_pbu_id: String,   // char[6] 回报交易单元
    pub submitting_pbu_id: String,  // char[6] 申报交易单元
    pub security_id: String,        // char[8] 证券代码
    pub security_id_source: String, // char[4] 证券代码源
    pub owner_type: u16,            // 订单所有者类型
    pub clearing_firm: String,      // char[2] 结算机构代码
    pub transact_time: i64,         // 回报时间 YYYYMMDDHHMMSSsss
    pub user_info: String,          // char[8] 用户私有信息
    pub order_id: String,           // char[16] 交易所订单编号
    pub cl_ord_id: String,          // char[10] 客户订单编号
    pub exec_id: String,            // char[16] 执行编号
    pub exec_type: u8,              // 执行类型（F=成交）
    pub ord_status: u8,             // 订单状态（1=部分成交 2=全部成交）
    pub last_px: i64,               // 成交价 N13(4)
    pub last_qty: i64,              // 成交数量 N15(2)
    pub leaves_qty: i64,            // 订单剩余数量 N15(2)
    pub cum_qty: i64,               // 累计执行数量 N15(2)
    pub side: u8,                   // 买卖方向（回填委托）
    pub account_id: String,         // char[12] 证券账户
    pub branch_id: String,          // char[4] 营业部代码
    /// 各业务扩展字段（超集，仅本业务相关字段有值）
    pub extend: ExtendFields,
}

/// 公共字段字节长度：4+8+3+6+6+8+4+2+2+8+8+16+10+16+1+1+8+8+8+8+1+12+4 = 152
const EXEC_RPT_TRADE_COMMON_LEN: usize = 152;

impl ExecRptTrade {
    /// 编码成完整报文帧：公共字段 + 按消息类型分派的扩展字段。
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
        self.write_trade_extend(&mut w);
        frame(self.msg_type, &w.into_inner())
    }

    /// 按消息类型写成交报告扩展字段（4.5.5.1 ~ 4.5.5.8 顺序）。
    fn write_trade_extend(&self, w: &mut BodyWriter) {
        match self.msg_type {
            // 4.5.5.1 现货集中竞价（200115）表 4-54：CashMargin
            msg_type::EXEC_RPT_CASH_TRADE => {
                w.ch(self.extend.cash_margin);
            }
            // 4.5.5.2 债券通用质押式回购（200215）表 4-55：MaturityDate
            // （回购到期日系统主动下发的 ApplID=029 到期成交报告也走本布局）
            msg_type::EXEC_RPT_BOND_REPO_TRADE => {
                w.u32(self.extend.maturity_date);
            }
            // 表 4-53 债券分销（200315）：无扩展字段
            msg_type::EXEC_RPT_BOND_DIST_TRADE => {}
            // 4.5.5.3 期权集中竞价（200415）表 4-56：
            // PositionEffect/CoveredOrUncovered/ContractAccountCode/SecondaryOrderID
            msg_type::EXEC_RPT_OPTION_TRADE => {
                w.ch(self.extend.position_effect);
                w.ch(self.extend.covered_or_uncovered);
                w.str(&self.extend.contract_account_code, 6);
                w.str(&self.extend.secondary_order_id, 16);
            }
            // 4.5.5.4 协议交易（200515）表 4-57：ConfirmID/CashMargin
            msg_type::EXEC_RPT_AGREEMENT_TRADE => {
                w.str(&self.extend.confirm_id, 8);
                w.ch(self.extend.cash_margin);
            }
            // 4.5.5.5 盘后定价大宗（200615）表 4-58：CashMargin
            msg_type::EXEC_RPT_BLOCK_TRADE => {
                w.ch(self.extend.cash_margin);
            }
            // 4.5.5.6 转融通证券出借（200715）表 4-59：
            // ExpirationDays/ExpirationType/MaturityDate/ShareProperty
            msg_type::EXEC_RPT_SEC_LENDING_TRADE => {
                w.u16(self.extend.expiration_days);
                w.ch(self.extend.expiration_type);
                w.u32(self.extend.maturity_date);
                w.str(&self.extend.share_property, 2);
            }
            // 表 4-53 港股通（206315）：无扩展字段
            msg_type::EXEC_RPT_HK_TRADE => {}
            // 4.5.5.7 盘后定价交易（203715）表 4-60：CashMargin
            msg_type::EXEC_RPT_AFTER_HOURS_TRADE => {
                w.ch(self.extend.cash_margin);
            }
            // 4.5.5.8(1) 债券现券匹配成交（204115）表 4-61：
            // CashMargin/SettlType/SettlPeriod/Counterparty 五字段
            msg_type::EXEC_RPT_BOND_CASH_TRADE => {
                w.ch(self.extend.cash_margin);
                w.u16(self.extend.settl_type);
                w.ch(self.extend.settl_period);
                w.str(&self.extend.counterparty_member_id, 6);
                w.str(&self.extend.counterparty_investor_type, 2);
                w.str(&self.extend.counterparty_investor_id, 10);
                w.str(&self.extend.counterparty_investor_name, 120);
                w.str(&self.extend.counterparty_trader_code, 8);
            }
            // 4.5.5.8(2) 债券现券竞买成交（204130）表 4-62：本方+对手方+17 字段
            msg_type::EXEC_RPT_BOND_BID_TRADE => {
                w.str(&self.extend.member_id, 6);
                w.str(&self.extend.investor_type, 2);
                w.str(&self.extend.investor_id, 10);
                w.str(&self.extend.investor_name, 120);
                w.str(&self.extend.trader_code, 8);
                w.str(&self.extend.counterparty_member_id, 6);
                w.str(&self.extend.counterparty_investor_type, 2);
                w.str(&self.extend.counterparty_investor_id, 10);
                w.str(&self.extend.counterparty_investor_name, 120);
                w.str(&self.extend.counterparty_trader_code, 8);
                w.str(&self.extend.secondary_order_id, 16);
                w.u16(self.extend.bid_trans_type);
                w.u16(self.extend.bid_exec_inst_type);
                w.u16(self.extend.settl_type);
                w.ch(self.extend.settl_period);
                w.ch(self.extend.cash_margin);
                w.str(&self.extend.memo, 160);
            }
            // 未知消息类型：仅公共字段（容错）
            _ => {}
        }
    }

    /// 按消息类型解码成交报告（公共字段 + 扩展字段）。
    /// 供 describe_fields 解析与 roundtrip 测试使用。
    pub fn decode(mt: u32, body: &[u8]) -> io::Result<Self> {
        // 防御：公共字段长度是硬性要求，不足说明数据错乱/不是本消息类型
        if body.len() < EXEC_RPT_TRADE_COMMON_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("成交报告公共字段长度不足: {} < {}", body.len(), EXEC_RPT_TRADE_COMMON_LEN),
            ));
        }
        let mut r = BodyReader::new(body);
        let mut t = ExecRptTrade {
            msg_type: mt,
            partition_no: r.i32()?,
            report_index: r.i64()?,
            appl_id: r.str(3)?,
            reporting_pbu_id: r.str(6)?,
            submitting_pbu_id: r.str(6)?,
            security_id: r.str(8)?,
            security_id_source: r.str(4)?,
            owner_type: r.u16()?,
            clearing_firm: r.str(2)?,
            transact_time: r.i64()?,
            user_info: r.str(8)?,
            order_id: r.str(16)?,
            cl_ord_id: r.str(10)?,
            exec_id: r.str(16)?,
            exec_type: r.ch()?,
            ord_status: r.ch()?,
            last_px: r.i64()?,
            last_qty: r.i64()?,
            leaves_qty: r.i64()?,
            cum_qty: r.i64()?,
            side: r.ch()?,
            account_id: r.str(12)?,
            branch_id: r.str(4)?,
            extend: ExtendFields::default(),
        };
        t.read_trade_extend(&mut r)?;
        Ok(t)
    }

    /// 按消息类型读成交报告扩展字段（与 write_trade_extend 对称）。
    fn read_trade_extend(&mut self, r: &mut BodyReader) -> io::Result<()> {
        // 扩展字段“容忍缺失”：剩余长度不足时跳过（对端只发公共字段的容错）
        match self.msg_type {
            msg_type::EXEC_RPT_CASH_TRADE
            | msg_type::EXEC_RPT_BLOCK_TRADE
            | msg_type::EXEC_RPT_AFTER_HOURS_TRADE => {
                if r.remaining() >= 1 {
                    self.extend.cash_margin = r.ch()?;
                }
            }
            msg_type::EXEC_RPT_BOND_REPO_TRADE => {
                if r.remaining() >= 4 {
                    self.extend.maturity_date = r.u32()?;
                }
            }
            msg_type::EXEC_RPT_OPTION_TRADE => {
                if r.remaining() >= 25 {
                    self.extend.position_effect = r.ch()?;
                    self.extend.covered_or_uncovered = r.ch()?;
                    self.extend.contract_account_code = r.str(6)?;
                    self.extend.secondary_order_id = r.str(16)?;
                }
            }
            msg_type::EXEC_RPT_AGREEMENT_TRADE => {
                if r.remaining() >= 9 {
                    self.extend.confirm_id = r.str(8)?;
                    self.extend.cash_margin = r.ch()?;
                }
            }
            msg_type::EXEC_RPT_SEC_LENDING_TRADE => {
                if r.remaining() >= 9 {
                    self.extend.expiration_days = r.u16()?;
                    self.extend.expiration_type = r.ch()?;
                    self.extend.maturity_date = r.u32()?;
                    self.extend.share_property = r.str(2)?;
                }
            }
            msg_type::EXEC_RPT_BOND_CASH_TRADE => {
                if r.remaining() >= 150 {
                    self.extend.cash_margin = r.ch()?;
                    self.extend.settl_type = r.u16()?;
                    self.extend.settl_period = r.ch()?;
                    self.extend.counterparty_member_id = r.str(6)?;
                    self.extend.counterparty_investor_type = r.str(2)?;
                    self.extend.counterparty_investor_id = r.str(10)?;
                    self.extend.counterparty_investor_name = r.str(120)?;
                    self.extend.counterparty_trader_code = r.str(8)?;
                }
            }
            msg_type::EXEC_RPT_BOND_BID_TRADE => {
                if r.remaining() >= 474 {
                    self.extend.member_id = r.str(6)?;
                    self.extend.investor_type = r.str(2)?;
                    self.extend.investor_id = r.str(10)?;
                    self.extend.investor_name = r.str(120)?;
                    self.extend.trader_code = r.str(8)?;
                    self.extend.counterparty_member_id = r.str(6)?;
                    self.extend.counterparty_investor_type = r.str(2)?;
                    self.extend.counterparty_investor_id = r.str(10)?;
                    self.extend.counterparty_investor_name = r.str(120)?;
                    self.extend.counterparty_trader_code = r.str(8)?;
                    self.extend.secondary_order_id = r.str(16)?;
                    self.extend.bid_trans_type = r.u16()?;
                    self.extend.bid_exec_inst_type = r.u16()?;
                    self.extend.settl_type = r.u16()?;
                    self.extend.settl_period = r.ch()?;
                    self.extend.cash_margin = r.ch()?;
                    self.extend.memo = r.str(160)?;
                }
            }
           // 其余消息类型（含 200315/206315）：无扩展字段
            _ => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 消息类型中文名与报文逐字段解析（供捕获展示/持久化使用）
// ---------------------------------------------------------------------------

/// 消息类型中文名（报文解析展示与日志使用）。
/// 命名带协议英文名，与 msg_type 常量一一对应。
pub fn msg_type_name(mt: u32) -> &'static str {
    match mt {
        // ---- 会话层 ----
        msg_type::LOGON => "登录 Logon",
        msg_type::LOGOUT => "注销 Logout",
        msg_type::HEARTBEAT => "心跳 Heartbeat",
        msg_type::BUSINESS_REJECT => "业务拒绝 BusinessReject",
        msg_type::REPORT_SYNC => "回报同步 ReportSync",
        msg_type::PLATFORM_STATE => "平台状态 PlatformState",
        msg_type::REPORT_FINISHED => "回报结束 ReportFinished",
        msg_type::PLATFORM_INFO => "平台信息 PlatformInfo",
        // ---- 4.5.1 新订单 ----
        msg_type::NEW_ORDER_CASH => "新订单 NewOrderCash(100101)",
        msg_type::NEW_ORDER_BOND_REPO => "新订单 NewOrderBondRepo(100201)",
        msg_type::NEW_ORDER_BOND_DIST => "新订单 NewOrderBondDist(100301)",
        msg_type::NEW_ORDER_OPTION_AUCTION => "新订单 NewOrderOptionAuction(100401)",
        msg_type::NEW_ORDER_AGREEMENT_TRADE => "新订单 NewOrderAgreementTrade(100501)",
        msg_type::NEW_ORDER_BLOCK_TRADE => "新订单 NewOrderBlockTrade(100601)",
        msg_type::NEW_ORDER_SEC_LENDING => "新订单 NewOrderSecLending(100701)",
        msg_type::NEW_ORDER_ETF_SUB_RED => "新订单 NewOrderEtfSubRed(101201)",
        msg_type::NEW_ORDER_ISSUE => "新订单 NewOrderIssue(101301)",
        msg_type::NEW_ORDER_RIGHTS => "新订单 NewOrderRights(101401)",
        msg_type::NEW_ORDER_BOND_CONVERT => "新订单 NewOrderBondConvert(101501)",
        msg_type::NEW_ORDER_OPTION_EXERCISE => "新订单 NewOrderOptionExercise(101601)",
        msg_type::NEW_ORDER_FUND_SUB_RED => "新订单 NewOrderFundSubRed(101701)",
        msg_type::NEW_ORDER_TENDER_OFFER => "新订单 NewOrderTenderOffer(101801)",
        msg_type::NEW_ORDER_REPO_PLEDGE => "新订单 NewOrderRepoPledge(101901)",
        msg_type::NEW_ORDER_GOLD_ETF => "新订单 NewOrderGoldEtf(102201)",
        msg_type::NEW_ORDER_WARRANT => "新订单 NewOrderWarrant(102301)",
        msg_type::NEW_ORDER_DISPOSAL => "新订单 NewOrderDisposal(102701)",
        msg_type::NEW_ORDER_LEND_RETURN => "新订单 NewOrderLendReturn(102801)",
        msg_type::NEW_ORDER_DEDUCTION => "新订单 NewOrderDeduction(102901)",
        msg_type::NEW_ORDER_SPLIT_MERGE => "新订单 NewOrderSplitMerge(103101)",
        msg_type::NEW_ORDER_3P_REPO => "新订单 NewOrder3pRepo(103301)",
        msg_type::NEW_ORDER_OPTION_CONVERT => "新订单 NewOrderOptionConvert(103501)",
        msg_type::NEW_ORDER_AFTER_HOURS => "新订单 NewOrderAfterHours(103701)",
        msg_type::NEW_ORDER_BOND_CASH => "新订单 NewOrderBondCash(104101)",
        msg_type::NEW_ORDER_BOND_BID => "新订单 NewOrderBondBid(104128)",
        msg_type::NEW_ORDER_INTERBANK_ETF => "新订单 NewOrderInterbankEtf(104701)",
        msg_type::NEW_ORDER_HK_CONNECT => "新订单 NewOrderHkConnect(106301)",
        // ---- 4.5.2 / 4.5.3 ----
        msg_type::ORDER_CANCEL_REQUEST => "撤单请求 OrderCancelRequest(190007)",
        msg_type::CANCEL_REJECT => "撤单失败响应 CancelReject(290008)",
        // ---- 4.5.4 确认执行报告 ----
        msg_type::EXEC_RPT_CASH_ACK => "订单执行报告 ExecRptCashAck(200102)",
        msg_type::EXEC_RPT_BOND_REPO_ACK => "订单执行报告 ExecRptBondRepoAck(200202)",
        msg_type::EXEC_RPT_BOND_DIST_ACK => "订单执行报告 ExecRptBondDistAck(200302)",
        msg_type::EXEC_RPT_OPTION_ACK => "订单执行报告 ExecRptOptionAck(200402)",
        msg_type::EXEC_RPT_AGREEMENT_ACK => "订单执行报告 ExecRptAgreementAck(200502)",
        msg_type::EXEC_RPT_BLOCK_ACK => "订单执行报告 ExecRptBlockAck(200602)",
        msg_type::EXEC_RPT_SEC_LENDING_ACK => "订单执行报告 ExecRptSecLendingAck(200702)",
        msg_type::EXEC_RPT_ETF_ACK => "订单执行报告 ExecRptEtfAck(201202)",
        msg_type::EXEC_RPT_ISSUE_ACK => "订单执行报告 ExecRptIssueAck(201302)",
        msg_type::EXEC_RPT_RIGHTS_ACK => "订单执行报告 ExecRptRightsAck(201402)",
        msg_type::EXEC_RPT_BOND_CONVERT_ACK => "订单执行报告 ExecRptBondConvertAck(201502)",
        msg_type::EXEC_RPT_OPTION_EXERCISE_ACK => "订单执行报告 ExecRptOptionExerciseAck(201602)",
        msg_type::EXEC_RPT_FUND_ACK => "订单执行报告 ExecRptFundAck(201702)",
        msg_type::EXEC_RPT_TENDER_ACK => "订单执行报告 ExecRptTenderAck(201802)",
        msg_type::EXEC_RPT_REPO_PLEDGE_ACK => "订单执行报告 ExecRptRepoPledgeAck(201902)",
        msg_type::EXEC_RPT_GOLD_ETF_ACK => "订单执行报告 ExecRptGoldEtfAck(202202)",
        msg_type::EXEC_RPT_WARRANT_ACK => "订单执行报告 ExecRptWarrantAck(202302)",
        msg_type::EXEC_RPT_DISPOSAL_ACK => "订单执行报告 ExecRptDisposalAck(202702)",
        msg_type::EXEC_RPT_LEND_RETURN_ACK => "订单执行报告 ExecRptLendReturnAck(202802)",
        msg_type::EXEC_RPT_DEDUCTION_ACK => "订单执行报告 ExecRptDeductionAck(202902)",
        msg_type::EXEC_RPT_SPLIT_ACK => "订单执行报告 ExecRptSplitAck(203102)",
        msg_type::EXEC_RPT_3P_REPO_ACK => "订单执行报告 ExecRpt3pRepoAck(203302)",
        msg_type::EXEC_RPT_OPTION_CONVERT_ACK => "订单执行报告 ExecRptOptionConvertAck(203502)",
        msg_type::EXEC_RPT_AFTER_HOURS_ACK => "订单执行报告 ExecRptAfterHoursAck(203702)",
        msg_type::EXEC_RPT_BOND_CASH_ACK => "订单执行报告 ExecRptBondCashAck(204102)",
        msg_type::EXEC_RPT_BOND_BID_ACK => "订单执行报告 ExecRptBondBidAck(204129)",
        msg_type::EXEC_RPT_INTERBANK_ETF_ACK => "订单执行报告 ExecRptInterbankEtfAck(204702)",
        msg_type::EXEC_RPT_HK_ACK => "订单执行报告 ExecRptHkAck(206302)",
        // ---- 4.5.5 成交执行报告 ----
        msg_type::EXEC_RPT_CASH_TRADE => "成交执行报告 ExecRptCashTrade(200115)",
        msg_type::EXEC_RPT_BOND_REPO_TRADE => "成交执行报告 ExecRptBondRepoTrade(200215)",
        msg_type::EXEC_RPT_BOND_DIST_TRADE => "成交执行报告 ExecRptBondDistTrade(200315)",
        msg_type::EXEC_RPT_OPTION_TRADE => "成交执行报告 ExecRptOptionTrade(200415)",
        msg_type::EXEC_RPT_AGREEMENT_TRADE => "成交执行报告 ExecRptAgreementTrade(200515)",
        msg_type::EXEC_RPT_BLOCK_TRADE => "成交执行报告 ExecRptBlockTrade(200615)",
        msg_type::EXEC_RPT_SEC_LENDING_TRADE => "成交执行报告 ExecRptSecLendingTrade(200715)",
        msg_type::EXEC_RPT_AFTER_HOURS_TRADE => "成交执行报告 ExecRptAfterHoursTrade(203715)",
        msg_type::EXEC_RPT_BOND_CASH_TRADE => "成交执行报告 ExecRptBondCashTrade(204115)",
        msg_type::EXEC_RPT_BOND_BID_TRADE => "成交执行报告 ExecRptBondBidTrade(204130)",
        msg_type::EXEC_RPT_HK_TRADE => "成交执行报告 ExecRptHkTrade(206315)",
        _ => "未知消息",
    }
}

/// 按交易所规范字段名解析一条报文，供捕获展示/持久化使用。
/// 第一条固定为 MsgType（中文名 + 消息号），其余按消息体字段顺序逐项读取。
/// 未知消息类型或字段读取失败时返回空列表（调用方只展示原始报文）。
pub fn describe_fields(mt: u32, body: &[u8]) -> Vec<ParsedField> {
    describe_body(mt, body).unwrap_or_default()
}

/// 拼一个解析字段（名称 + 可读值；数字/字符等类型自动转字符串）
fn f(name: impl Into<String>, value: impl ToString) -> ParsedField {
    ParsedField { name: name.into(), value: value.to_string() }
}

/// 深交所时间戳（YYYYMMDDHHMMSSsss → "2026-07-31 09:30:00.123"）
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

/// 放大整数 → 自然单位字符串（去尾零），如 123400/10000 → "12.34"
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
        b'D' => "D (申购)".into(),
        b'E' => "E (赎回)".into(),
        b'F' => "F (出借)".into(),
        b'G' => "G (借入)".into(),
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

/// 应用标识 → 业务名（表 3-3），未知的按原样显示
fn appl_id_label(s: &str) -> String {
    match s.trim_end() {
        "010" => "010 (现货集中竞价)".into(),
        "020" => "020 (债券通用质押式回购)".into(),
        "030" => "030 (债券分销)".into(),
        "040" => "040 (期权集中竞价)".into(),
        "051" => "051 (协议交易定价)".into(),
        "052" => "052 (协议交易点击成交)".into(),
        "060" => "060 (盘后定价大宗-收盘价)".into(),
        "061" => "061 (盘后定价大宗-VWAP)".into(),
        "070" => "070 (转融通证券出借)".into(),
        "120" => "120 (ETF实时申赎)".into(),
        "130" => "130 (网上发行-发行增发)".into(),
        "131" => "131 (网上发行-向原持有人配售)".into(),
        "132" => "132 (网上发行-公开发售)".into(),
        "140" => "140 (配股认购)".into(),
        "150" => "150 (债券转股)".into(),
        "151" => "151 (债券回售)".into(),
        "152" => "152 (债券回售撤销)".into(),
        "160" => "160 (期权行权)".into(),
        "170" => "170 (开放式基金申赎)".into(),
        "180" => "180 (要约收购-预受要约)".into(),
        "181" => "181 (要约收购-解除预受)".into(),
        "190" => "190 (回购质押)".into(),
        "191" => "191 (回购解押)".into(),
        "220" => "220 (黄金ETF实物申赎)".into(),
        "230" => "230 (权证行权)".into(),
        "270" => "270 (转处置-扣券)".into(),
        "271" => "271 (转处置-还券)".into(),
        "280" => "280 (垫券)".into(),
        "281" => "281 (还券)".into(),
        "290" => "290 (待清偿扣划-客户)".into(),
        "291" => "291 (待清偿扣划-自营)".into(),
        "310" => "310 (分级基金实时分拆)".into(),
        "311" => "311 (分级基金实时合并)".into(),
        "330" => "330 (三方回购入库)".into(),
        "331" => "331 (三方回购出库)".into(),
        "350" => "350 (期权普通转备兑)".into(),
        "351" => "351 (期权备兑转普通)".into(),
        "370" => "370 (盘后定价交易)".into(),
        "410" => "410 (债券现券匹配成交)".into(),
        "412" => "412 (债券现券点击报价)".into(),
        "415" => "415 (债券现券询价报价)".into(),
        "417" => "417 (债券现券竞买成交)".into(),
        "470" => "470 (跨银行间ETF实物申赎)".into(),
        "630" => "630 (港股通)".into(),
        other => other.to_string(),
    }
}

/// 4.5.4 确认执行报告消息类型判定（describe_body 分派用）
pub fn is_exec_rpt_ack(mt: u32) -> bool {
    matches!(
        mt,
        msg_type::EXEC_RPT_CASH_ACK
            | msg_type::EXEC_RPT_BOND_REPO_ACK
            | msg_type::EXEC_RPT_BOND_DIST_ACK
            | msg_type::EXEC_RPT_OPTION_ACK
            | msg_type::EXEC_RPT_AGREEMENT_ACK
            | msg_type::EXEC_RPT_BLOCK_ACK
            | msg_type::EXEC_RPT_SEC_LENDING_ACK
            | msg_type::EXEC_RPT_ETF_ACK
            | msg_type::EXEC_RPT_ISSUE_ACK
            | msg_type::EXEC_RPT_RIGHTS_ACK
            | msg_type::EXEC_RPT_BOND_CONVERT_ACK
            | msg_type::EXEC_RPT_OPTION_EXERCISE_ACK
            | msg_type::EXEC_RPT_FUND_ACK
            | msg_type::EXEC_RPT_TENDER_ACK
            | msg_type::EXEC_RPT_REPO_PLEDGE_ACK
            | msg_type::EXEC_RPT_GOLD_ETF_ACK
            | msg_type::EXEC_RPT_WARRANT_ACK
            | msg_type::EXEC_RPT_DISPOSAL_ACK
            | msg_type::EXEC_RPT_LEND_RETURN_ACK
            | msg_type::EXEC_RPT_DEDUCTION_ACK
            | msg_type::EXEC_RPT_SPLIT_ACK
            | msg_type::EXEC_RPT_3P_REPO_ACK
            | msg_type::EXEC_RPT_OPTION_CONVERT_ACK
            | msg_type::EXEC_RPT_AFTER_HOURS_ACK
            | msg_type::EXEC_RPT_BOND_CASH_ACK
            | msg_type::EXEC_RPT_BOND_BID_ACK
            | msg_type::EXEC_RPT_INTERBANK_ETF_ACK
            | msg_type::EXEC_RPT_HK_ACK
    )
}

/// 4.5.5 成交执行报告消息类型判定（describe_body 分派用）
pub fn is_exec_rpt_trade(mt: u32) -> bool {
    matches!(
        mt,
        msg_type::EXEC_RPT_CASH_TRADE
            | msg_type::EXEC_RPT_BOND_REPO_TRADE
            | msg_type::EXEC_RPT_BOND_DIST_TRADE
            | msg_type::EXEC_RPT_OPTION_TRADE
            | msg_type::EXEC_RPT_AGREEMENT_TRADE
            | msg_type::EXEC_RPT_BLOCK_TRADE
            | msg_type::EXEC_RPT_SEC_LENDING_TRADE
            | msg_type::EXEC_RPT_AFTER_HOURS_TRADE
            | msg_type::EXEC_RPT_BOND_CASH_TRADE
            | msg_type::EXEC_RPT_BOND_BID_TRADE
            | msg_type::EXEC_RPT_HK_TRADE
    )
}

/// 新订单公共字段 → 字段列表（describe_body 用）
fn describe_new_order_common(o: &NewOrder) -> Vec<ParsedField> {
    vec![
        f("ApplID", appl_id_label(&o.common.appl_id)),
        f("SubmittingPBUId", &o.common.submitting_pbu_id),
        f("SecurityID", &o.common.security_id),
        f("SecurityIDSource", &o.common.security_id_source),
        f("OwnerType", o.common.owner_type),
        f("ClearingFirm", &o.common.clearing_firm),
        f("TransactTime", fmt_time(o.common.transact_time)),
        f("UserInfo", &o.common.user_info),
        f("ClOrdID", &o.common.cl_ord_id),
        f("AccountID", &o.common.account_id),
        f("BranchID", &o.common.branch_id),
        f("OrderRestrictions", &o.common.order_restrictions),
        f("Side", side_label(o.common.side)),
        f("OrdType", ord_type_label(o.common.ord_type)),
        f("OrderQty", format!("{} 股", fmt_scaled(o.common.order_qty, 100))),
        f("Price", format!("{} 元", fmt_scaled(o.common.price, 10000))),
    ]
}

/// 新订单扩展字段 → 字段列表（按 4.5.1.x 顺序，仅输出本业务相关字段）
fn describe_new_order_extend(mt: u32, o: &NewOrder) -> Vec<ParsedField> {
    let e = &o.extend;
    let mut out = Vec::new();
    match mt {
        // 4.5.1.1 现货 / 4.5.1.17(1) 债券现券匹配：5 字段
        msg_type::NEW_ORDER_CASH | msg_type::NEW_ORDER_BOND_CASH => {
            out.push(f("StopPx", format!("{} 元", fmt_scaled(e.stop_px, 10000))));
            out.push(f("MinQty", format!("{} 股", fmt_scaled(e.min_qty, 100))));
            out.push(f("MaxPriceLevels", e.max_price_levels));
            out.push(f("TimeInForce", e.time_in_force));
            out.push(f("CashMargin", e.cash_margin));
        }
        // 4.5.1.2 债券回购：4 字段
        msg_type::NEW_ORDER_BOND_REPO => {
            out.push(f("StopPx", format!("{} 元", fmt_scaled(e.stop_px, 10000))));
            out.push(f("MinQty", format!("{} 股", fmt_scaled(e.min_qty, 100))));
            out.push(f("MaxPriceLevels", e.max_price_levels));
            out.push(f("TimeInForce", e.time_in_force));
        }
        // 4.5.1.3 期权集中竞价：8 字段
        msg_type::NEW_ORDER_OPTION_AUCTION => {
            out.push(f("StopPx", format!("{} 元", fmt_scaled(e.stop_px, 10000))));
            out.push(f("MinQty", format!("{} 股", fmt_scaled(e.min_qty, 100))));
            out.push(f("MaxPriceLevels", e.max_price_levels));
            out.push(f("TimeInForce", e.time_in_force));
            out.push(f("PositionEffect", e.position_effect as char));
            out.push(f("CoveredOrUncovered", e.covered_or_uncovered));
            out.push(f("ContractAccountCode", &e.contract_account_code));
            out.push(f("SecondaryOrderID", &e.secondary_order_id));
        }
        // 4.5.1.4 协议交易
        msg_type::NEW_ORDER_AGREEMENT_TRADE => {
            out.push(f("ConfirmID", &e.confirm_id));
            out.push(f("CashMargin", e.cash_margin));
        }
        // 4.5.1.5 盘后定价大宗 / 4.5.1.16 盘后定价交易
        msg_type::NEW_ORDER_BLOCK_TRADE | msg_type::NEW_ORDER_AFTER_HOURS => {
            out.push(f("CashMargin", e.cash_margin));
        }
        // 4.5.1.6 转融通出借
        msg_type::NEW_ORDER_SEC_LENDING => {
            out.push(f("ExpirationDays", e.expiration_days));
            out.push(f("ExpirationType", e.expiration_type));
            out.push(f("ShareProperty", &e.share_property));
        }
        // 4.5.1.19 ETF 实时申赎：NoAccounts + 重复组
        msg_type::NEW_ORDER_ETF_SUB_RED => {
            out.push(f("NoAccounts", e.no_accounts));
            for (i, a) in e.other_accounts.iter().enumerate() {
                out.push(f(format!("Account[{0}].MarketID", i + 1), &a.market_id));
                out.push(f(format!("Account[{0}].PBUID", i + 1), &a.pbu_id));
                out.push(f(format!("Account[{0}].AccountID", i + 1), &a.account_id));
            }
        }
        // 4.5.1.7 债券转股回售
        msg_type::NEW_ORDER_BOND_CONVERT => {
            out.push(f("ShareProperty", &e.share_property));
        }
        // 4.5.1.8 期权行权 / 4.5.1.15 备兑互转
        msg_type::NEW_ORDER_OPTION_EXERCISE | msg_type::NEW_ORDER_OPTION_CONVERT => {
            out.push(f("ContractAccountCode", &e.contract_account_code));
        }
        // 4.5.1.9 基金申赎
        msg_type::NEW_ORDER_FUND_SUB_RED => {
            out.push(f("CashOrderQty", format!("{} 元", fmt_scaled(e.cash_order_qty, 10000))));
        }
        // 4.5.1.10 要约收购
        msg_type::NEW_ORDER_TENDER_OFFER => {
            out.push(f("Tenderer", &e.tenderer));
        }
        // 4.5.1.11 转处置
        msg_type::NEW_ORDER_DISPOSAL => {
            out.push(f("DisposalPBU", &e.disposal_pbu));
            out.push(f("DisposalAccountID", &e.disposal_account_id));
        }
        // 4.5.1.12 垫券还券
        msg_type::NEW_ORDER_LEND_RETURN => {
            out.push(f("LenderPBU", &e.lender_pbu));
            out.push(f("LenderAccountID", &e.lender_account_id));
        }
        // 4.5.1.13 待清偿扣划
        msg_type::NEW_ORDER_DEDUCTION => {
            out.push(f("DeductionPBU", &e.deduction_pbu));
            out.push(f("DeductionAccountID", &e.deduction_account_id));
        }
        // 4.5.1.17(2) 债券竞买：17 字段
        msg_type::NEW_ORDER_BOND_BID => {
            out.push(f("MemberID", &e.member_id));
            out.push(f("InvestorType", &e.investor_type));
            out.push(f("InvestorID", &e.investor_id));
            out.push(f("InvestorName", &e.investor_name));
            out.push(f("TraderCode", &e.trader_code));
            out.push(f("SecondaryOrderID", &e.secondary_order_id));
            out.push(f("BidTransType", e.bid_trans_type));
            out.push(f("BidExecInstType", e.bid_exec_inst_type));
            out.push(f("LowLimitPrice", format!("{} 元", fmt_scaled(e.low_limit_price, 10000))));
            out.push(f("HighLimitPrice", format!("{} 元", fmt_scaled(e.high_limit_price, 10000))));
            out.push(f("MinQty", format!("{} 股", fmt_scaled(e.min_qty, 100))));
            out.push(f("TradeDate", e.trade_date));
            out.push(f("SettlType", e.settl_type));
            out.push(f("SettlPeriod", e.settl_period));
            out.push(f("PreTradeAnonymity", e.pre_trade_anonymity));
            out.push(f("CashMargin", e.cash_margin));
            out.push(f("Memo", &e.memo));
        }
        // 4.5.1.18 跨银行间 ETF
        msg_type::NEW_ORDER_INTERBANK_ETF => {
            out.push(f("SecondaryOrderID", &e.secondary_order_id));
        }
        // 4.5.1.14 港股通：5 字段
        msg_type::NEW_ORDER_HK_CONNECT => {
            out.push(f("StopPx", format!("{} 元", fmt_scaled(e.stop_px, 10000))));
            out.push(f("MinQty", format!("{} 股", fmt_scaled(e.min_qty, 100))));
            out.push(f("MaxPriceLevels", e.max_price_levels));
            out.push(f("TimeInForce", e.time_in_force));
            out.push(f("LotType", e.lot_type));
        }
        // 其余业务（100301/101301/101401/101901/102201/102301/103101/103301）：无扩展字段
        _ => {}
    }
    out
}

/// 确认执行报告公共字段 → 字段列表（describe_body 用）
fn describe_ack_common(a: &ExecRptAck) -> Vec<ParsedField> {
    vec![
        f("PartitionNo", a.partition_no),
        f("ReportIndex", a.report_index),
        f("ApplID", appl_id_label(&a.appl_id)),
        f("ReportingPBUId", &a.reporting_pbu_id),
        f("SubmittingPBUId", &a.submitting_pbu_id),
        f("SecurityID", &a.security_id),
        f("SecurityIDSource", &a.security_id_source),
        f("OwnerType", a.owner_type),
        f("ClearingFirm", &a.clearing_firm),
        f("TransactTime", fmt_time(a.transact_time)),
        f("UserInfo", &a.user_info),
        f("OrderID", &a.order_id),
        f("ClOrdID", &a.cl_ord_id),
        f("OrigClOrdID", &a.orig_cl_ord_id),
        f("ExecID", &a.exec_id),
        f("ExecType", exec_type_label(a.exec_type)),
        f("OrdStatus", ord_status_label(a.ord_status)),
        f("OrdRejReason", a.ord_rej_reason),
        f("LeavesQty", format!("{} 股", fmt_scaled(a.leaves_qty, 100))),
        f("CumQty", format!("{} 股", fmt_scaled(a.cum_qty, 100))),
        f("Side", side_label(a.side)),
        f("OrdType", ord_type_label(a.ord_type)),
        f("OrderQty", format!("{} 股", fmt_scaled(a.order_qty, 100))),
        f("Price", format!("{} 元", fmt_scaled(a.price, 10000))),
        f("AccountID", &a.account_id),
        f("BranchID", &a.branch_id),
        f("OrderRestrictions", &a.order_restrictions),
    ]
}

/// 确认执行报告扩展字段 → 字段列表（按 4.5.4.x 顺序，仅输出本业务相关字段）
fn describe_ack_extend(mt: u32, a: &ExecRptAck) -> Vec<ParsedField> {
    let e = &a.extend;
    let mut out = Vec::new();
    match mt {
        // 4.5.4.1 现货 / 4.5.4.19(1) 债券现券匹配：5 字段
        msg_type::EXEC_RPT_CASH_ACK | msg_type::EXEC_RPT_BOND_CASH_ACK => {
            out.push(f("StopPx", format!("{} 元", fmt_scaled(e.stop_px, 10000))));
            out.push(f("MinQty", format!("{} 股", fmt_scaled(e.min_qty, 100))));
            out.push(f("MaxPriceLevels", e.max_price_levels));
            out.push(f("TimeInForce", e.time_in_force));
            out.push(f("CashMargin", e.cash_margin));
        }
        // 4.5.4.2 债券回购：4 字段
        msg_type::EXEC_RPT_BOND_REPO_ACK => {
            out.push(f("StopPx", format!("{} 元", fmt_scaled(e.stop_px, 10000))));
            out.push(f("MinQty", format!("{} 股", fmt_scaled(e.min_qty, 100))));
            out.push(f("MaxPriceLevels", e.max_price_levels));
            out.push(f("TimeInForce", e.time_in_force));
        }
        // 4.5.4.3 期权集中竞价：8 字段
        msg_type::EXEC_RPT_OPTION_ACK => {
            out.push(f("StopPx", format!("{} 元", fmt_scaled(e.stop_px, 10000))));
            out.push(f("MinQty", format!("{} 股", fmt_scaled(e.min_qty, 100))));
            out.push(f("MaxPriceLevels", e.max_price_levels));
            out.push(f("TimeInForce", e.time_in_force));
            out.push(f("PositionEffect", e.position_effect as char));
            out.push(f("CoveredOrUncovered", e.covered_or_uncovered));
            out.push(f("ContractAccountCode", &e.contract_account_code));
            out.push(f("SecondaryOrderID", &e.secondary_order_id));
        }
        // 4.5.4.4 协议交易
        msg_type::EXEC_RPT_AGREEMENT_ACK => {
            out.push(f("ConfirmID", &e.confirm_id));
            out.push(f("CashMargin", e.cash_margin));
        }
        // 4.5.4.5 盘后定价大宗 / 4.5.4.18 盘后定价交易
        msg_type::EXEC_RPT_BLOCK_ACK | msg_type::EXEC_RPT_AFTER_HOURS_ACK => {
            out.push(f("CashMargin", e.cash_margin));
        }
        // 4.5.4.6 转融通出借
        msg_type::EXEC_RPT_SEC_LENDING_ACK => {
            out.push(f("ExpirationDays", e.expiration_days));
            out.push(f("ExpirationType", e.expiration_type));
            out.push(f("ShareProperty", &e.share_property));
        }
        // 4.5.4.7 ETF 实时申赎：不足成份股 + 成份股重复组 + 账户重复组
        msg_type::EXEC_RPT_ETF_ACK => {
            out.push(f("InsufficientSecurityID", &e.insufficient_security_id));
            out.push(f("NoSecurity", e.no_security));
            for (i, u) in e.underlying_securities.iter().enumerate() {
                out.push(f(format!("Security[{0}].UnderlyingSecurityID", i + 1), &u.security_id));
                out.push(f(format!("Security[{0}].SecurityIDSource", i + 1), &u.security_id_source));
                out.push(f(format!("Security[{0}].DeliveryQty", i + 1), fmt_scaled(u.delivery_qty, 100)));
                out.push(f(format!("Security[{0}].SubstCash", i + 1), fmt_scaled(u.subst_cash, 10000)));
            }
            out.push(f("NoAccounts", e.no_accounts));
            for (i, a) in e.other_accounts.iter().enumerate() {
                out.push(f(format!("Account[{0}].MarketID", i + 1), &a.market_id));
                out.push(f(format!("Account[{0}].PBUID", i + 1), &a.pbu_id));
                out.push(f(format!("Account[{0}].AccountID", i + 1), &a.account_id));
            }
        }
        // 4.5.4.8 债券转股回售
        msg_type::EXEC_RPT_BOND_CONVERT_ACK => {
            out.push(f("ShareProperty", &e.share_property));
        }
        // 4.5.4.9 期权行权 / 4.5.4.17 备兑互转
        msg_type::EXEC_RPT_OPTION_EXERCISE_ACK | msg_type::EXEC_RPT_OPTION_CONVERT_ACK => {
            out.push(f("ContractAccountCode", &e.contract_account_code));
        }
        // 4.5.4.10 基金申赎
        msg_type::EXEC_RPT_FUND_ACK => {
            out.push(f("CashOrderQty", format!("{} 元", fmt_scaled(e.cash_order_qty, 10000))));
        }
        // 4.5.4.11 要约收购
        msg_type::EXEC_RPT_TENDER_ACK => {
            out.push(f("Tenderer", &e.tenderer));
        }
        // 4.5.4.12 转处置
        msg_type::EXEC_RPT_DISPOSAL_ACK => {
            out.push(f("DisposalPBU", &e.disposal_pbu));
            out.push(f("DisposalAccountID", &e.disposal_account_id));
        }
        // 4.5.4.13 垫券还券
        msg_type::EXEC_RPT_LEND_RETURN_ACK => {
            out.push(f("LenderPBU", &e.lender_pbu));
            out.push(f("LenderAccountID", &e.lender_account_id));
        }
        // 4.5.4.14 待清偿扣划
        msg_type::EXEC_RPT_DEDUCTION_ACK => {
            out.push(f("DeductionPBU", &e.deduction_pbu));
            out.push(f("DeductionAccountID", &e.deduction_account_id));
        }
        // 4.5.4.15 分级基金分拆合并：不足子基金 + 子基金重复组
        msg_type::EXEC_RPT_SPLIT_ACK => {
            out.push(f("InsufficientSecurityID", &e.insufficient_security_id));
            out.push(f("NoSecurity", e.no_security));
            for (i, u) in e.underlying_securities.iter().enumerate() {
                out.push(f(format!("Security[{0}].UnderlyingSecurityID", i + 1), &u.security_id));
                out.push(f(format!("Security[{0}].SecurityIDSource", i + 1), &u.security_id_source));
                out.push(f(format!("Security[{0}].DeliveryQty", i + 1), fmt_scaled(u.delivery_qty, 100)));
            }
        }
        // 4.5.4.19(2) 债券竞买：17 字段
        msg_type::EXEC_RPT_BOND_BID_ACK => {
            out.push(f("MemberID", &e.member_id));
            out.push(f("InvestorType", &e.investor_type));
            out.push(f("InvestorID", &e.investor_id));
            out.push(f("InvestorName", &e.investor_name));
            out.push(f("TraderCode", &e.trader_code));
            out.push(f("SecondaryOrderID", &e.secondary_order_id));
            out.push(f("BidTransType", e.bid_trans_type));
            out.push(f("BidExecInstType", e.bid_exec_inst_type));
            out.push(f("LowLimitPrice", format!("{} 元", fmt_scaled(e.low_limit_price, 10000))));
            out.push(f("HighLimitPrice", format!("{} 元", fmt_scaled(e.high_limit_price, 10000))));
            out.push(f("MinQty", format!("{} 股", fmt_scaled(e.min_qty, 100))));
            out.push(f("TradeDate", e.trade_date));
            out.push(f("SettlType", e.settl_type));
            out.push(f("SettlPeriod", e.settl_period));
            out.push(f("PreTradeAnonymity", e.pre_trade_anonymity));
            out.push(f("CashMargin", e.cash_margin));
            out.push(f("Memo", &e.memo));
        }
        // 4.5.4.20 跨银行间 ETF
        msg_type::EXEC_RPT_INTERBANK_ETF_ACK => {
            out.push(f("SecondaryOrderID", &e.secondary_order_id));
        }
        // 4.5.4.16 港股通：8 字段（RejectText 在最前，IMCRejectText 变长）
        msg_type::EXEC_RPT_HK_ACK => {
            out.push(f("RejectText", &e.reject_text));
            out.push(f("StopPx", format!("{} 元", fmt_scaled(e.stop_px, 10000))));
            out.push(f("MinQty", format!("{} 股", fmt_scaled(e.min_qty, 100))));
            out.push(f("MaxPriceLevels", e.max_price_levels));
            out.push(f("TimeInForce", e.time_in_force));
            out.push(f("LotType", e.lot_type));
            out.push(f("IMCRejectTextLen", e.imc_reject_text_len));
            out.push(f("IMCRejectText", &e.imc_reject_text));
        }
        // 其余业务（200302/201302/201402/201902/202202/202302/203302）：无扩展字段
        _ => {}
    }
    out
}

/// 成交执行报告公共字段 → 字段列表（describe_body 用）
fn describe_trade_common(t: &ExecRptTrade) -> Vec<ParsedField> {
    vec![
        f("PartitionNo", t.partition_no),
        f("ReportIndex", t.report_index),
        f("ApplID", appl_id_label(&t.appl_id)),
        f("ReportingPBUId", &t.reporting_pbu_id),
        f("SubmittingPBUId", &t.submitting_pbu_id),
        f("SecurityID", &t.security_id),
        f("SecurityIDSource", &t.security_id_source),
        f("OwnerType", t.owner_type),
        f("ClearingFirm", &t.clearing_firm),
        f("TransactTime", fmt_time(t.transact_time)),
        f("UserInfo", &t.user_info),
        f("OrderID", &t.order_id),
        f("ClOrdID", &t.cl_ord_id),
        f("ExecID", &t.exec_id),
        f("ExecType", exec_type_label(t.exec_type)),
        f("OrdStatus", ord_status_label(t.ord_status)),
        f("LastPx", format!("{} 元", fmt_scaled(t.last_px, 10000))),
        f("LastQty", format!("{} 股", fmt_scaled(t.last_qty, 100))),
        f("LeavesQty", format!("{} 股", fmt_scaled(t.leaves_qty, 100))),
        f("CumQty", format!("{} 股", fmt_scaled(t.cum_qty, 100))),
        f("Side", side_label(t.side)),
        f("AccountID", &t.account_id),
        f("BranchID", &t.branch_id),
    ]
}

/// 成交执行报告扩展字段 → 字段列表（按 4.5.5.x 顺序，仅输出本业务相关字段）
fn describe_trade_extend(mt: u32, t: &ExecRptTrade) -> Vec<ParsedField> {
    let e = &t.extend;
    let mut out = Vec::new();
    match mt {
        // 4.5.5.1 现货 / 4.5.5.5 大宗 / 4.5.5.7 盘后定价：CashMargin
        msg_type::EXEC_RPT_CASH_TRADE
        | msg_type::EXEC_RPT_BLOCK_TRADE
        | msg_type::EXEC_RPT_AFTER_HOURS_TRADE => {
            out.push(f("CashMargin", e.cash_margin));
        }
        // 4.5.5.2 债券回购：MaturityDate
        msg_type::EXEC_RPT_BOND_REPO_TRADE => {
            out.push(f("MaturityDate", e.maturity_date));
        }
        // 4.5.5.3 期权集中竞价：4 字段
        msg_type::EXEC_RPT_OPTION_TRADE => {
            out.push(f("PositionEffect", e.position_effect as char));
            out.push(f("CoveredOrUncovered", e.covered_or_uncovered));
            out.push(f("ContractAccountCode", &e.contract_account_code));
            out.push(f("SecondaryOrderID", &e.secondary_order_id));
        }
        // 4.5.5.4 协议交易
        msg_type::EXEC_RPT_AGREEMENT_TRADE => {
            out.push(f("ConfirmID", &e.confirm_id));
            out.push(f("CashMargin", e.cash_margin));
        }
        // 4.5.5.6 转融通出借：4 字段
        msg_type::EXEC_RPT_SEC_LENDING_TRADE => {
            out.push(f("ExpirationDays", e.expiration_days));
            out.push(f("ExpirationType", e.expiration_type));
            out.push(f("MaturityDate", e.maturity_date));
            out.push(f("ShareProperty", &e.share_property));
        }
        // 4.5.5.8(1) 债券现券匹配成交：CashMargin/Settl/Counterparty
        msg_type::EXEC_RPT_BOND_CASH_TRADE => {
            out.push(f("CashMargin", e.cash_margin));
            out.push(f("SettlType", e.settl_type));
            out.push(f("SettlPeriod", e.settl_period));
            out.push(f("CounterpartyMemberID", &e.counterparty_member_id));
            out.push(f("CounterpartyInvestorType", &e.counterparty_investor_type));
            out.push(f("CounterpartyInvestorID", &e.counterparty_investor_id));
            out.push(f("CounterpartyInvestorName", &e.counterparty_investor_name));
            out.push(f("CounterpartyTraderCode", &e.counterparty_trader_code));
        }
        // 4.5.5.8(2) 债券竞买成交：本方+对手方+17 字段
        msg_type::EXEC_RPT_BOND_BID_TRADE => {
            out.push(f("MemberID", &e.member_id));
            out.push(f("InvestorType", &e.investor_type));
            out.push(f("InvestorID", &e.investor_id));
            out.push(f("InvestorName", &e.investor_name));
            out.push(f("TraderCode", &e.trader_code));
            out.push(f("CounterpartyMemberID", &e.counterparty_member_id));
            out.push(f("CounterpartyInvestorType", &e.counterparty_investor_type));
            out.push(f("CounterpartyInvestorID", &e.counterparty_investor_id));
            out.push(f("CounterpartyInvestorName", &e.counterparty_investor_name));
            out.push(f("CounterpartyTraderCode", &e.counterparty_trader_code));
            out.push(f("SecondaryOrderID", &e.secondary_order_id));
            out.push(f("BidTransType", e.bid_trans_type));
            out.push(f("BidExecInstType", e.bid_exec_inst_type));
            out.push(f("SettlType", e.settl_type));
            out.push(f("SettlPeriod", e.settl_period));
            out.push(f("CashMargin", e.cash_margin));
            out.push(f("Memo", &e.memo));
        }
        // 其余业务（200315/206315）：无扩展字段
        _ => {}
    }
    out
}

/// 实际逐字段读取逻辑：按消息类型分派，读失败（长度不符）即整体放弃。
/// 新订单/确认/成交报告先解码到结构体再转字段列表，字段顺序与规范一致。
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
        // 4.5.1 新订单：解码到 NewOrder 统一结构再输出字段
        m if NewOrder::is_new_order(m) => {
            let o = NewOrder::decode(m, body)?;
            out.extend(describe_new_order_common(&o));
            out.extend(describe_new_order_extend(m, &o));
        }
        // 4.5.4 确认执行报告
        m if is_exec_rpt_ack(m) => {
            let a = ExecRptAck::decode(m, body)?;
            out.extend(describe_ack_common(&a));
            out.extend(describe_ack_extend(m, &a));
        }
        // 4.5.5 成交执行报告
        m if is_exec_rpt_trade(m) => {
            let t = ExecRptTrade::decode(m, body)?;
            out.extend(describe_trade_common(&t));
            out.extend(describe_trade_extend(m, &t));
        }
        // 其他消息类型：不逐字段解析（调用方只展示原始报文）
        _ => {}
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个指定消息类型的新订单报文体（公共字段 + 扩展字段）。
    /// 公共字段值固定，便于断言；扩展字段按各业务测试用例自行追加。
    fn build_order_body(mt: u32, extend: impl FnOnce(&mut BodyWriter)) -> Vec<u8> {
        let mut w = BodyWriter::new();
        let appl = match mt {
            msg_type::NEW_ORDER_CASH => "010",
            msg_type::NEW_ORDER_BOND_REPO => "020",
            msg_type::NEW_ORDER_OPTION_AUCTION => "040",
            msg_type::NEW_ORDER_BOND_BID => "417",
            msg_type::NEW_ORDER_ETF_SUB_RED => "120",
            msg_type::NEW_ORDER_HK_CONNECT => "630",
            _ => "010",
        };
        w.str(appl, 3);
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
        extend(&mut w);
        w.into_inner()
    }

    #[test]
    fn test_new_order_decode_cash() {
        // 现货竞价：公共 89 字节 + 扩展 20 字节
        let body = build_order_body(msg_type::NEW_ORDER_CASH, |w| {
            w.i64(0);
            w.i64(0);
            w.u16(0);
            w.ch(b'0');
            w.ch(b'1');
        });
        assert_eq!(body.len(), OrderCommon::COMMON_LEN + 20);
        let o = NewOrder::decode(msg_type::NEW_ORDER_CASH, &body).unwrap();
        assert_eq!(o.common.appl_id, "010");
        assert_eq!(o.common.security_id, "000001");
        assert_eq!(o.common.cl_ord_id, "CL0000001");
        assert_eq!(o.common.side, b'1');
        assert_eq!(o.common.order_qty, 100_00);
        assert_eq!(o.common.price, 12_3400);
        assert_eq!(o.extend.cash_margin, b'1');
    }

    #[test]
    fn test_new_order_decode_option() {
        // 期权集中竞价：扩展 43 字节（表 4-9）
        let body = build_order_body(msg_type::NEW_ORDER_OPTION_AUCTION, |w| {
            w.i64(0);
            w.i64(0);
            w.u16(0);
            w.ch(b'0');
            w.ch(b'O');
            w.ch(b'0');
            w.str("AC0001", 6);
            w.str("SECONDARY123456", 16);
        });
        let o = NewOrder::decode(msg_type::NEW_ORDER_OPTION_AUCTION, &body).unwrap();
        assert_eq!(o.extend.position_effect, b'O');
        assert_eq!(o.extend.contract_account_code, "AC0001");
        assert_eq!(o.extend.secondary_order_id, "SECONDARY123456");
    }

    #[test]
    fn test_new_order_decode_bond_bid() {
        // 债券竞买：扩展 359 字节（表 4-24），验证竞买 17 字段布局
        let body = build_order_body(msg_type::NEW_ORDER_BOND_BID, |w| {
            w.str("MEMBER", 6);
            w.str("01", 2);
            w.str("INVESTOR01", 10);
            w.str("某机构客户名称", 120);
            w.str("TRADER01", 8);
            w.str("SECONDARY123456", 16);
            w.u16(2); // 发起
            w.u16(1); // 单一主体中标
            w.i64(10_0000); // 下限 10 元
            w.i64(11_0000); // 上限 11 元
            w.i64(50_00); // 最低 50 股
            w.u32(20260731);
            w.u16(103); // 多边净额
            w.ch(b'0'); // T+0
            w.ch(b'1'); // 匿名
            w.ch(b'1'); // 信用
            w.str("竞买备注", 160);
        });
        assert_eq!(body.len(), OrderCommon::COMMON_LEN + 359);
        let o = NewOrder::decode(msg_type::NEW_ORDER_BOND_BID, &body).unwrap();
        assert_eq!(o.common.appl_id, "417");
        assert_eq!(o.extend.member_id, "MEMBER");
        assert_eq!(o.extend.bid_trans_type, 2);
        assert_eq!(o.extend.low_limit_price, 10_0000);
        assert_eq!(o.extend.trade_date, 20260731);
        assert_eq!(o.extend.memo, "竞买备注");
    }

    #[test]
    fn test_new_order_decode_etf_repeated_groups() {
        // ETF 实时申赎：NoAccounts 重复组（表 4-26）
        let body = build_order_body(msg_type::NEW_ORDER_ETF_SUB_RED, |w| {
            w.u32(1);
            w.str("XSHG", 8);
            w.str("SH0001", 6);
            w.str("A00000000001", 12);
        });
        let o = NewOrder::decode(msg_type::NEW_ORDER_ETF_SUB_RED, &body).unwrap();
        assert_eq!(o.extend.no_accounts, 1);
        assert_eq!(o.extend.other_accounts.len(), 1);
        assert_eq!(o.extend.other_accounts[0].market_id, "XSHG");
        assert_eq!(o.extend.other_accounts[0].account_id, "A00000000001");
    }

    #[test]
    fn test_new_order_decode_hk_connect() {
        // 港股通：扩展 20 字节（表 4-20）
        let body = build_order_body(msg_type::NEW_ORDER_HK_CONNECT, |w| {
            w.i64(0);
            w.i64(0);
            w.u16(0);
            w.ch(b'0');
            w.ch(b'2'); // 整手
        });
        let o = NewOrder::decode(msg_type::NEW_ORDER_HK_CONNECT, &body).unwrap();
        assert_eq!(o.common.appl_id, "630");
        assert_eq!(o.extend.lot_type, b'2');
    }

    #[test]
    fn test_new_order_decode_tolerates_missing_extend() {
        // 只发公共字段（旧柜台）：扩展字段容忍缺失，不解析失败
        let body = build_order_body(msg_type::NEW_ORDER_CASH, |_| {});
        assert_eq!(body.len(), OrderCommon::COMMON_LEN);
        let o = NewOrder::decode(msg_type::NEW_ORDER_CASH, &body).unwrap();
        assert_eq!(o.common.cl_ord_id, "CL0000001");
        assert_eq!(o.extend.cash_margin, 0);
    }

    #[test]
    fn test_exec_rpt_ack_roundtrip_cash() {
        // 现货确认报告：公共 169 字节 + 扩展 20 字节
        let a = ExecRptAck {
            msg_type: msg_type::EXEC_RPT_CASH_ACK,
            partition_no: 1,
            report_index: 123,
            appl_id: "010".into(),
            reporting_pbu_id: "100001".into(),
            submitting_pbu_id: "100001".into(),
            security_id: "000001".into(),
            security_id_source: "102".into(),
            owner_type: 1,
            clearing_firm: "01".into(),
            transact_time: 20260731093000123,
            user_info: "UI".into(),
            order_id: "0000000000001234".into(),
            cl_ord_id: "CL0000001".into(),
            orig_cl_ord_id: String::new(),
            exec_id: "E000000000000001".into(),
            exec_type: exec_type::NEW,
            ord_status: ord_status::NEW,
            ord_rej_reason: 0,
            leaves_qty: 100_00,
            cum_qty: 0,
            side: b'1',
            ord_type: b'2',
            order_qty: 100_00,
            price: 12_3400,
            account_id: "0123456789AB".into(),
            branch_id: "0001".into(),
            order_restrictions: String::new(),
            extend: ExtendFields {
                cash_margin: b'1',
                ..Default::default()
            },
        };
        let frame = a.encode();
        assert_eq!(frame.len(), 8 + EXEC_RPT_ACK_COMMON_LEN + 20 + 4);
        // 校验和正确
        let cks = u32::from_be_bytes(frame[frame.len() - 4..].try_into().unwrap());
        assert_eq!(cks, checksum(&frame[..frame.len() - 4]));
        let d = ExecRptAck::decode(msg_type::EXEC_RPT_CASH_ACK, &frame[8..frame.len() - 4]).unwrap();
        assert_eq!(d.order_id, "0000000000001234");
        assert_eq!(d.exec_type, b'0');
        assert_eq!(d.extend.cash_margin, b'1');
    }

    #[test]
    fn test_exec_rpt_ack_roundtrip_hk() {
        // 港股通确认：IMCRejectText 变长字段
        let a = ExecRptAck {
            msg_type: msg_type::EXEC_RPT_HK_ACK,
            partition_no: 1,
            report_index: 1,
            appl_id: "630".into(),
            reporting_pbu_id: "100001".into(),
            submitting_pbu_id: "100001".into(),
            security_id: "00700".into(),
            security_id_source: "102".into(),
            owner_type: 1,
            clearing_firm: "01".into(),
            transact_time: 20260731093000123,
            user_info: "UI".into(),
            order_id: "0000000000001234".into(),
            cl_ord_id: "CL0000001".into(),
            orig_cl_ord_id: String::new(),
            exec_id: "E000000000000001".into(),
            exec_type: exec_type::REJECT,
            ord_status: ord_status::REJECTED,
            ord_rej_reason: 29998,
            leaves_qty: 0,
            cum_qty: 0,
            side: b'1',
            ord_type: b'2',
            order_qty: 100_00,
            price: 0,
            account_id: "0123456789AB".into(),
            branch_id: "0001".into(),
            order_restrictions: String::new(),
            extend: ExtendFields {
                reject_text: "RJTEXT01".into(),
                // “拒绝原因”UTF-8 占 12 字节（每汉字 3 字节）
                imc_reject_text_len: 12,
                imc_reject_text: "拒绝原因".into(),
                ..Default::default()
            },
        };
        let frame = a.encode();
        let d = ExecRptAck::decode(msg_type::EXEC_RPT_HK_ACK, &frame[8..frame.len() - 4]).unwrap();
        assert_eq!(d.extend.reject_text, "RJTEXT01");
        assert_eq!(d.extend.imc_reject_text_len, 12);
        assert_eq!(d.extend.imc_reject_text, "拒绝原因");
    }

    #[test]
    fn test_exec_rpt_trade_roundtrip_repo() {
        // 债券回购成交：扩展 MaturityDate
        let t = ExecRptTrade {
            msg_type: msg_type::EXEC_RPT_BOND_REPO_TRADE,
            partition_no: 1,
            report_index: 1,
            appl_id: "020".into(),
            reporting_pbu_id: "100001".into(),
            submitting_pbu_id: "100001".into(),
            security_id: "131800".into(),
            security_id_source: "102".into(),
            owner_type: 1,
            clearing_firm: "01".into(),
            transact_time: 20260731093000123,
            user_info: "UI".into(),
            order_id: "0000000000001234".into(),
            cl_ord_id: "CL0000001".into(),
            exec_id: "E000000000000001".into(),
            exec_type: exec_type::TRADE,
            ord_status: ord_status::FILLED,
            last_px: 12_3400,
            last_qty: 100_00,
            leaves_qty: 0,
            cum_qty: 100_00,
            side: b'2',
            account_id: "0123456789AB".into(),
            branch_id: "0001".into(),
            extend: ExtendFields {
                maturity_date: 20260814,
                ..Default::default()
            },
        };
        let frame = t.encode();
        assert_eq!(frame.len(), 8 + EXEC_RPT_TRADE_COMMON_LEN + 4 + 4);
        let d = ExecRptTrade::decode(msg_type::EXEC_RPT_BOND_REPO_TRADE, &frame[8..frame.len() - 4]).unwrap();
        assert_eq!(d.last_px, 12_3400);
        assert_eq!(d.extend.maturity_date, 20260814);
    }

    #[test]
    fn test_exec_rpt_trade_roundtrip_bond_bid() {
        // 债券竞买成交：本方+对手方 17 字段（表 4-62）
        let t = ExecRptTrade {
            msg_type: msg_type::EXEC_RPT_BOND_BID_TRADE,
            partition_no: 1,
            report_index: 1,
            appl_id: "417".into(),
            reporting_pbu_id: "100001".into(),
            submitting_pbu_id: "100001".into(),
            security_id: "102219".into(),
            security_id_source: "102".into(),
            owner_type: 1,
            clearing_firm: "01".into(),
            transact_time: 20260731093000123,
            user_info: "UI".into(),
            order_id: "0000000000001234".into(),
            cl_ord_id: "CL0000001".into(),
            exec_id: "E000000000000001".into(),
            exec_type: exec_type::TRADE,
            ord_status: ord_status::FILLED,
            last_px: 10_5000,
            last_qty: 100_00,
            leaves_qty: 0,
            cum_qty: 100_00,
            side: b'1',
            account_id: "0123456789AB".into(),
            branch_id: "0001".into(),
            extend: ExtendFields {
                member_id: "MEMBER".into(),
                // 对手方交易商代码 char[6]，与本方 MemberID 同宽
                counterparty_member_id: "CPMEMB".into(),
                bid_trans_type: 2,
                settl_type: 103,
                memo: "竞买成交".into(),
                ..Default::default()
            },
        };
        let frame = t.encode();
        let d = ExecRptTrade::decode(msg_type::EXEC_RPT_BOND_BID_TRADE, &frame[8..frame.len() - 4]).unwrap();
        assert_eq!(d.extend.member_id, "MEMBER");
        assert_eq!(d.extend.counterparty_member_id, "CPMEMB");
        assert_eq!(d.extend.memo, "竞买成交");
    }

    #[test]
    fn describe_new_order_fields() {
        let body = build_order_body(msg_type::NEW_ORDER_CASH, |w| {
            w.i64(0);
            w.i64(0);
            w.u16(0);
            w.ch(b'0');
            w.ch(b'1');
        });
        let fields = describe_fields(msg_type::NEW_ORDER_CASH, &body);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"MsgType"));
        assert!(names.contains(&"SecurityID"));
        assert!(names.contains(&"ClOrdID"));
        assert!(names.contains(&"CashMargin"));
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
    fn describe_ack_fields_include_extend() {
        let a = ExecRptAck {
            msg_type: msg_type::EXEC_RPT_SEC_LENDING_ACK,
            partition_no: 1,
            report_index: 1,
            appl_id: "070".into(),
            reporting_pbu_id: "100001".into(),
            submitting_pbu_id: "100001".into(),
            security_id: "000001".into(),
            security_id_source: "102".into(),
            owner_type: 1,
            clearing_firm: "01".into(),
            transact_time: 20260731093000123,
            user_info: "UI".into(),
            order_id: "0000000000001234".into(),
            cl_ord_id: "CL0000001".into(),
            orig_cl_ord_id: String::new(),
            exec_id: "E000000000000001".into(),
            exec_type: exec_type::NEW,
            ord_status: ord_status::NEW,
            ord_rej_reason: 0,
            leaves_qty: 100_00,
            cum_qty: 0,
            side: b'F',
            ord_type: b'2',
            order_qty: 100_00,
            price: 0,
            account_id: "0123456789AB".into(),
            branch_id: "0001".into(),
            order_restrictions: String::new(),
            extend: ExtendFields {
                expiration_days: 28,
                expiration_type: 1,
                share_property: "00".into(),
                ..Default::default()
            },
        };
        let frame = a.encode();
        let fields = describe_fields(msg_type::EXEC_RPT_SEC_LENDING_ACK, &frame[8..frame.len() - 4]);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"ApplID"));
        assert!(names.contains(&"ExecType"));
        assert!(names.contains(&"ExpirationDays"));
        assert!(names.contains(&"ShareProperty"));
        let appl = fields.iter().find(|f| f.name == "ApplID").unwrap();
        assert!(appl.value.contains("转融通"));
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
    fn test_msg_type_name_covers_all_businesses() {
        // 抽查几个业务的名称映射（英文标识 + 消息号；中文业务名见 appl_id_label）
        assert!(msg_type_name(msg_type::NEW_ORDER_HK_CONNECT).contains("NewOrderHkConnect"));
        assert!(msg_type_name(msg_type::EXEC_RPT_BOND_BID_ACK).contains("BondBidAck"));
        assert!(msg_type_name(msg_type::EXEC_RPT_HK_TRADE).contains("ExecRptHkTrade"));
        assert!(msg_type_name(999_999).contains("未知"));
    }
}
