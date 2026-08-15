//! 上交所竞价平台 Binary 交易接口协议编解码（规格说明书 0.54 版）。
//!
//! # 与深交所协议的关键差异
//!
//! 两个交易所的报文都是“二进制定长字段”，但格式约定完全不同：
//!
//! ```text
//! +----------------+------------------+------------------+----------------+----------------+
//! | MsgType (4字节) | MsgSeqNum (8字节) | MsgBodyLen (4字节) | 消息体 (N字节)   | Checksum (4字节) |
//! |  消息类型        |  消息序号          |  消息体长度         |  具体业务字段     |  校验和          |
//! +----------------+------------------+------------------+----------------+----------------+
//! ```
//!
//! - 消息头 16 字节（深交所 8 字节）：多了一个 MsgSeqNum 消息序号，
//!   每次建立新会话从 1 开始连续递增，供双方定位消息
//! - 校验和：从消息头开始到消息体结束所有字节按 uint8 累加（自然溢出），
//!   与深交所“求和 mod 256”结果一致
//! - 所有整数大端序；上行消息（OMS→TDGW）最大 4K
//! - Price  = int64 N13(5)：价格放大 10 万倍，如 1864000 表示 18.64 元
//! - Qty    = int64 N15(3)：数量放大 1000 倍，如 100000 表示 100 股
//! - Amount = int64 N18(5)：金额放大 10 万倍
//! - date   = uint32，格式 YYYYMMDD
//! - ntime  = uint64，格式 HHMMSSsssnnnn（时分秒毫秒 + 4 位百纳秒）
//!
//! # 消息序号怎么填
//!
//! 编码函数一律把 MsgSeqNum 填 0 占位；session 模块的 writer 任务在真正
//! 发送时按发送顺序统一赋值（见 [`finalize_seq`]）。这样即便回报被
//! “延迟任务”打乱了生成顺序，序号依然与实际发出顺序严格一致。

use std::io;

use crate::capture::ParsedField;

/// 消息类型常量（报文头的 MsgType 字段，括号内为规范章节号）
pub mod msg_type {
    /// 登录 Logon（4.3.1）
    pub const LOGON: u32 = 40;
    /// 注销 Logout（4.3.2）
    pub const LOGOUT: u32 = 41;
    /// 心跳 Heartbeat（4.3.3），MsgBodyLen = 0
    pub const HEARTBEAT: u32 = 33;
    /// 新订单申报 NewOrderSingle（4.5.1）
    pub const NEW_ORDER: u32 = 58;
    /// 撤单申报 OrderCancelRequest（4.5.2）
    pub const CANCEL_ORDER: u32 = 61;
    /// 申报响应/撤单成功执行报告 ExecutionReport（4.5.3）
    pub const EXEC_RPT: u32 = 32;
    /// 撤单失败响应 CancelReject（4.5.4）
    pub const CANCEL_REJECT: u32 = 59;
    /// 成交执行报告 ExecutionReport-Trade（4.5.5）
    pub const TRADE: u32 = 103;
    /// 申报拒绝 OrderReject（4.5.6）
    pub const ORDER_REJECT: u32 = 204;
    /// 分区序号同步 ExecRptSync（4.6.4）
    pub const EXEC_RPT_SYNC: u32 = 206;
    /// 分区序号同步响应 ExecRptSyncRsp（4.6.5）
    pub const EXEC_RPT_SYNC_RSP: u32 = 207;
    /// 执行报告信息 ExecRptInfo（4.6.3）
    pub const EXEC_RPT_INFO: u32 = 208;
    /// 平台状态 PlatformState（4.6.2）
    pub const PLATFORM_STATE: u32 = 209;
    /// 分区执行报告结束 ExecRptEndOfStream（4.6.6）
    pub const EXEC_RPT_EOS: u32 = 210;
    /// 注册处理申报（4.4.1）
    pub const REGISTRATION: u32 = 301;
    /// 注册处理执行回报（4.4.2）
    pub const REGISTRATION_RPT: u32 = 302;
    /// 网络密码服务申报（4.5.1）
    pub const PWD_SERVICE: u32 = 306;
    /// 网络密码服务申报响应（4.5.2）
    pub const PWD_SERVICE_RSP: u32 = 308;
}

/// 执行类型 ExecType 取值（ASCII 字符）
pub mod exec_type {
    /// 申报确认（订单已被交易所接受）
    pub const NEW: u8 = b'0';
    /// 撤单成功
    pub const CANCELLED: u8 = b'4';
    /// 订单拒绝
    pub const REJECT: u8 = b'8';
    /// 成交
    pub const TRADE: u8 = b'F';
}

/// 订单状态 OrdStatus 取值
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

/// 平台状态 PlatformState 取值（消息 209 的第二个字段）
pub mod platform_state {
    pub const NOT_OPEN: u16 = 0;
    pub const PRE_OPEN: u16 = 1;
    /// 开放（可以报单）
    pub const OPEN: u16 = 2;
    pub const BREAK: u16 = 3;
    pub const CLOSE: u16 = 4;
}

// ---------------------------------------------------------------------------
// 业务标识 BizID（表 3.2.1 业务类型表，新订单/撤单/回报里的业务标识）
// ---------------------------------------------------------------------------

/// 现货竞价交易：唯一支持部分成交、SetID 为 1-6,20（多分区）的业务
pub const BIZ_ID_CASH_AUCTION: u32 = 100_010;
/// 发行（ETF 认购可撤单，其他不可撤）
pub const BIZ_ID_ISSUE: u32 = 300_010;
/// 配股/科创板配售（不支持撤单，有成交确认）
pub const BIZ_ID_RIGHTS: u32 = 300_020;
/// 配转债（不支持撤单，有成交确认）
pub const BIZ_ID_RIGHTS_BOND: u32 = 300_021;
/// 要约预受
pub const BIZ_ID_TENDER_ACCEPT: u32 = 300_030;
/// 要约撤销
pub const BIZ_ID_TENDER_CANCEL: u32 = 300_031;
/// 开放式基金申购
pub const BIZ_ID_FUND_SUB: u32 = 300_040;
/// 开放式基金赎回
pub const BIZ_ID_FUND_RED: u32 = 300_041;
/// 开放式基金认购
pub const BIZ_ID_FUND_SUB_ISSUE: u32 = 300_050;
/// 开放式基金转托管（扩展字段 Custodian）
pub const BIZ_ID_FUND_TRANSFER: u32 = 300_060;
/// 开放式基金分红设置（扩展字段 DividendSelect）
pub const BIZ_ID_FUND_DIVIDEND: u32 = 300_070;
/// 开放式基金转换（扩展字段 DestSecurity）
pub const BIZ_ID_FUND_CONVERT: u32 = 300_080;
/// 余券划转（扩展字段 DestSecurity）
pub const BIZ_ID_REMAIN_TRANSFER: u32 = 300_090;
/// 还券划转（扩展字段 DestSecurity）
pub const BIZ_ID_RETURN_TRANSFER: u32 = 300_091;
/// 担保品划入（扩展字段 DestSecurity）
pub const BIZ_ID_COLLATERAL_IN: u32 = 300_092;
/// 担保品划出（扩展字段 DestSecurity）
pub const BIZ_ID_COLLATERAL_OUT: u32 = 300_093;
/// 券源划入（扩展字段 DestSecurity）
pub const BIZ_ID_SEC_SRC_IN: u32 = 300_094;
/// 券源划出（扩展字段 DestSecurity）
pub const BIZ_ID_SEC_SRC_OUT: u32 = 300_095;
/// 网络密码服务：不经 58 新订单申报，走 306/308 独立消息；不进执行报告流
pub const BIZ_ID_PWD_SERVICE: u32 = 300_100;
/// 指定登记：不经 58 新订单申报，走 301/302 注册处理；执行报告分区 992
pub const BIZ_ID_DESIGNATION: u32 = 300_200;
/// 指定撤销：同指定登记
pub const BIZ_ID_DESIGNATION_CANCEL: u32 = 300_201;

/// 除现货竞价外的业务共用执行报告分区（表 3.2.1 的 SetID 列）
pub const SET_ID_OTHER_BIZ: u32 = 991;
/// 注册处理（指定登记/指定撤销）的执行报告分区
pub const SET_ID_DESIGNATION: u32 = 992;

/// 校验和：从消息头到消息体结束所有字节按 uint8 累加（自然溢出），
/// 规范附录一的 C 代码等价于无符号字节和 mod 256
pub fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) as u32
}

/// 是否回报类消息：沪市回报（32 执行报告/59 撤单拒绝/103 成交/302 注册执行回报）
/// 消息体统一以 Pbu(8)+SetID(4)+ReportIndex(8) 开头，writer 据此补写 ReportIndex；
/// 申报拒绝(204)/密码服务响应(308)/同步响应(207)/心跳/登录等消息无 ReportIndex。
pub fn is_report_frame(mt: u32) -> bool {
    matches!(mt, 32 | 59 | 103 | 302)
}

/// 发送前把回报记录号 ReportIndex 补进报文并重算校验和。
///
/// 回报类消息体统一以 Pbu(char[8]) + SetID(u32,4 字节) + ReportIndex(u64,8 字节) 开头
/// （见 ExecRpt/TradeRpt/CancelReject/RegistrationRpt 的 encode），因此 ReportIndex
/// 恒位于帧偏移 16(报文头) + 8 + 4 = 28 处。
/// ReportIndex 在真实发送时分配（与发送顺序严格一致），避免“生成时分配 +
/// 延迟发送”导致线上序号乱序（真实柜台按分区校验 ReportIndex 单调递增）。
pub fn patch_report_index(frame: &mut [u8], report_index: u64) {
    frame[28..36].copy_from_slice(&report_index.to_be_bytes());
    // 报文内容变了，校验和要重算（与 finalize_seq 同一思路）
    let n = frame.len();
    let cks = checksum(&frame[..n - 4]);
    frame[n - 4..].copy_from_slice(&cks.to_be_bytes());
}

/// 组装完整报文：消息头（类型+序号占位+长度）+ 消息体 + 校验和。
/// MsgSeqNum 先填 0，真正发送前由 [`finalize_seq`] 补上。
pub fn frame(msg_type: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 20);
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(&0u64.to_be_bytes()); // MsgSeqNum 占位
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(&checksum(&out).to_be_bytes());
    out
}

/// 把消息序号写进报文头（字节 4..12）并重算校验和。
/// writer 任务发送每条报文前调用一次，保证序号与实际发送顺序一致。
pub fn finalize_seq(frame: &mut [u8], seq: u64) {
    let n = frame.len();
    frame[4..12].copy_from_slice(&seq.to_be_bytes());
    let cks = checksum(&frame[..n - 4]);
    frame[n - 4..].copy_from_slice(&cks.to_be_bytes());
}

/// 消息体写入器：把各种类型的字段按规范顺序追加到字节缓冲区
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

    /// 定长字符串：截断到 len 字节（不把中文字符拦腰切断），不足补空格
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

    /// 单字符字段（规范中的 char，如买卖方向、订单类型）
    pub fn ch(&mut self, c: u8) {
        self.buf.push(c);
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u64(&mut self, v: u64) {
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

/// 消息体读取器：BodyWriter 的逆操作
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

    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
}

/// 当前交易日期，date 类型，格式 YYYYMMDD（如 20260731）
pub fn now_date() -> u32 {
    chrono::Local::now()
        .format("%Y%m%d")
        .to_string()
        .parse()
        .unwrap_or(0)
}

/// 当前时间，ntime 类型，格式 HHMMSSsssnnnn（时分秒毫秒 + 4 位百纳秒）。
/// 本地时钟只有毫秒精度，后 4 位百纳秒补 0
pub fn now_ntime() -> u64 {
    chrono::Local::now()
        .format("%H%M%S%3f")
        .to_string()
        .parse::<u64>()
        .unwrap_or(0)
        * 10_000
}

// ---------------------------------------------------------------------------
// 会话层消息
// ---------------------------------------------------------------------------

/// 登录消息（MsgType=40），OMS→TDGW 与 TDGW→OMS 结构相同。
/// 注意：与深交所不同，上交所 Logon 没有密码字段。
#[derive(Debug, Clone, Default)]
pub struct Logon {
    pub sender_comp_id: String, // char[32] 发送方代码
    pub target_comp_id: String, // char[32] 接收方代码（OMS 填 "TDGW"）
    pub heart_bt_int: u16,      // 心跳间隔（秒），有效范围 [5,60]
    pub prtcl_version: String,  // char[8] 协议版本，如 "0.50"
    pub trade_date: u32,        // 交易日期 YYYYMMDD
    pub qsize: u32,             // 滑动窗口大小
}

impl Logon {
    pub const BODY_LEN: usize = 32 + 32 + 2 + 8 + 4 + 4;

    pub fn decode(body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        Ok(Self {
            sender_comp_id: r.str(32)?,
            target_comp_id: r.str(32)?,
            heart_bt_int: r.u16()?,
            prtcl_version: r.str(8)?,
            trade_date: r.u32()?,
            qsize: r.u32()?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.str(&self.sender_comp_id, 32);
        w.str(&self.target_comp_id, 32);
        w.u16(self.heart_bt_int);
        w.str(&self.prtcl_version, 8);
        w.u32(self.trade_date);
        w.u32(self.qsize);
        frame(msg_type::LOGON, &w.into_inner())
    }
}

/// 注销消息（MsgType=41）。SessionStatus=0 表示正常退出，
/// 其余取值见规范附录三错误代码（如 5002 心跳超时）
#[derive(Debug, Clone, Default)]
pub struct Logout {
    pub session_status: u32, // 会话状态/错误码
    pub text: String,        // char[64] 说明文字
}

impl Logout {
    pub fn decode(body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        Ok(Self {
            session_status: r.u32()?,
            text: r.str(64)?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.u32(self.session_status);
        w.str(&self.text, 64);
        frame(msg_type::LOGOUT, &w.into_inner())
    }
}

/// 心跳消息（MsgType=33，MsgBodyLen=0）
pub fn encode_heartbeat() -> Vec<u8> {
    frame(msg_type::HEARTBEAT, &[])
}

/// 平台状态消息（MsgType=209）：PlatformID（竞价平台=0）+ 状态
pub fn encode_platform_state(platform_id: u16, state: u16) -> Vec<u8> {
    let mut w = BodyWriter::new();
    w.u16(platform_id);
    w.u16(state);
    frame(msg_type::PLATFORM_STATE, &w.into_inner())
}

/// 执行报告信息消息（MsgType=208）：登录成功后 TDGW 主动推送，
/// 告知 OMS 有哪些回报 PBU 和分区（OMS 据此发起序号同步）。
///
/// 4.6.3 定义的嵌套结构：PlatformID + NoGroups{Pbu + NoGroups{SetID}}，
/// 每个 PBU 下各有一组分区号。调用方传 pbu_set_ids 的每一项为 (Pbu, 该 PBU 的分区列表)。
pub fn encode_exec_rpt_info(platform_id: u16, pbu_set_ids: &[(&str, &[u32])]) -> Vec<u8> {
    let mut w = BodyWriter::new();
    w.u16(platform_id);
    w.u16(pbu_set_ids.len() as u16);
    for &(pbu, set_ids) in pbu_set_ids {
        w.str(pbu, 8);
        w.u16(set_ids.len() as u16);
        for s in set_ids {
            w.u32(*s);
        }
    }
    frame(msg_type::EXEC_RPT_INFO, &w.into_inner())
}

/// 分区执行报告结束消息（MsgType=210）
pub fn encode_exec_rpt_eos(pbu: &str, set_id: u32, end_report_index: u64) -> Vec<u8> {
    let mut w = BodyWriter::new();
    w.str(pbu, 8);
    w.u32(set_id);
    w.u64(end_report_index);
    frame(msg_type::EXEC_RPT_EOS, &w.into_inner())
}

/// 分区序号同步请求（MsgType=206）中的一个分区项
#[derive(Debug, Clone, Default)]
pub struct SyncGroup {
    pub pbu: String,             // char[8] 回报交易单元
    pub set_id: u32,             // 平台分区号
    pub begin_report_index: u64, // 期望的起始回报序号
}

/// 解析分区序号同步请求（MsgType=206）：NoGroups + 每组 (Pbu, SetID, BeginReportIndex)
pub fn decode_exec_rpt_sync(body: &[u8]) -> io::Result<Vec<SyncGroup>> {
    let mut r = BodyReader::new(body);
    let n = r.u16()? as usize;
    let mut groups = Vec::with_capacity(n);
    for _ in 0..n {
        groups.push(SyncGroup {
            pbu: r.str(8)?,
            set_id: r.u32()?,
            begin_report_index: r.u64()?,
        });
    }
    Ok(groups)
}

/// 分区序号同步响应（MsgType=207）中的一个分区项
#[derive(Debug, Clone, Default)]
pub struct SyncRspGroup {
    pub pbu: String,             // char[8]
    pub set_id: u32,
    pub begin_report_index: u64, // 回填请求值
    pub end_report_index: u64,   // 该分区当前最大回报序号
    pub rej_reason: u32,         // 0 = 同步成功
    pub text: String,            // char[64]
}

/// 编码分区序号同步响应（MsgType=207）
pub fn encode_exec_rpt_sync_rsp(groups: &[SyncRspGroup]) -> Vec<u8> {
    let mut w = BodyWriter::new();
    w.u16(groups.len() as u16);
    for g in groups {
        w.str(&g.pbu, 8);
        w.u32(g.set_id);
        w.u64(g.begin_report_index);
        w.u64(g.end_report_index);
        w.u32(g.rej_reason);
        w.str(&g.text, 64);
    }
    frame(msg_type::EXEC_RPT_SYNC_RSP, &w.into_inner())
}

// ---------------------------------------------------------------------------
// 业务消息：新订单（58）与执行报告（32/103 等）
// ---------------------------------------------------------------------------

/// 新订单/执行报告的扩展字段（超集，仅本业务相关字段有值）。
///
/// 表 3.2.1 中带扩展字段的业务共 9 种（4.3.1.1~4.3.1.7）：
/// - 转托管 300060：Custodian char[3]（目标方代理人销售人代码）
/// - 分红设置 300070：DividendSelect char（U=红利转投 C=现金分红）
/// - 转换 300080 / 余券 300090 / 还券 300091 / 担保品 300092/300093 /
///   券源 300094/300095：DestSecurity char[12]（目标证券代码）
/// 其余业务无扩展字段；执行报告（32）按 4.3.3.1 说明 2 同样携带。
#[derive(Debug, Clone, Default)]
pub struct ExtendFields {
    /// 转托管目标方代理人（对方销售人代码 000-999，不足 3 位左补 0）
    pub custodian: String, // char[3]
    /// 分红方式：'U'=红利转投 'C'=现金分红
    pub dividend_select: u8, // char
    /// 目标基金/证券代码（转换与各类划转，前 6 位有效）
    pub dest_security: String, // char[12]
}

/// 某业务扩展字段的字节总长度（表 3.2.1 + 4.3.1.1~4.3.1.7）；无扩展字段的业务返回 0
pub fn extend_len(biz_id: u32) -> usize {
    match biz_id {
        BIZ_ID_FUND_TRANSFER => 3,
        BIZ_ID_FUND_DIVIDEND => 1,
        BIZ_ID_FUND_CONVERT
        | BIZ_ID_REMAIN_TRANSFER
        | BIZ_ID_RETURN_TRANSFER
        | BIZ_ID_COLLATERAL_IN
        | BIZ_ID_COLLATERAL_OUT
        | BIZ_ID_SEC_SRC_IN
        | BIZ_ID_SEC_SRC_OUT => 12,
        _ => 0,
    }
}

/// 新订单申报（MsgType=58，公共字段 125 字节，扩展字段按业务追加）
#[derive(Debug, Clone, Default)]
pub struct NewOrder {
    pub biz_id: u32,           // 业务标识（表 3.2.1，现货竞价 = 100010）
    pub biz_pbu: String,       // char[8] 业务交易单元
    pub cl_ord_id: String,     // char[10] 客户订单编号（10 位数字字母）
    pub security_id: String,   // char[12] 证券代码
    pub account: String,       // char[13] 证券账户
    pub owner_type: u8,        // 订单所有者类型
    pub side: u8,              // 买卖方向 '1'=买 '2'=卖
    pub price: i64,            // 价格 N13(5)，放大 10 万倍
    pub order_qty: i64,        // 数量 N15(3)，放大 1000 倍
    pub ord_type: u8,          // 订单类型 '1'市转撤 '2'限价 '3'市转限 '4'本方最优 '5'对手方最优
    pub time_in_force: u8,     // 订单有效时间类型
    pub transact_time: u64,    // 委托时间 ntime
    pub credit_tag: String,    // char[2] 信用标签
    pub clearing_firm: String, // char[8] 结算会员
    pub branch_id: String,     // char[8] 营业部代码
    pub user_info: String,     // char[32] 用户私有信息（下行回填，前 12 位有效）
    /// 各业务扩展字段（4.3.1.1~4.3.1.7，超集，仅本业务相关字段有值）
    pub extend: ExtendFields,
}

impl NewOrder {
    /// 公共字段长度（不含扩展字段）
    pub const BODY_LEN: usize = 4 + 8 + 10 + 12 + 13 + 1 + 1 + 8 + 8 + 1 + 1 + 8 + 2 + 8 + 8 + 32;

    /// 解码：先读公共字段，再按 BizID 读取对应业务的扩展字段。
    /// 扩展字段“容忍缺失”：对端只发公共字段时剩余长度不足，跳过不报错。
    pub fn decode(body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        let mut o = Self {
            biz_id: r.u32()?,
            biz_pbu: r.str(8)?,
            cl_ord_id: r.str(10)?,
            security_id: r.str(12)?,
            account: r.str(13)?,
            owner_type: r.u8()?,
            side: r.ch()?,
            price: r.i64()?,
            order_qty: r.i64()?,
            ord_type: r.ch()?,
            time_in_force: r.ch()?,
            transact_time: r.u64()?,
            credit_tag: r.str(2)?,
            clearing_firm: r.str(8)?,
            branch_id: r.str(8)?,
            user_info: r.str(32)?,
            extend: ExtendFields::default(),
        };
        // 4.3.1.1~4.3.1.7 按业务读取扩展字段
        match o.biz_id {
            BIZ_ID_FUND_TRANSFER => {
                if r.remaining() >= 3 {
                    o.extend.custodian = r.str(3)?;
                }
            }
            BIZ_ID_FUND_DIVIDEND => {
                if r.remaining() >= 1 {
                    o.extend.dividend_select = r.ch()?;
                }
            }
            BIZ_ID_FUND_CONVERT
            | BIZ_ID_REMAIN_TRANSFER
            | BIZ_ID_RETURN_TRANSFER
            | BIZ_ID_COLLATERAL_IN
            | BIZ_ID_COLLATERAL_OUT
            | BIZ_ID_SEC_SRC_IN
            | BIZ_ID_SEC_SRC_OUT => {
                if r.remaining() >= 12 {
                    o.extend.dest_security = r.str(12)?;
                }
            }
            _ => {}
        }
        Ok(o)
    }
}

/// 撤单申报（MsgType=61，107 字节）
#[derive(Debug, Clone, Default)]
pub struct CancelOrder {
    pub biz_id: u32,
    pub biz_pbu: String,        // char[8]
    pub cl_ord_id: String,      // char[10] 本笔撤单的编号
    pub security_id: String,    // char[12]
    pub account: String,        // char[13]
    pub owner_type: u8,
    pub side: u8,
    pub orig_cl_ord_id: String, // char[10] 要撤的原订单编号
    pub transact_time: u64,     // ntime
    pub branch_id: String,      // char[8]
    pub user_info: String,      // char[32]
}

impl CancelOrder {
    pub const BODY_LEN: usize = 4 + 8 + 10 + 12 + 13 + 1 + 1 + 10 + 8 + 8 + 32;

    pub fn decode(body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        Ok(Self {
            biz_id: r.u32()?,
            biz_pbu: r.str(8)?,
            cl_ord_id: r.str(10)?,
            security_id: r.str(12)?,
            account: r.str(13)?,
            owner_type: r.u8()?,
            side: r.ch()?,
            orig_cl_ord_id: r.str(10)?,
            transact_time: r.u64()?,
            branch_id: r.str(8)?,
            user_info: r.str(32)?,
        })
    }
}

/// 申报响应/撤单成功执行报告（MsgType=32，213 字节）。
/// 三种用途（ExecType/OrdStatus 组合）：
/// - 申报成功：'0'/'0'  - 申报拒绝：'8'/'8'  - 撤单成功：'4'/'4'
#[derive(Debug, Clone, Default)]
pub struct ExecRpt {
    pub pbu: String,              // char[8] 回报交易单元
    pub set_id: u32,              // 平台分区号
    pub report_index: u64,        // 回报序号（分区内连续递增）
    pub biz_id: u32,
    pub exec_type: u8,            // 执行类型
    pub biz_pbu: String,          // char[8]
    pub cl_ord_id: String,        // char[10]
    pub security_id: String,      // char[12]
    pub account: String,          // char[13]
    pub owner_type: u8,
    pub side: u8,
    pub price: i64,               // N13(5)
    pub order_qty: i64,           // N15(3)
    pub leaves_qty: i64,          // 剩余数量
    pub cxl_qty: i64,             // 已撤数量
    pub ord_type: u8,
    pub time_in_force: u8,
    pub ord_status: u8,           // 订单状态
    pub credit_tag: String,       // char[2]
    pub orig_cl_ord_id: String,   // char[10]（撤单成功时有值）
    pub clearing_firm: String,    // char[8]
    pub branch_id: String,        // char[8]
    pub ord_rej_reason: u32,      // 拒绝原因代码
    pub ord_cnfm_id: String,      // char[16] 交易所订单确认编号（数字左补 0）
    pub orig_ord_cnfm_id: String, // char[16] 原订单确认编号
    pub trade_date: u32,          // date
    pub transact_time: u64,       // ntime
    pub user_info: String,        // char[32] 回填上行值
    /// 各业务扩展字段（4.3.3.1 说明 2：与新订单对应业务的扩展字段一致）
    pub extend: ExtendFields,
}

/// 把扩展字段按业务写入消息体尾部（4.3.1.1~4.3.1.7）；无扩展字段的业务不写
pub fn write_extend(w: &mut BodyWriter, biz_id: u32, extend: &ExtendFields) {
    match biz_id {
        BIZ_ID_FUND_TRANSFER => w.str(&extend.custodian, 3),
        BIZ_ID_FUND_DIVIDEND => w.ch(extend.dividend_select),
        BIZ_ID_FUND_CONVERT
        | BIZ_ID_REMAIN_TRANSFER
        | BIZ_ID_RETURN_TRANSFER
        | BIZ_ID_COLLATERAL_IN
        | BIZ_ID_COLLATERAL_OUT
        | BIZ_ID_SEC_SRC_IN
        | BIZ_ID_SEC_SRC_OUT => w.str(&extend.dest_security, 12),
        _ => {}
    }
}

impl ExecRpt {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.str(&self.pbu, 8);
        w.u32(self.set_id);
        w.u64(self.report_index);
        w.u32(self.biz_id);
        w.ch(self.exec_type);
        w.str(&self.biz_pbu, 8);
        w.str(&self.cl_ord_id, 10);
        w.str(&self.security_id, 12);
        w.str(&self.account, 13);
        w.u8(self.owner_type);
        w.ch(self.side);
        w.i64(self.price);
        w.i64(self.order_qty);
        w.i64(self.leaves_qty);
        w.i64(self.cxl_qty);
        w.ch(self.ord_type);
        w.ch(self.time_in_force);
        w.ch(self.ord_status);
        w.str(&self.credit_tag, 2);
        w.str(&self.orig_cl_ord_id, 10);
        w.str(&self.clearing_firm, 8);
        w.str(&self.branch_id, 8);
        w.u32(self.ord_rej_reason);
        w.str(&self.ord_cnfm_id, 16);
        w.str(&self.orig_ord_cnfm_id, 16);
        w.u32(self.trade_date);
        w.u64(self.transact_time);
        w.str(&self.user_info, 32);
        write_extend(&mut w, self.biz_id, &self.extend);
        frame(msg_type::EXEC_RPT, &w.into_inner())
    }
}

/// 撤单失败响应（MsgType=59）
#[derive(Debug, Clone, Default)]
pub struct CancelReject {
    pub pbu: String,            // char[8]
    pub set_id: u32,
    pub report_index: u64,
    pub biz_id: u32,
    pub biz_pbu: String,        // char[8]
    pub cl_ord_id: String,      // char[10]
    pub security_id: String,    // char[12]
    pub orig_cl_ord_id: String, // char[10]
    pub branch_id: String,      // char[8]
    pub cxl_rej_reason: u32,    // 撤单失败原因代码
    pub trade_date: u32,
    pub transact_time: u64,
    pub user_info: String,      // char[32]
}

impl CancelReject {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.str(&self.pbu, 8);
        w.u32(self.set_id);
        w.u64(self.report_index);
        w.u32(self.biz_id);
        w.str(&self.biz_pbu, 8);
        w.str(&self.cl_ord_id, 10);
        w.str(&self.security_id, 12);
        w.str(&self.orig_cl_ord_id, 10);
        w.str(&self.branch_id, 8);
        w.u32(self.cxl_rej_reason);
        w.u32(self.trade_date);
        w.u64(self.transact_time);
        w.str(&self.user_info, 32);
        frame(msg_type::CANCEL_REJECT, &w.into_inner())
    }
}

/// 成交执行报告（MsgType=103，ExecType=F）
#[derive(Debug, Clone, Default)]
pub struct TradeRpt {
    pub pbu: String,             // char[8]
    pub set_id: u32,
    pub report_index: u64,
    pub biz_id: u32,
    pub exec_type: u8,           // 'F'
    pub biz_pbu: String,         // char[8]
    pub cl_ord_id: String,       // char[10]
    pub security_id: String,     // char[12]
    pub account: String,         // char[13]
    pub owner_type: u8,
    pub order_entry_time: u64,   // ntime 订单进入撮合平台时间
    pub last_px: i64,            // 成交价 N13(5)
    pub last_qty: i64,           // 成交数量 N15(3)
    pub gross_trade_amt: i64,    // 成交金额 N18(5)
    pub side: u8,
    pub order_qty: i64,
    pub leaves_qty: i64,
    pub ord_status: u8,          // '1'部分成交 '2'全部成交
    pub credit_tag: String,      // char[2]
    pub clearing_firm: String,   // char[8]
    pub branch_id: String,       // char[8]
    pub trd_cnfm_id: String,     // char[16] 成交编号（数字左补 0）
    pub ord_cnfm_id: String,     // char[16]
    pub trade_date: u32,
    pub transact_time: u64,
    pub user_info: String,       // char[32]
}

impl TradeRpt {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.str(&self.pbu, 8);
        w.u32(self.set_id);
        w.u64(self.report_index);
        w.u32(self.biz_id);
        w.ch(self.exec_type);
        w.str(&self.biz_pbu, 8);
        w.str(&self.cl_ord_id, 10);
        w.str(&self.security_id, 12);
        w.str(&self.account, 13);
        w.u8(self.owner_type);
        w.u64(self.order_entry_time);
        w.i64(self.last_px);
        w.i64(self.last_qty);
        w.i64(self.gross_trade_amt);
        w.ch(self.side);
        w.i64(self.order_qty);
        w.i64(self.leaves_qty);
        w.ch(self.ord_status);
        w.str(&self.credit_tag, 2);
        w.str(&self.clearing_firm, 8);
        w.str(&self.branch_id, 8);
        w.str(&self.trd_cnfm_id, 16);
        w.str(&self.ord_cnfm_id, 16);
        w.u32(self.trade_date);
        w.u64(self.transact_time);
        w.str(&self.user_info, 32);
        frame(msg_type::TRADE, &w.into_inner())
    }
}

/// 申报拒绝（MsgType=204）：前置检查未通过时的独立拒绝消息
#[derive(Debug, Clone, Default)]
pub struct OrderReject {
    pub biz_id: u32,
    pub biz_pbu: String,     // char[8]
    pub cl_ord_id: String,   // char[10]
    pub security_id: String, // char[12]
    pub ord_rej_reason: u32,
    pub trade_date: u32,
    pub transact_time: u64,
    pub user_info: String,   // char[32]
}

impl OrderReject {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.u32(self.biz_id);
        w.str(&self.biz_pbu, 8);
        w.str(&self.cl_ord_id, 10);
        w.str(&self.security_id, 12);
        w.u32(self.ord_rej_reason);
        w.u32(self.trade_date);
        w.u64(self.transact_time);
        w.str(&self.user_info, 32);
        frame(msg_type::ORDER_REJECT, &w.into_inner())
    }
}

// ---------------------------------------------------------------------------
// 注册处理（4.4）：301 申报 / 302 执行回报
// ---------------------------------------------------------------------------

/// 注册指令 DesignationInstruction 取值
pub mod designation_instruction {
    /// 1 = 指定交易登记
    pub const REGISTER: u8 = b'1';
    /// 2 = 指定交易撤销
    pub const CANCEL: u8 = b'2';
}

/// 注册处理申报（MsgType=301，4.4.1）。
///
/// 仅支持两种组合（说明 1）：
/// - 指定登记：SecurityID=799999，注册指令='1'，注册类型='1'
/// - 指定撤销：SecurityID=799998，注册指令='2'，注册类型='1'
#[derive(Debug, Clone, Default)]
pub struct RegistrationOrder {
    pub biz_id: u32,          // 业务编号，指定登记 300200 / 指定撤销 300201
    pub biz_pbu: String,      // char[8] 业务 PBU 编号
    pub cl_ord_id: String,    // char[10] 会员内部订单编号
    pub security_id: String,  // char[12] 证券代码（799999 指定登记 / 799998 指定撤销）
    pub account: String,      // char[13] 证券账户
    pub owner_type: u8,       // 订单所有者类型，暂不启用
    pub designation_instruction: u8, // 注册指令 '1'登记 '2'撤销
    pub designation_trans_type: u8,  // 注册类型 '1'=新注册请求
    pub orig_cl_ord_id: String,      // char[10] 原始订单编号，暂不启用
    pub transact_time: u64,          // ntime 申报时间
    pub branch_id: String,           // char[8] 营业部代码，暂不启用
    pub user_info: String,           // char[32] 用户私有信息
}

impl RegistrationOrder {
    pub const BODY_LEN: usize = 4 + 8 + 10 + 12 + 13 + 1 + 1 + 1 + 10 + 8 + 8 + 32;

    pub fn decode(body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        Ok(Self {
            biz_id: r.u32()?,
            biz_pbu: r.str(8)?,
            cl_ord_id: r.str(10)?,
            security_id: r.str(12)?,
            account: r.str(13)?,
            owner_type: r.u8()?,
            designation_instruction: r.ch()?,
            designation_trans_type: r.ch()?,
            orig_cl_ord_id: r.str(10)?,
            transact_time: r.u64()?,
            branch_id: r.str(8)?,
            user_info: r.str(32)?,
        })
    }
}

/// 注册处理执行回报（MsgType=302，4.4.2）。
///
/// 与 32 执行报告同构：带 Pbu/SetID(=992)/ReportIndex，编入执行报告流；
/// ExecType 与 OrdStatus 组合取值：0/0 申报成功、8/8 申报拒绝、4/4 撤单成功。
#[derive(Debug, Clone, Default)]
pub struct RegistrationRpt {
    pub pbu: String,            // char[8] 登录或订阅 Pbu
    pub set_id: u32,            // 平台内分区号（指定登记/撤销 = 992）
    pub report_index: u64,      // 执行报告编号（分区内连续递增）
    pub biz_id: u32,            // 业务编号
    pub exec_type: u8,          // 执行类型 '0'成功 '4'撤单成功 '8'拒绝
    pub biz_pbu: String,        // char[8] 业务 PBU 编号
    pub cl_ord_id: String,      // char[10] 会员内部订单编号
    pub security_id: String,    // char[12] 证券代码
    pub account: String,        // char[13] 证券账户
    pub owner_type: u8,         // 订单所有者类型，暂不启用
    pub ord_status: u8,         // 订单状态 '0'新订单 '4'已撤销 '8'已拒绝
    pub orig_cl_ord_id: String, // char[10] 仅撤单成功（ExecType=4）时有意义
    pub branch_id: String,      // char[8] 营业部代码，暂不启用
    pub ord_rej_reason: u32,    // 订单拒绝码，仅拒绝响应（ExecType=8）时有意义
    pub ord_cnfm_id: String,    // char[16] 交易所订单编号，仅申报成功（ExecType=0）时有意义
    pub orig_ord_cnfm_id: String, // char[16] 暂不启用
    pub trade_date: u32,        // date 交易日期
    pub transact_time: u64,     // ntime 回报时间
    pub user_info: String,      // char[32] 用户私有信息
}

impl RegistrationRpt {
    pub const BODY_LEN: usize = 8 + 4 + 8 + 4 + 1 + 8 + 10 + 12 + 13 + 1 + 1 + 10 + 8 + 4 + 16 + 16 + 4 + 8 + 32;

    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.str(&self.pbu, 8);
        w.u32(self.set_id);
        w.u64(self.report_index);
        w.u32(self.biz_id);
        w.ch(self.exec_type);
        w.str(&self.biz_pbu, 8);
        w.str(&self.cl_ord_id, 10);
        w.str(&self.security_id, 12);
        w.str(&self.account, 13);
        w.u8(self.owner_type);
        w.ch(self.ord_status);
        w.str(&self.orig_cl_ord_id, 10);
        w.str(&self.branch_id, 8);
        w.u32(self.ord_rej_reason);
        w.str(&self.ord_cnfm_id, 16);
        w.str(&self.orig_ord_cnfm_id, 16);
        w.u32(self.trade_date);
        w.u64(self.transact_time);
        w.str(&self.user_info, 32);
        frame(msg_type::REGISTRATION_RPT, &w.into_inner())
    }
}

// ---------------------------------------------------------------------------
// 网络密码服务（4.5）：306 申报 / 308 申报响应
// ---------------------------------------------------------------------------

/// 网络密码服务申报（MsgType=306，4.5.1）。
///
/// Side 取值：'1'=激活 '2'=注销；SecurityID：A 股账户 799988、B 股账户 939988。
/// 该业务不进行重单校验，响应（308）不进执行报告流（无 Pbu/SetID/ReportIndex）。
#[derive(Debug, Clone, Default)]
pub struct PasswordServiceOrder {
    pub biz_id: u32,         // 业务编号，固定 300100
    pub biz_pbu: String,     // char[8] 业务 PBU 编号
    pub cl_ord_id: String,   // char[10] 会员内部订单编号
    pub security_id: String, // char[12] 证券代码（799988 A 股 / 939988 B 股）
    pub account: String,     // char[13] 证券账户
    pub owner_type: u8,      // 订单所有者类型，暂不启用
    pub transact_time: u64,  // ntime 申报时间
    pub branch_id: String,   // char[8] 营业部代码，暂不启用
    pub side: u8,            // '1'=激活 '2'=注销
    pub validation_code: String, // char[8] 投资者注册获得的激活码，仅 Side=1 时有意义
    pub user_info: String,   // char[32] 用户私有信息
}

impl PasswordServiceOrder {
    pub const BODY_LEN: usize = 4 + 8 + 10 + 12 + 13 + 1 + 8 + 8 + 1 + 8 + 32;

    pub fn decode(body: &[u8]) -> io::Result<Self> {
        let mut r = BodyReader::new(body);
        Ok(Self {
            biz_id: r.u32()?,
            biz_pbu: r.str(8)?,
            cl_ord_id: r.str(10)?,
            security_id: r.str(12)?,
            account: r.str(13)?,
            owner_type: r.u8()?,
            transact_time: r.u64()?,
            branch_id: r.str(8)?,
            side: r.ch()?,
            validation_code: r.str(8)?,
            user_info: r.str(32)?,
        })
    }
}

/// 网络密码服务申报响应（MsgType=308，4.5.2）。
///
/// 与 306 对称，但无 Pbu/SetID/ReportIndex——不进执行报告流
/// （表 3.2.1 注 2：申报响应消息不进执行报告）；OrdRejReason 成功时返回 0。
#[derive(Debug, Clone, Default)]
pub struct PasswordServiceRsp {
    pub biz_id: u32,            // 业务编号
    pub biz_pbu: String,        // char[8] 业务 PBU 编号
    pub cl_ord_id: String,      // char[10] 会员内部订单编号
    pub security_id: String,    // char[12] 证券代码
    pub account: String,        // char[13] 证券账户
    pub owner_type: u8,         // 订单所有者类型，暂不启用
    pub branch_id: String,      // char[8] 营业部代码，暂不启用
    pub side: u8,               // '1'=激活 '2'=注销
    pub validation_code: String, // char[8] 激活码
    pub ord_rej_reason: u32,    // 订单拒绝码，申报成功响应时返回 0
    pub trade_date: u32,        // date 交易日期
    pub transact_time: u64,     // ntime 回报时间
    pub user_info: String,      // char[32] 用户私有信息
}

impl PasswordServiceRsp {
    pub const BODY_LEN: usize = 4 + 8 + 10 + 12 + 13 + 1 + 8 + 1 + 8 + 4 + 4 + 8 + 32;

    pub fn encode(&self) -> Vec<u8> {
        let mut w = BodyWriter::new();
        w.u32(self.biz_id);
        w.str(&self.biz_pbu, 8);
        w.str(&self.cl_ord_id, 10);
        w.str(&self.security_id, 12);
        w.str(&self.account, 13);
        w.u8(self.owner_type);
        w.str(&self.branch_id, 8);
        w.ch(self.side);
        w.str(&self.validation_code, 8);
        w.u32(self.ord_rej_reason);
        w.u32(self.trade_date);
        w.u64(self.transact_time);
        w.str(&self.user_info, 32);
        frame(msg_type::PWD_SERVICE_RSP, &w.into_inner())
    }
}

/// 消息类型中文名（报文解析展示与日志用）
pub fn msg_type_name(mt: u32) -> &'static str {
    match mt {
        msg_type::LOGON => "登录 Logon",
        msg_type::LOGOUT => "注销 Logout",
        msg_type::HEARTBEAT => "心跳 Heartbeat",
        msg_type::NEW_ORDER => "新订单申报 NewOrderSingle",
        msg_type::CANCEL_ORDER => "撤单申报 OrderCancelRequest",
        msg_type::EXEC_RPT => "申报响应执行报告 ExecutionReport",
        msg_type::CANCEL_REJECT => "撤单失败 CancelReject",
        msg_type::TRADE => "成交执行报告 Trade",
        msg_type::ORDER_REJECT => "申报拒绝 OrderReject",
        msg_type::EXEC_RPT_SYNC => "分区序号同步 ExecRptSync",
        msg_type::EXEC_RPT_SYNC_RSP => "同步响应 ExecRptSyncRsp",
        msg_type::EXEC_RPT_INFO => "执行报告信息 ExecRptInfo",
        msg_type::PLATFORM_STATE => "平台状态 PlatformState",
        msg_type::EXEC_RPT_EOS => "回报结束 ExecRptEOS",
        msg_type::REGISTRATION => "注册处理申报 RegistrationOrder",
        msg_type::REGISTRATION_RPT => "注册处理执行回报 RegistrationRpt",
        msg_type::PWD_SERVICE => "网络密码服务申报 PasswordServiceOrder",
        msg_type::PWD_SERVICE_RSP => "网络密码服务申报响应 PasswordServiceRsp",
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

/// 沪市 ntime 时间戳 HHMMSSsssnnnn → “09:30:00.001”
fn fmt_sh_time(v: u64) -> String {
    if v == 0 {
        return "0".into();
    }
    let s = format!("{:013}", v);
    format!("{}:{}:{}.{}", &s[0..2], &s[2..4], &s[4..6], &s[6..9])
}

/// 沪市 date 日期 YYYYMMDD → “2026-07-31”
fn fmt_sh_date(v: u32) -> String {
    if v == 0 {
        return "0".into();
    }
    let s = format!("{:08}", v);
    format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
}

/// 放大整数 → 自然单位字符串（去尾零），如 1234000/100000 → “12.34”
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

/// 业务标识中文名（表 3.2.1 业务类型表）
fn biz_id_label(v: u32) -> String {
    match v {
        BIZ_ID_CASH_AUCTION => format!("{} (股票现货竞价)", v),
        BIZ_ID_ISSUE => format!("{} (发行)", v),
        BIZ_ID_RIGHTS => format!("{} (配股/科创板配售)", v),
        BIZ_ID_RIGHTS_BOND => format!("{} (配转债)", v),
        BIZ_ID_TENDER_ACCEPT => format!("{} (要约预受)", v),
        BIZ_ID_TENDER_CANCEL => format!("{} (要约撤销)", v),
        BIZ_ID_FUND_SUB => format!("{} (基金申购)", v),
        BIZ_ID_FUND_RED => format!("{} (基金赎回)", v),
        BIZ_ID_FUND_SUB_ISSUE => format!("{} (基金认购)", v),
        BIZ_ID_FUND_TRANSFER => format!("{} (转托管)", v),
        BIZ_ID_FUND_DIVIDEND => format!("{} (分红设置)", v),
        BIZ_ID_FUND_CONVERT => format!("{} (转换)", v),
        BIZ_ID_REMAIN_TRANSFER => format!("{} (余券划转)", v),
        BIZ_ID_RETURN_TRANSFER => format!("{} (还券划转)", v),
        BIZ_ID_COLLATERAL_IN => format!("{} (担保品划入)", v),
        BIZ_ID_COLLATERAL_OUT => format!("{} (担保品划出)", v),
        BIZ_ID_SEC_SRC_IN => format!("{} (券源划入)", v),
        BIZ_ID_SEC_SRC_OUT => format!("{} (券源划出)", v),
        BIZ_ID_PWD_SERVICE => format!("{} (网络密码服务)", v),
        BIZ_ID_DESIGNATION => format!("{} (指定登记)", v),
        BIZ_ID_DESIGNATION_CANCEL => format!("{} (指定撤销)", v),
        other => other.to_string(),
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

/// 订单类别：1=市转撤 2=限价 3=市转限 4=本方最优 5=对手方最优
fn ord_type_label(b: u8) -> String {
    match b {
        b'1' => "1 (市转撤)".into(),
        b'2' => "2 (限价)".into(),
        b'3' => "3 (市转限)".into(),
        b'4' => "4 (本方最优)".into(),
        b'5' => "5 (对手方最优)".into(),
        b => (b as char).to_string(),
    }
}

/// 执行类型：0=申报确认 4=撤单成功 8=申报拒绝 F=成交
fn exec_type_label(b: u8) -> String {
    match b {
        b'0' => "0 (申报确认)".into(),
        b'4' => "4 (撤单成功)".into(),
        b'8' => "8 (申报拒绝)".into(),
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

/// 实际逐字段读取逻辑：按消息类型分派，读失败（长度不符）即整体放弃
fn describe_body(mt: u32, body: &[u8]) -> io::Result<Vec<ParsedField>> {
    let mut r = BodyReader::new(body);
    let mut out = vec![f("MsgType", format!("{} ({})", msg_type_name(mt), mt))];
    match mt {
        msg_type::LOGON => {
            out.push(f("SenderCompID", r.str(32)?));
            out.push(f("TargetCompID", r.str(32)?));
            out.push(f("HeartBtInt", format!("{} 秒", r.u16()?)));
            out.push(f("PrtclVersion", r.str(8)?));
            out.push(f("TradeDate", fmt_sh_date(r.u32()?)));
            out.push(f("Qsize", r.u32()?));
        }
        msg_type::LOGOUT => {
            out.push(f("SessionStatus", r.u32()?));
            out.push(f("Text", r.str(64)?));
        }
        msg_type::HEARTBEAT => {}
        msg_type::NEW_ORDER => {
            let biz_id = r.u32()?;
            out.push(f("BizID", biz_id_label(biz_id)));
            out.push(f("BizPBU", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("SecurityID", r.str(12)?));
            out.push(f("Account", r.str(13)?));
            out.push(f("OwnerType", r.u8()?));
            out.push(f("Side", side_label(r.ch()?)));
            out.push(f("Price", format!("{} 元", fmt_scaled(r.i64()?, 100_000))));
            out.push(f("OrderQty", format!("{} 股", fmt_scaled(r.i64()?, 1000))));
            out.push(f("OrdType", ord_type_label(r.ch()?)));
            out.push(f("TimeInForce", r.ch()?));
            out.push(f("TransactTime", fmt_sh_time(r.u64()?)));
            out.push(f("CreditTag", r.str(2)?));
            out.push(f("ClearingFirm", r.str(8)?));
            out.push(f("BranchID", r.str(8)?));
            out.push(f("UserInfo", r.str(32)?));
            // 4.3.1.1~4.3.1.7 各业务扩展字段（容忍缺失）
            match biz_id {
                BIZ_ID_FUND_TRANSFER => {
                    if r.remaining() >= 3 {
                        out.push(f("Custodian", r.str(3)?));
                    }
                }
                BIZ_ID_FUND_DIVIDEND => {
                    if r.remaining() >= 1 {
                        out.push(f("DividendSelect", r.ch()?));
                    }
                }
                BIZ_ID_FUND_CONVERT
                | BIZ_ID_REMAIN_TRANSFER
                | BIZ_ID_RETURN_TRANSFER
                | BIZ_ID_COLLATERAL_IN
                | BIZ_ID_COLLATERAL_OUT
                | BIZ_ID_SEC_SRC_IN
                | BIZ_ID_SEC_SRC_OUT => {
                    if r.remaining() >= 12 {
                        out.push(f("DestSecurity", r.str(12)?));
                    }
                }
                _ => {}
            }
        }
        msg_type::CANCEL_ORDER => {
            out.push(f("BizID", biz_id_label(r.u32()?)));
            out.push(f("BizPBU", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("SecurityID", r.str(12)?));
            out.push(f("Account", r.str(13)?));
            out.push(f("OwnerType", r.u8()?));
            out.push(f("Side", side_label(r.ch()?)));
            out.push(f("OrigClOrdID", r.str(10)?));
            out.push(f("TransactTime", fmt_sh_time(r.u64()?)));
            out.push(f("BranchID", r.str(8)?));
            out.push(f("UserInfo", r.str(32)?));
        }
        msg_type::EXEC_RPT => {
            out.push(f("PBU", r.str(8)?));
            out.push(f("SetID", r.u32()?));
            out.push(f("ReportIndex", r.u64()?));
            let biz_id = r.u32()?;
            out.push(f("BizID", biz_id_label(biz_id)));
            out.push(f("ExecType", exec_type_label(r.ch()?)));
            out.push(f("BizPBU", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("SecurityID", r.str(12)?));
            out.push(f("Account", r.str(13)?));
            out.push(f("OwnerType", r.u8()?));
            out.push(f("Side", side_label(r.ch()?)));
            out.push(f("Price", format!("{} 元", fmt_scaled(r.i64()?, 100_000))));
            out.push(f("OrderQty", format!("{} 股", fmt_scaled(r.i64()?, 1000))));
            out.push(f("LeavesQty", format!("{} 股", fmt_scaled(r.i64()?, 1000))));
            out.push(f("CxlQty", format!("{} 股", fmt_scaled(r.i64()?, 1000))));
            out.push(f("OrdType", ord_type_label(r.ch()?)));
            out.push(f("TimeInForce", r.ch()?));
            out.push(f("OrdStatus", ord_status_label(r.ch()?)));
            out.push(f("CreditTag", r.str(2)?));
            out.push(f("OrigClOrdID", r.str(10)?));
            out.push(f("ClearingFirm", r.str(8)?));
            out.push(f("BranchID", r.str(8)?));
            out.push(f("OrdRejReason", r.u32()?));
            out.push(f("OrdCnfmID", r.str(16)?));
            out.push(f("OrigOrdCnfmID", r.str(16)?));
            out.push(f("TradeDate", fmt_sh_date(r.u32()?)));
            out.push(f("TransactTime", fmt_sh_time(r.u64()?)));
            out.push(f("UserInfo", r.str(32)?));
            // 4.3.3.1 说明 2：扩展字段与新订单对应业务一致（容忍缺失）
            match biz_id {
                BIZ_ID_FUND_TRANSFER => {
                    if r.remaining() >= 3 {
                        out.push(f("Custodian", r.str(3)?));
                    }
                }
                BIZ_ID_FUND_DIVIDEND => {
                    if r.remaining() >= 1 {
                        out.push(f("DividendSelect", r.ch()?));
                    }
                }
                BIZ_ID_FUND_CONVERT
                | BIZ_ID_REMAIN_TRANSFER
                | BIZ_ID_RETURN_TRANSFER
                | BIZ_ID_COLLATERAL_IN
                | BIZ_ID_COLLATERAL_OUT
                | BIZ_ID_SEC_SRC_IN
                | BIZ_ID_SEC_SRC_OUT => {
                    if r.remaining() >= 12 {
                        out.push(f("DestSecurity", r.str(12)?));
                    }
                }
                _ => {}
            }
        }
        msg_type::CANCEL_REJECT => {
            out.push(f("PBU", r.str(8)?));
            out.push(f("SetID", r.u32()?));
            out.push(f("ReportIndex", r.u64()?));
            out.push(f("BizID", biz_id_label(r.u32()?)));
            out.push(f("BizPBU", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("SecurityID", r.str(12)?));
            out.push(f("OrigClOrdID", r.str(10)?));
            out.push(f("BranchID", r.str(8)?));
            out.push(f("CxlRejReason", r.u32()?));
            out.push(f("TradeDate", fmt_sh_date(r.u32()?)));
            out.push(f("TransactTime", fmt_sh_time(r.u64()?)));
            out.push(f("UserInfo", r.str(32)?));
        }
        msg_type::TRADE => {
            out.push(f("PBU", r.str(8)?));
            out.push(f("SetID", r.u32()?));
            out.push(f("ReportIndex", r.u64()?));
            out.push(f("BizID", biz_id_label(r.u32()?)));
            out.push(f("ExecType", exec_type_label(r.ch()?)));
            out.push(f("BizPBU", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("SecurityID", r.str(12)?));
            out.push(f("Account", r.str(13)?));
            out.push(f("OwnerType", r.u8()?));
            out.push(f("OrderEntryTime", fmt_sh_time(r.u64()?)));
            out.push(f("LastPx", format!("{} 元", fmt_scaled(r.i64()?, 100_000))));
            out.push(f("LastQty", format!("{} 股", fmt_scaled(r.i64()?, 1000))));
            out.push(f("GrossTradeAmt", format!("{} 元", fmt_scaled(r.i64()?, 100_000))));
            out.push(f("Side", side_label(r.ch()?)));
            out.push(f("OrderQty", format!("{} 股", fmt_scaled(r.i64()?, 1000))));
            out.push(f("LeavesQty", format!("{} 股", fmt_scaled(r.i64()?, 1000))));
            out.push(f("OrdStatus", ord_status_label(r.ch()?)));
            out.push(f("CreditTag", r.str(2)?));
            out.push(f("ClearingFirm", r.str(8)?));
            out.push(f("BranchID", r.str(8)?));
            out.push(f("TrdCnfmID", r.str(16)?));
            out.push(f("OrdCnfmID", r.str(16)?));
            out.push(f("TradeDate", fmt_sh_date(r.u32()?)));
            out.push(f("TransactTime", fmt_sh_time(r.u64()?)));
            out.push(f("UserInfo", r.str(32)?));
        }
        msg_type::ORDER_REJECT => {
            out.push(f("BizID", biz_id_label(r.u32()?)));
            out.push(f("BizPBU", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("SecurityID", r.str(12)?));
            out.push(f("OrdRejReason", r.u32()?));
            out.push(f("TradeDate", fmt_sh_date(r.u32()?)));
            out.push(f("TransactTime", fmt_sh_time(r.u64()?)));
            out.push(f("UserInfo", r.str(32)?));
        }
        msg_type::EXEC_RPT_SYNC => {
            let n = r.u16()?;
            out.push(f("NoGroups", n));
            for i in 0..n {
                out.push(f(format!("Group[{}].PBU", i + 1), r.str(8)?));
                out.push(f(format!("Group[{}].SetID", i + 1), r.u32()?));
                out.push(f(format!("Group[{}].BeginReportIndex", i + 1), r.u64()?));
            }
        }
        msg_type::EXEC_RPT_SYNC_RSP => {
            let n = r.u16()?;
            out.push(f("NoGroups", n));
            for i in 0..n {
                out.push(f(format!("Group[{}].PBU", i + 1), r.str(8)?));
                out.push(f(format!("Group[{}].SetID", i + 1), r.u32()?));
                out.push(f(format!("Group[{}].BeginReportIndex", i + 1), r.u64()?));
                out.push(f(format!("Group[{}].EndReportIndex", i + 1), r.u64()?));
                out.push(f(format!("Group[{}].RejReason", i + 1), r.u32()?));
                out.push(f(format!("Group[{}].Text", i + 1), r.str(64)?));
            }
        }
        msg_type::EXEC_RPT_INFO => {
            out.push(f("PlatformID", r.u16()?));
            let np = r.u16()?;
            out.push(f("NoPBUs", np));
            for i in 0..np {
                out.push(f(format!("PBU[{}]", i + 1), r.str(8)?));
                let ns = r.u16()?;
                out.push(f(format!("PBU[{}].NoSetIDs", i + 1), ns));
                for j in 0..ns {
                    out.push(f(format!("PBU[{}].SetID[{}]", i + 1, j + 1), r.u32()?));
                }
            }
        }
        msg_type::PLATFORM_STATE => {
            out.push(f("PlatformID", r.u16()?));
            out.push(f("State", r.u16()?));
        }
        msg_type::EXEC_RPT_EOS => {
            out.push(f("PBU", r.str(8)?));
            out.push(f("SetID", r.u32()?));
            out.push(f("EndReportIndex", r.u64()?));
        }
        msg_type::REGISTRATION => {
            out.push(f("BizID", biz_id_label(r.u32()?)));
            out.push(f("BizPBU", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("SecurityID", r.str(12)?));
            out.push(f("Account", r.str(13)?));
            out.push(f("OwnerType", r.u8()?));
            out.push(f("DesignationInstruction", r.ch()?));
            out.push(f("DesignationTransType", r.ch()?));
            out.push(f("OrigClOrdID", r.str(10)?));
            out.push(f("TransactTime", fmt_sh_time(r.u64()?)));
            out.push(f("BranchID", r.str(8)?));
            out.push(f("UserInfo", r.str(32)?));
        }
        msg_type::REGISTRATION_RPT => {
            out.push(f("PBU", r.str(8)?));
            out.push(f("SetID", r.u32()?));
            out.push(f("ReportIndex", r.u64()?));
            out.push(f("BizID", biz_id_label(r.u32()?)));
            out.push(f("ExecType", exec_type_label(r.ch()?)));
            out.push(f("BizPBU", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("SecurityID", r.str(12)?));
            out.push(f("Account", r.str(13)?));
            out.push(f("OwnerType", r.u8()?));
            out.push(f("OrdStatus", ord_status_label(r.ch()?)));
            out.push(f("OrigClOrdID", r.str(10)?));
            out.push(f("BranchID", r.str(8)?));
            out.push(f("OrdRejReason", r.u32()?));
            out.push(f("OrdCnfmID", r.str(16)?));
            out.push(f("OrigOrdCnfmID", r.str(16)?));
            out.push(f("TradeDate", fmt_sh_date(r.u32()?)));
            out.push(f("TransactTime", fmt_sh_time(r.u64()?)));
            out.push(f("UserInfo", r.str(32)?));
        }
        msg_type::PWD_SERVICE => {
            out.push(f("BizID", biz_id_label(r.u32()?)));
            out.push(f("BizPBU", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("SecurityID", r.str(12)?));
            out.push(f("Account", r.str(13)?));
            out.push(f("OwnerType", r.u8()?));
            out.push(f("TransactTime", fmt_sh_time(r.u64()?)));
            out.push(f("BranchID", r.str(8)?));
            out.push(f("Side", r.ch()?));
            out.push(f("ValidationCode", r.str(8)?));
            out.push(f("UserInfo", r.str(32)?));
        }
        msg_type::PWD_SERVICE_RSP => {
            out.push(f("BizID", biz_id_label(r.u32()?)));
            out.push(f("BizPBU", r.str(8)?));
            out.push(f("ClOrdID", r.str(10)?));
            out.push(f("SecurityID", r.str(12)?));
            out.push(f("Account", r.str(13)?));
            out.push(f("OwnerType", r.u8()?));
            out.push(f("BranchID", r.str(8)?));
            out.push(f("Side", r.ch()?));
            out.push(f("ValidationCode", r.str(8)?));
            out.push(f("OrdRejReason", r.u32()?));
            out.push(f("TradeDate", fmt_sh_date(r.u32()?)));
            out.push(f("TransactTime", fmt_sh_time(r.u64()?)));
            out.push(f("UserInfo", r.str(32)?));
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
        w.u32(BIZ_ID_CASH_AUCTION);
        w.str("PBU00001", 8);
        w.str("A000000001", 10);
        w.str("600000", 12);
        w.str("B880000001", 13);
        w.u8(1);
        w.ch(b'1');
        w.i64(12_34000); // 12.34 元（放大 10 万倍）
        w.i64(100_000); // 100 股（放大 1000 倍）
        w.ch(b'2');
        w.ch(b'0');
        w.u64(930_000_010_000); // 09:30:00.001（HHMMSSsssnnnn 共 13 位）
        w.str("", 2);
        w.str("CF01", 8);
        w.str("BR01", 8);
        w.str("UINFO", 32);
        let body = w.into_inner();
        let fields = describe_fields(msg_type::NEW_ORDER, &body);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"SecurityID"));
        assert!(names.contains(&"ClOrdID"));
        // 数量/价格换算自然单位并带单位
        let qty = fields.iter().find(|f| f.name == "OrderQty").unwrap();
        assert_eq!(qty.value, "100 股");
        let px = fields.iter().find(|f| f.name == "Price").unwrap();
        assert_eq!(px.value, "12.34 元");
        // 时间戳转可读时间
        let tt = fields.iter().find(|f| f.name == "TransactTime").unwrap();
        assert_eq!(tt.value, "09:30:00.001");
    }

    #[test]
    fn test_checksum_wrapping() {
        assert_eq!(checksum(&[1, 2, 3]), 6);
        // uint8 自然溢出：255 + 255 = 510 → 510 mod 256 = 254
        assert_eq!(checksum(&[255, 255]), 254);
    }

    #[test]
    fn test_frame_header_and_finalize_seq() {
        let body = vec![0xAAu8, 0xBB];
        let mut f = frame(msg_type::HEARTBEAT, &body);
        // 头 16 字节：MsgType + MsgSeqNum(占位0) + MsgBodyLen
        assert_eq!(&f[0..4], &33u32.to_be_bytes());
        assert_eq!(&f[4..12], &0u64.to_be_bytes());
        assert_eq!(&f[12..16], &2u32.to_be_bytes());
        let n = f.len();
        assert_eq!(n, 16 + 2 + 4);
        // 补序号后校验和仍然自洽
        finalize_seq(&mut f, 7);
        assert_eq!(&f[4..12], &7u64.to_be_bytes());
        let cks = u32::from_be_bytes(f[n - 4..].try_into().unwrap());
        assert_eq!(cks, checksum(&f[..n - 4]));
    }

    #[test]
    fn test_logon_roundtrip() {
        let l = Logon {
            sender_comp_id: "TDGW".into(),
            target_comp_id: "OMS001".into(),
            heart_bt_int: 30,
            prtcl_version: "0.50".into(),
            trade_date: 20260731,
            qsize: 32,
        };
        let f = l.encode();
        let body = &f[16..f.len() - 4];
        assert_eq!(body.len(), Logon::BODY_LEN);
        let d = Logon::decode(body).unwrap();
        assert_eq!(d.sender_comp_id, "TDGW");
        assert_eq!(d.heart_bt_int, 30);
        assert_eq!(d.prtcl_version, "0.50");
        assert_eq!(d.trade_date, 20260731);
    }

    #[test]
    fn test_new_order_decode() {
        let mut w = BodyWriter::new();
        w.u32(BIZ_ID_CASH_AUCTION);
        w.str("PBU00001", 8);
        w.str("A000000001", 10);
        w.str("600000", 12);
        w.str("B880000001", 13);
        w.u8(1);
        w.ch(b'1');
        w.i64(12_34000); // 12.34 元（放大 10 万倍）
        w.i64(100_000); // 100 股（放大 1000 倍）
        w.ch(b'2');
        w.ch(b'0');
        w.u64(930000001_0000);
        w.str("", 2);
        w.str("CF01", 8);
        w.str("BR01", 8);
        w.str("UINFO", 32);
        let body = w.into_inner();
        assert_eq!(body.len(), NewOrder::BODY_LEN);
        let o = NewOrder::decode(&body).unwrap();
        assert_eq!(o.biz_id, BIZ_ID_CASH_AUCTION);
        assert_eq!(o.cl_ord_id, "A000000001");
        assert_eq!(o.security_id, "600000");
        assert_eq!(o.side, b'1');
        assert_eq!(o.price, 12_34000);
        assert_eq!(o.order_qty, 100_000);
        assert_eq!(o.user_info, "UINFO");
    }

    #[test]
    fn test_cancel_order_decode() {
        let mut w = BodyWriter::new();
        w.u32(BIZ_ID_CASH_AUCTION);
        w.str("PBU00001", 8);
        w.str("A000000002", 10);
        w.str("600000", 12);
        w.str("B880000001", 13);
        w.u8(1);
        w.ch(b'1');
        w.str("A000000001", 10);
        w.u64(931000001_0000);
        w.str("BR01", 8);
        w.str("", 32);
        let body = w.into_inner();
        assert_eq!(body.len(), CancelOrder::BODY_LEN);
        let c = CancelOrder::decode(&body).unwrap();
        assert_eq!(c.cl_ord_id, "A000000002");
        assert_eq!(c.orig_cl_ord_id, "A000000001");
    }

    #[test]
    fn test_exec_rpt_body_len() {
        let f = ExecRpt::default().encode();
        // 213 字节消息体 + 16 头 + 4 尾
        assert_eq!(f.len(), 213 + 20);
    }

    #[test]
    fn test_sync_roundtrip() {
        // OMS 发来的同步请求
        let mut w = BodyWriter::new();
        w.u16(1);
        w.str("PBU00001", 8);
        w.u32(1);
        w.u64(1);
        let groups = decode_exec_rpt_sync(&w.into_inner()).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].pbu, "PBU00001");
        assert_eq!(groups[0].begin_report_index, 1);
        // 我们回的响应帧长度：头16 + 2 + (8+4+8+8+4+64) + 尾4
        let rsp = encode_exec_rpt_sync_rsp(&[SyncRspGroup {
            pbu: "PBU00001".into(),
            set_id: 1,
            begin_report_index: 1,
            end_report_index: 0,
            rej_reason: 0,
            text: "OK".into(),
        }]);
        assert_eq!(rsp.len(), 16 + 2 + 96 + 4);
    }

    #[test]
    fn test_exec_rpt_info_nested() {
        // 4.6.3 嵌套结构：PlatformID + NoPBUs{Pbu + NoSetIDs{SetID}}
        let f = encode_exec_rpt_info(0, &[("PBU00001", &[1, 2]), ("PBU00002", &[992])]);
        let body = &f[16..f.len() - 4];
        let mut r = BodyReader::new(body);
        assert_eq!(r.u16().unwrap(), 0); // PlatformID
        assert_eq!(r.u16().unwrap(), 2); // NoPBUs
        assert_eq!(r.str(8).unwrap(), "PBU00001");
        assert_eq!(r.u16().unwrap(), 2); // 第一个 PBU 下 2 个 SetID
        assert_eq!(r.u32().unwrap(), 1);
        assert_eq!(r.u32().unwrap(), 2);
        assert_eq!(r.str(8).unwrap(), "PBU00002");
        assert_eq!(r.u16().unwrap(), 1); // 第二个 PBU 下 1 个 SetID
        assert_eq!(r.u32().unwrap(), 992);
        assert_eq!(r.remaining(), 0);
        // 解析展示也能读出嵌套字段
        let fields = describe_fields(msg_type::EXEC_RPT_INFO, body);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"PBU[1].NoSetIDs"));
        assert!(names.contains(&"PBU[1].SetID[2]"));
        assert!(names.contains(&"PBU[2].SetID[1]"));
    }

    #[test]
    fn test_extend_fields_roundtrip() {
        // 转托管 300060：Custodian char[3]
        let mut w = BodyWriter::new();
        w.u32(BIZ_ID_FUND_TRANSFER);
        w.str("PBU00001", 8);
        w.str("A000000001", 10);
        w.str("519888", 12);
        w.str("B880000001", 13);
        w.u8(1);
        w.ch(b'1');
        w.i64(0);
        w.i64(100_000);
        w.ch(b'2');
        w.ch(b'0');
        w.u64(930000001_0000);
        w.str("", 2);
        w.str("CF01", 8);
        w.str("BR01", 8);
        w.str("UINFO", 32);
        w.str("123", 3); // Custodian
        let body = w.into_inner();
        assert_eq!(body.len(), NewOrder::BODY_LEN + 3);
        let o = NewOrder::decode(&body).unwrap();
        assert_eq!(o.extend.custodian, "123");
        // 容忍缺失：只发公共字段也不报错
        let o2 = NewOrder::decode(&body[..NewOrder::BODY_LEN]).unwrap();
        assert_eq!(o2.extend.custodian, "");
        // 分红设置 300070：DividendSelect char
        let mut w = BodyWriter::new();
        w.u32(BIZ_ID_FUND_DIVIDEND);
        w.str("PBU00001", 8);
        w.str("A000000001", 10);
        w.str("519888", 12);
        w.str("B880000001", 13);
        w.u8(1);
        w.ch(b'1');
        w.i64(0);
        w.i64(100_000);
        w.ch(b'2');
        w.ch(b'0');
        w.u64(930000001_0000);
        w.str("", 2);
        w.str("CF01", 8);
        w.str("BR01", 8);
        w.str("UINFO", 32);
        w.ch(b'U'); // 红利转投
        let o3 = NewOrder::decode(&w.into_inner()).unwrap();
        assert_eq!(o3.extend.dividend_select, b'U');
        // 转换 300080：DestSecurity char[12]
        let mut w = BodyWriter::new();
        w.u32(BIZ_ID_FUND_CONVERT);
        w.str("PBU00001", 8);
        w.str("A000000001", 10);
        w.str("519888", 12);
        w.str("B880000001", 13);
        w.u8(1);
        w.ch(b'1');
        w.i64(0);
        w.i64(100_000);
        w.ch(b'2');
        w.ch(b'0');
        w.u64(930000001_0000);
        w.str("", 2);
        w.str("CF01", 8);
        w.str("BR01", 8);
        w.str("UINFO", 32);
        w.str("510050", 12); // 目标基金代码
        let o4 = NewOrder::decode(&w.into_inner()).unwrap();
        assert_eq!(o4.extend.dest_security, "510050");
        // ExecRpt 扩展字段回填（4.3.3.1 说明 2）
        let mut e = ExecRpt::default();
        e.biz_id = BIZ_ID_FUND_TRANSFER;
        e.extend.custodian = "123".into();
        let f = e.encode();
        let body = &f[16..f.len() - 4];
        assert_eq!(body.len(), 213 + 3);
        let fields = describe_fields(msg_type::EXEC_RPT, body);
        let cs = fields.iter().find(|x| x.name == "Custodian").unwrap();
        assert_eq!(cs.value, "123");
    }

    #[test]
    fn test_registration_roundtrip() {
        // 301 指定登记申报（4.4.1）：SecurityID=799999 指令'1' 类型'1'
        let mut w = BodyWriter::new();
        w.u32(BIZ_ID_DESIGNATION);
        w.str("PBU00001", 8);
        w.str("D000000001", 10);
        w.str("799999", 12);
        w.str("B880000001", 13);
        w.u8(1);
        w.ch(b'1'); // 指定交易登记
        w.ch(b'1'); // 新注册请求
        w.str("", 10);
        w.u64(930000001_0000);
        w.str("", 8);
        w.str("UINFO", 32);
        let body = w.into_inner();
        assert_eq!(body.len(), RegistrationOrder::BODY_LEN);
        let d = RegistrationOrder::decode(&body).unwrap();
        assert_eq!(d.biz_id, BIZ_ID_DESIGNATION);
        assert_eq!(d.security_id, "799999");
        assert_eq!(d.designation_instruction, b'1');
        assert_eq!(d.designation_trans_type, b'1');
        let fields = describe_fields(msg_type::REGISTRATION, &body);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"DesignationInstruction"));
        // 302 执行回报：168 字节消息体 + 16 头 + 4 尾
        let r = RegistrationRpt {
            pbu: "PBU00001".into(),
            set_id: SET_ID_DESIGNATION,
            report_index: 1,
            biz_id: BIZ_ID_DESIGNATION,
            exec_type: b'0',
            biz_pbu: "PBU00001".into(),
            cl_ord_id: "D000000001".into(),
            security_id: "799999".into(),
            account: "B880000001".into(),
            owner_type: 1,
            ord_status: b'0',
            ord_cnfm_id: "0000000000000001".into(),
            trade_date: 20260814,
            transact_time: 930000001_0000,
            user_info: "UINFO".into(),
            ..Default::default()
        };
        let f = r.encode();
        assert_eq!(f.len(), RegistrationRpt::BODY_LEN + 20);
        let fields = describe_fields(msg_type::REGISTRATION_RPT, &f[16..f.len() - 4]);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"OrdCnfmID"));
        assert!(names.contains(&"ExecType"));
    }

    #[test]
    fn test_password_service_roundtrip() {
        // 306 激活申报（4.5.1）：SecurityID=799988、Side='1'、带激活码
        let mut w = BodyWriter::new();
        w.u32(BIZ_ID_PWD_SERVICE);
        w.str("PBU00001", 8);
        w.str("P000000001", 10);
        w.str("799988", 12);
        w.str("B880000001", 13);
        w.u8(1);
        w.u64(930000001_0000);
        w.str("", 8);
        w.ch(b'1'); // 激活
        w.str("ABCD1234", 8); // 激活码
        w.str("UINFO", 32);
        let body = w.into_inner();
        assert_eq!(body.len(), PasswordServiceOrder::BODY_LEN);
        let d = PasswordServiceOrder::decode(&body).unwrap();
        assert_eq!(d.biz_id, BIZ_ID_PWD_SERVICE);
        assert_eq!(d.security_id, "799988");
        assert_eq!(d.side, b'1');
        assert_eq!(d.validation_code, "ABCD1234");
        let fields = describe_fields(msg_type::PWD_SERVICE, &body);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"ValidationCode"));
        // 308 申报响应：113 字节消息体 + 16 头 + 4 尾，不进执行报告流
        let r = PasswordServiceRsp {
            biz_id: BIZ_ID_PWD_SERVICE,
            biz_pbu: "PBU00001".into(),
            cl_ord_id: "P000000001".into(),
            security_id: "799988".into(),
            account: "B880000001".into(),
            owner_type: 1,
            branch_id: String::new(),
            side: b'1',
            validation_code: "ABCD1234".into(),
            ord_rej_reason: 0,
            trade_date: 20260814,
            transact_time: 930000001_0000,
            user_info: "UINFO".into(),
        };
        let f = r.encode();
        assert_eq!(f.len(), PasswordServiceRsp::BODY_LEN + 20);
        let fields = describe_fields(msg_type::PWD_SERVICE_RSP, &f[16..f.len() - 4]);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"OrdRejReason"));
    }
}
