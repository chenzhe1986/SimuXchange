//! 上海综合业务平台 EzSTEP 协议的 STEP 消息编解码。
//!
//! # 消息长什么样
//!
//! 参考柜台报盘代码 `fix_impl.cpp` 的打包逻辑，STEP 消息是 FIX 风格的
//! **纯 ASCII** 文本（没有 TCP 头/校验和，因为走数据库字段传输）：
//!
//! ```text
//! 9=123<SOH>35=D<SOH>11=ORDER00001<SOH>48=510300<SOH>38=1000<SOH>...
//! ```
//!
//! - `SOH` = 0x01，字段分隔符；
//! - `9=` 是消息体长度：**9= 字段自身之后**的字节数（含 35= 及后续所有字段）；
//! - `35=` 是消息类型：D=新委托、F=撤单、8=执行报告、9=撤单拒绝；
//! - 空字符串字段写成 `标签= <SOH>`（等号后一个空格），数字空值写 0；
//! - 标签编号沿用 FIX（11=ClOrdID、48=SecurityID、38=OrderQty、44=Price…），
//!   完整定义见参考代码 `tag_step.h` 与 `EzStepTemplate.xml`。
//!
//! # 本模块职责
//!
//! - 把数据库里的 `reqtext` / `resptext` / `execreporttext` 解析成
//!   [`StepMessage`]（保留原始标签顺序，重复组也能按顺序还原）；
//! - 把柜台委托解析成 [`StepOrder`]（本模拟器关心的公共字段）；
//! - 构造确认/成交/拒单/撤单成功/撤单拒绝五类回报；
//! - 把消息转成报文弹窗用的 [`ParsedField`]（字段名 + 可读值）。
//!
//! 不校验 `9=` 的长度值（参考柜台解析器也只把它当普通字段消费），
//! 但构造消息时按规范正确填写，保证真实 EzSTEP 网关也能接收。

use crate::capture::ParsedField;

/// SOH 字段分隔符（FIX 系协议沿用 0x01）
pub const SOH: char = '\x01';

/// 消息类型：新委托
pub const MSG_NEW_ORDER: &str = "D";
/// 消息类型：撤单请求
pub const MSG_CANCEL: &str = "F";
/// 消息类型：执行报告（申报响应/成交回报）
pub const MSG_EXEC_REPORT: &str = "8";
/// 消息类型：撤单拒绝
pub const MSG_CANCEL_REJECT: &str = "9";

/// STEP 标签编号（只列本模块用到的；编号来自参考代码 tag_step.h）
pub mod tag {
    pub const BODY_LENGTH: u32 = 9;
    pub const MSG_TYPE: u32 = 35;
    pub const CL_ORD_ID: u32 = 11;
    pub const ORDER_ID: u32 = 37;
    pub const ORDER_QTY: u32 = 38;
    pub const ORD_STATUS: u32 = 39;
    pub const ORD_TYPE: u32 = 40;
    pub const ORIG_CL_ORD_ID: u32 = 41;
    pub const ORIG_TIME: u32 = 42;
    pub const PRICE: u32 = 44;
    pub const SECURITY_ID: u32 = 48;
    pub const SIDE: u32 = 54;
    pub const TEXT: u32 = 58;
    pub const TIME_IN_FORCE: u32 = 59;
    pub const TRANSACT_TIME: u32 = 60;
    pub const POSITION_EFFECT: u32 = 77;
    pub const CXL_REJ_REASON: u32 = 102;
    pub const ORD_REJ_REASON: u32 = 103;
    pub const EXEC_TYPE: u32 = 150;
    pub const LEAVES_QTY: u32 = 151;
    pub const EXEC_ID: u32 = 17;
    pub const LAST_PX: u32 = 31;
    pub const LAST_QTY: u32 = 32;
    pub const COVERED_OR_UNCOVERED: u32 = 203;
    pub const UNDERLYING_SECURITY_ID: u32 = 308;
    pub const SECURITY_ALT_ID: u32 = 455;
    pub const CXL_REJ_RESPONSE_TO: u32 = 434;
    pub const PARTY_ID: u32 = 448;
    pub const PARTY_ROLE: u32 = 452;
    pub const NO_PARTY_IDS: u32 = 453;
    pub const OWNER_TYPE: u32 = 522;
    pub const NO_LEGS: u32 = 555;
    pub const NO_DATES: u32 = 580;
    pub const PRICE2: u32 = 640;
    pub const TRADE_REPORT_TYPE: u32 = 856;
    pub const NUM_TRADES: u32 = 8503;
    pub const TOTAL_VALUE_TRADED: u32 = 8504;
    pub const MARGIN_AMT_TYPE: u32 = 1644;
    pub const NO_PARTY_DETAILS: u32 = 1671;
}

/// 执行类型（150）取值（参考柜台解析器：0=已报，8=拒绝，6=已撤，F=成交）
pub mod exec_type {
    pub const NEW: char = '0';
    pub const REJECT: char = '8';
    pub const CANCELED: char = '6';
    pub const TRADE: char = 'F';
}

/// 订单状态（39）取值（注意本协议撤单成功用 6，而非 FIX 标准的 4）
pub mod ord_status {
    pub const NEW: char = '0';
    pub const PARTIAL: char = '1';
    pub const FILLED: char = '2';
    pub const CANCELED: char = '6';
    pub const REJECTED: char = '8';
}

/// 参与方角色（452）：5=投资者证券账户，1=业务 PBU/席位，4001=营业部
pub mod party_role {
    pub const ACCOUNT: i32 = 5;
    pub const PBU: i32 = 1;
    pub const BRANCH: i32 = 4001;
}

/// 一条 STEP 消息：消息类型 + 按原始顺序保存的 (标签, 值) 列表。
///
/// 用 Vec 而不是 HashMap，是因为报文里同一个标签会出现多次
/// （同一字段的重复组，例如 448/452 参与方），顺序本身有语义。
#[derive(Debug, Clone)]
pub struct StepMessage {
    pub msg_type: String,
    pub fields: Vec<(u32, String)>,
}

impl StepMessage {
    /// 解析数据库字段里的 STEP 文本。
    ///
    /// 容错策略：
    /// - 忽略首尾 NUL/换行/空格（数据库 char 字段可能补空格）；
    /// - 没有 `35=` 或类型为空时返回错误（无法判断怎么处理）；
    /// - 其它畸形字段（没有等号、标签不是数字）跳过，不因单个坏字段丢掉整条消息。
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim_end_matches(['\0', '\r', '\n']);
        let text = text.trim_end_matches(' ');
        if text.is_empty() {
            return Err("STEP 消息为空".into());
        }
        let mut msg_type = String::new();
        let mut fields = Vec::new();
        for part in text.split(SOH) {
            let part = part.trim_end_matches('\0');
            if part.is_empty() {
                continue;
            }
            let Some((tag_str, value)) = part.split_once('=') else {
                continue;
            };
            let Ok(t) = tag_str.trim().parse::<u32>() else {
                continue;
            };
            // 规范约定空字符串写作 "标签= "（等号后一个空格）；这里还原成空串，
            // 其余值原样保留（Text 等字段可能合法地含前导/尾随空格）
            let value = if value == " " { "" } else { value };
            match t {
                tag::BODY_LENGTH => {
                    // 长度字段只用于传输层定位，解析时不校验（参考柜台实现同样忽略）
                }
                tag::MSG_TYPE => msg_type = value.to_string(),
                _ => fields.push((t, value.to_string())),
            }
        }
        if msg_type.is_empty() {
            return Err("STEP 消息缺少 35=(MsgType)".into());
        }
        Ok(Self { msg_type, fields })
    }

    /// 取第一个匹配标签的值（重复标签返回第一个）
    pub fn get(&self, t: u32) -> Option<&str> {
        self.fields
            .iter()
            .find(|(tag, _)| *tag == t)
            .map(|(_, v)| v.as_str())
    }

    /// 取整数标签值；空/非数字返回 None
    pub fn get_int(&self, t: u32) -> Option<i64> {
        self.get(t).and_then(|v| v.trim().parse().ok())
    }

    /// 取浮点标签值；空/非数字返回 None
    pub fn get_f64(&self, t: u32) -> Option<f64> {
        self.get(t).and_then(|v| v.trim().parse().ok())
    }

    /// 还原参与方重复组（453 的后续 448/452 对）。
    /// 不依赖 453 的位置，直接扫描所有 448（每个 448 开始一条记录），
    /// 紧跟其后的 452 作为角色——这样即使发送方省略 453 也能解析。
    pub fn parties(&self) -> Vec<Party> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.fields.len() {
            if self.fields[i].0 == tag::PARTY_ID {
                let id = self.fields[i].1.clone();
                let role = if i + 1 < self.fields.len() && self.fields[i + 1].0 == tag::PARTY_ROLE {
                    i += 1;
                    self.fields[i].1.trim().parse().unwrap_or(0)
                } else {
                    0
                };
                out.push(Party { id, role });
            }
            i += 1;
        }
        out
    }

    /// 转成报文弹窗的字段列表（名称 + 可读值）
    pub fn describe(&self) -> Vec<ParsedField> {
        let mut out = Vec::with_capacity(self.fields.len() + 1);
        out.push(ParsedField {
            name: "MsgType".into(),
            value: format!("{} ({})", self.msg_type, msg_type_name(&self.msg_type)),
        });
        for (t, v) in &self.fields {
            out.push(ParsedField {
                name: tag_name(*t).to_string(),
                value: readable_value(*t, v),
            });
        }
        out
    }
}

/// 一个参与方（重复组里的一条记录）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Party {
    pub id: String,
    pub role: i32,
}

impl Party {
    pub fn new(id: impl Into<String>, role: i32) -> Self {
        Self {
            id: id.into(),
            role,
        }
    }
}

/// 本模拟器从新委托/撤单消息里抽取的公共字段。
/// 各业务专属字段（转融通期限、约定号等）不参与模拟回报，保持为空。
#[derive(Debug, Clone, Default)]
pub struct StepOrder {
    /// 数据库 reqresp.reqid 列（业务类型，如 PFP/EEC）
    pub reqid: String,
    /// 消息类型：D=新委托 / F=撤单
    pub msg_type: String,
    /// 柜台委托编号（ClOrdID / 数据库 reff 字段）
    pub cl_ord_id: String,
    /// 撤单时的原始委托编号（OrigClOrdID）
    pub orig_cl_ord_id: String,
    /// 证券代码
    pub security_id: String,
    /// 买卖方向（1=买，2=卖；非交易业务可能为空）
    pub side: String,
    /// 订单类型（OrdType）
    pub ord_type: String,
    /// 委托价（元）
    pub price: f64,
    /// 委托数量（股/份/手，按业务自然单位）
    pub qty: f64,
    /// 参与方（账户/PBU/营业部）
    pub parties: Vec<Party>,
    /// 投资者证券账户（第一个角色 5 的 PartyID）
    pub account: String,
    /// 业务 PBU（角色 1 的 PartyID）
    pub pbu: String,
    /// 营业部代码（角色 4001 的 PartyID）
    pub branch: String,
    /// 订单所有者类型（ExecReport 回填）
    pub owner_type: i64,
    /// 备注文本
    pub text: String,
    /// 委托时间
    pub transact_time: String,
}

impl StepOrder {
    /// 从 STEP 消息解析出模拟器需要的公共字段。
    /// `reqid` 是数据库 reqresp.reqid 列（业务类型），消息本身不一定带。
    pub fn parse(msg: &StepMessage, reqid: &str) -> Result<Self, String> {
        if msg.msg_type != MSG_NEW_ORDER && msg.msg_type != MSG_CANCEL {
            return Err(format!("不支持的 STEP 消息类型: {}", msg.msg_type));
        }
        let cl_ord_id = msg.get(tag::CL_ORD_ID).unwrap_or("").trim().to_string();
        if cl_ord_id.is_empty() {
            return Err("STEP 委托缺少 ClOrdID(11)".into());
        }
        let orig_cl_ord_id = msg
            .get(tag::ORIG_CL_ORD_ID)
            .unwrap_or("")
            .trim()
            .to_string();
        if msg.msg_type == MSG_CANCEL && orig_cl_ord_id.is_empty() {
            return Err(format!("撤单 {} 缺少 OrigClOrdID(41)", cl_ord_id));
        }
        let parties = msg.parties();
        // 角色优先：账户=5、PBU=1、营业部=4001；缺角色时按出现顺序兜底
        let account = parties
            .iter()
            .find(|p| p.role == party_role::ACCOUNT)
            .or_else(|| parties.first())
            .map(|p| p.id.clone())
            .unwrap_or_default();
        let pbu = parties
            .iter()
            .find(|p| p.role == party_role::PBU)
            .or_else(|| parties.get(1))
            .map(|p| p.id.clone())
            .unwrap_or_default();
        let branch = parties
            .iter()
            .find(|p| p.role == party_role::BRANCH)
            .map(|p| p.id.clone())
            .unwrap_or_default();
        Ok(Self {
            reqid: reqid.trim().to_ascii_uppercase(),
            msg_type: msg.msg_type.clone(),
            cl_ord_id,
            orig_cl_ord_id,
            security_id: msg.get(tag::SECURITY_ID).unwrap_or("").trim().to_string(),
            side: msg.get(tag::SIDE).unwrap_or("").trim().to_string(),
            ord_type: msg.get(tag::ORD_TYPE).unwrap_or("").trim().to_string(),
            price: msg.get_f64(tag::PRICE).unwrap_or(0.0),
            qty: msg.get_f64(tag::ORDER_QTY).unwrap_or(0.0),
            parties,
            account,
            pbu,
            branch,
            owner_type: msg.get_int(tag::OWNER_TYPE).unwrap_or(0),
            text: msg.get(tag::TEXT).unwrap_or("").to_string(),
            transact_time: msg.get(tag::TRANSACT_TIME).unwrap_or("").to_string(),
        })
    }
}

/// STEP 消息构造器：按规范拼 `9=<bodylen>` 头 + `35=<type>` + 各标签。
///
/// 用法（链式）：
/// ```ignore
/// let text = StepBuilder::new(MSG_EXEC_REPORT)
///     .str(tag::CL_ORD_ID, "ORDER00001")
///     .int(tag::ORDER_QTY, 1000)
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct StepBuilder {
    msg_type: String,
    fields: Vec<(u32, String)>,
}

impl StepBuilder {
    pub fn new(msg_type: &str) -> Self {
        Self {
            msg_type: msg_type.to_string(),
            fields: Vec::new(),
        }
    }

    /// 追加字符串字段（空串按规范编码成 "标签= "）
    pub fn str(mut self, t: u32, value: impl Into<String>) -> Self {
        self.fields.push((t, value.into()));
        self
    }

    /// 追加整数字段
    pub fn int(mut self, t: u32, value: i64) -> Self {
        self.fields.push((t, value.to_string()));
        self
    }

    /// 追加单字符字段（协议里的枚举都是单字符）
    pub fn ch(mut self, t: u32, value: char) -> Self {
        self.fields.push((t, value.to_string()));
        self
    }

    /// 追加浮点字段，保留 3 位小数（本平台价格单位：元）
    pub fn price(mut self, t: u32, value: f64) -> Self {
        self.fields.push((t, format!("{:.3}", value)));
        self
    }

    /// 追加参与方重复组：453=个数，随后每条记录 448=ID、452=角色
    pub fn parties(mut self, parties: &[Party]) -> Self {
        self.fields
            .push((tag::NO_PARTY_IDS, parties.len().to_string()));
        for p in parties {
            self.fields.push((tag::PARTY_ID, p.id.clone()));
            self.fields.push((tag::PARTY_ROLE, p.role.to_string()));
        }
        self
    }

    /// 生成最终文本。9= 的值 = 它自身字段之后的字节数（35=... 起的部分）。
    pub fn build(self) -> String {
        let mut body = String::with_capacity(256);
        body.push_str("35=");
        body.push_str(&self.msg_type);
        body.push(SOH);
        for (t, v) in &self.fields {
            body.push_str(&t.to_string());
            body.push('=');
            // 空串按规范写成 "标签= <SOH>"（等号后一个空格）
            if v.is_empty() {
                body.push(' ');
            } else {
                body.push_str(v);
            }
            body.push(SOH);
        }
        // 消息体长度按字节数算（与参考实现 strlen 的语义一致；字段通常是 ASCII，
        // Text 含中文时也按 UTF-8 字节数，保证长度值可被逐字节校验）
        format!("9={}{}{}", body.as_bytes().len(), SOH, body)
    }
}

/// 构造确认回报（ExecutionReport，ExecType=0 / OrdStatus=0）。
pub fn build_ack(order: &StepOrder, order_id: &str, transact_time: &str) -> String {
    base_exec_report(
        order,
        order_id,
        exec_type::NEW,
        ord_status::NEW,
        transact_time,
    )
    .str(tag::ORIG_CL_ORD_ID, "")
    .int(tag::ORD_REJ_REASON, 0)
    .str(tag::EXEC_ID, "")
    .int(tag::LAST_PX, 0)
    .int(tag::LAST_QTY, 0)
    .int(tag::TOTAL_VALUE_TRADED, 0)
    .int(tag::LEAVES_QTY, order.qty.round() as i64)
    .str(tag::TEXT, "")
    .build()
}

/// 构造拒单回报（ExecutionReport，ExecType=8 / OrdStatus=8）。
pub fn build_reject(
    order: &StepOrder,
    order_id: &str,
    reason: i64,
    text: &str,
    transact_time: &str,
) -> String {
    base_exec_report(
        order,
        order_id,
        exec_type::REJECT,
        ord_status::REJECTED,
        transact_time,
    )
    .str(tag::ORIG_CL_ORD_ID, "")
    .int(tag::ORD_REJ_REASON, reason)
    .str(tag::EXEC_ID, "")
    .int(tag::LAST_PX, 0)
    .int(tag::LAST_QTY, 0)
    .int(tag::TOTAL_VALUE_TRADED, 0)
    .int(tag::LEAVES_QTY, 0)
    .str(tag::TEXT, text)
    .build()
}

/// 构造成交回报（ExecutionReport，ExecType=F）。
/// last_qty/leaves_qty 为自然单位；filled=true 时 OrdStatus=2（全成），
/// 否则 1（部分成交）。
#[allow(clippy::too_many_arguments)]
pub fn build_trade(
    order: &StepOrder,
    order_id: &str,
    exec_id: &str,
    last_px: f64,
    last_qty: f64,
    leaves_qty: f64,
    filled: bool,
    transact_time: &str,
) -> String {
    let status = if filled {
        ord_status::FILLED
    } else {
        ord_status::PARTIAL
    };
    base_exec_report(order, order_id, exec_type::TRADE, status, transact_time)
        .str(tag::ORIG_CL_ORD_ID, "")
        .int(tag::ORD_REJ_REASON, 0)
        .str(tag::EXEC_ID, exec_id)
        .price(tag::LAST_PX, last_px)
        .int(tag::LAST_QTY, last_qty.round() as i64)
        .int(
            tag::TOTAL_VALUE_TRADED,
            (last_px * last_qty).round() as i64,
        )
        .int(tag::LEAVES_QTY, leaves_qty.round() as i64)
        .str(tag::TEXT, "")
        .build()
}

/// 构造撤单成功回报（ExecutionReport，ExecType=6 / OrdStatus=6）。
/// 规范约定撤单响应不分配交易所订单号，因此 OrderID 为空。
pub fn build_cancel_success(
    cancel: &StepOrder,
    withdraw_qty: f64,
    transact_time: &str,
) -> String {
    base_exec_report(
        cancel,
        "",
        exec_type::CANCELED,
        ord_status::CANCELED,
        transact_time,
    )
    .str(tag::ORIG_CL_ORD_ID, &cancel.orig_cl_ord_id)
    .int(tag::ORD_REJ_REASON, 0)
    .str(tag::EXEC_ID, "")
    .int(tag::LAST_PX, 0)
    .int(tag::LAST_QTY, withdraw_qty.round() as i64)
    .int(tag::TOTAL_VALUE_TRADED, 0)
    .int(tag::LEAVES_QTY, 0)
    .str(tag::TEXT, "")
    .build()
}

/// 构造撤单拒绝消息（OrderCancelReject，MsgType=9）。
pub fn build_cancel_reject(
    cancel: &StepOrder,
    reason: i64,
    text: &str,
    transact_time: &str,
) -> String {
    StepBuilder::new(MSG_CANCEL_REJECT)
        .str(tag::ORDER_ID, "")
        .ch(tag::ORD_STATUS, ord_status::NEW)
        .str(tag::CL_ORD_ID, &cancel.cl_ord_id)
        .str(tag::ORIG_CL_ORD_ID, &cancel.orig_cl_ord_id)
        .str(tag::TRANSACT_TIME, transact_time)
        .int(tag::CXL_REJ_REASON, reason)
        // CxlRejResponseTo：1=对撤单请求的拒绝（FIX 标准取值）
        .ch(tag::CXL_REJ_RESPONSE_TO, '1')
        .str(tag::TEXT, text)
        .build()
}

/// 构造 ETF 申赎执行报告（1.51 规范 14.1.4 / 14.2.4）。
///
/// 三种 `TradeReportType`：
/// - `1` ETF 成交：`SecurityAltID` 为 ETF 代码，`LastQty` 为申赎份额；
/// - `2` 成份券成交：`SecurityAltID` 为成份券代码，`LastQty` 为过户数量；
/// - `3` 资金替代成交：`LastQty=0`，`TotalValueTraded` 为现金替代总额，
///   `NoLeges` 后接资金类型重复组（1644 MarginAmtType，8504 分组金额）。
///
/// 该报文按 1.51 的 ETF 专用字段表构造，不携带普通执行报告的
/// OrdStatus/LastPx 等字段（交易所发的是专用子集）。参与方固定为
/// 投资者账户（role=5）与业务 PBU（role=1）两条。
#[allow(clippy::too_many_arguments)]
pub fn build_etf_trade(
    order: &StepOrder,
    order_id: &str,
    exec_id: &str,
    trade_report_type: char,
    security_alt_id: &str,
    last_qty: f64,
    side: char,
    total_value_traded: f64,
    cash_groups: &[(i32, f64)],
    num_trades: u32,
    transact_time: &str,
) -> String {
    let mut parties = Vec::new();
    if !order.account.is_empty() {
        parties.push(Party::new(order.account.clone(), party_role::ACCOUNT));
    }
    if !order.pbu.is_empty() {
        parties.push(Party::new(order.pbu.clone(), party_role::PBU));
    }
    if parties.is_empty() {
        parties = vec![
            Party::new("", party_role::ACCOUNT),
            Party::new("", party_role::PBU),
        ];
    }
    let mut b = StepBuilder::new(MSG_EXEC_REPORT)
        .str(tag::CL_ORD_ID, &order.cl_ord_id)
        .str(tag::ORDER_ID, order_id)
        .str(tag::EXEC_ID, exec_id)
        .str(tag::SECURITY_ID, &order.security_id)
        .ch(tag::TRADE_REPORT_TYPE, trade_report_type)
        .str(tag::SECURITY_ALT_ID, security_alt_id)
        .int(tag::LAST_QTY, last_qty.round() as i64)
        // 顶层成交金额放在重复组之前：与 EzStepTemplate 的 ExecutionReport
        // 字段顺序一致，也便于只按标签取值的解析器拿到“总额”
        .price(tag::TOTAL_VALUE_TRADED, total_value_traded)
        .int(tag::NO_LEGS, cash_groups.len() as i64);
    for (amt_type, amount) in cash_groups {
        b = b.int(tag::MARGIN_AMT_TYPE, *amt_type as i64);
        b = b.price(tag::TOTAL_VALUE_TRADED, *amount);
    }
    b.int(tag::NUM_TRADES, num_trades as i64)
        .ch(tag::SIDE, side)
        .str(tag::TRANSACT_TIME, transact_time)
        .str(tag::ORIG_TIME, &order.transact_time)
        .ch(tag::EXEC_TYPE, exec_type::TRADE)
        .parties(&parties)
        .build()
}

/// 执行报告公共字段（顺序与 EzStepTemplate.xml 的 ExecutionReport 模板一致）。
/// 各业务用不到的字段按规范填空；订单缓存/柜台解析只依赖其中少数几个。
fn base_exec_report(
    order: &StepOrder,
    order_id: &str,
    exec_type: char,
    status: char,
    transact_time: &str,
) -> StepBuilder {
    let parties = if order.parties.is_empty() {
        // 没有参与方时至少保留账户与 PBU 两个空记录，避免柜台解析重复组越界
        vec![Party::new("", party_role::ACCOUNT), Party::new("", party_role::PBU)]
    } else {
        order.parties.clone()
    };
    StepBuilder::new(MSG_EXEC_REPORT)
        .str(tag::CL_ORD_ID, &order.cl_ord_id)
        .str(tag::ORDER_ID, order_id)
        .ch(tag::EXEC_TYPE, exec_type)
        .ch(tag::ORD_STATUS, status)
        .str(tag::SECURITY_ID, &order.security_id)
        .str(tag::UNDERLYING_SECURITY_ID, "")
        .str(tag::TRANSACT_TIME, transact_time)
        .str(tag::PRICE2, "")
        .str(tag::SIDE, &order.side)
        .str(tag::POSITION_EFFECT, "")
        .str(tag::COVERED_OR_UNCOVERED, "")
        .str(tag::SECURITY_ALT_ID, "")
        .str(tag::TRADE_REPORT_TYPE, "")
        .int(tag::NO_LEGS, 0)
        .int(tag::NUM_TRADES, 0)
        .int(tag::OWNER_TYPE, order.owner_type)
        .str(tag::NO_DATES, "")
        .str(tag::ORIG_TIME, "")
        .parties(&parties)
        .int(tag::NO_PARTY_DETAILS, 0)
}

/// STEP 里各标签对应的字段名（报文弹窗展示用）。
pub fn tag_name(t: u32) -> &'static str {
    match t {
        tag::MSG_TYPE => "MsgType",
        tag::CL_ORD_ID => "ClOrdID",
        tag::ORDER_ID => "OrderID",
        tag::ORDER_QTY => "OrderQty",
        tag::ORD_STATUS => "OrdStatus",
        tag::ORD_TYPE => "OrdType",
        tag::ORIG_CL_ORD_ID => "OrigClOrdID",
        tag::ORIG_TIME => "OrigTime",
        tag::PRICE => "Price",
        tag::SECURITY_ID => "SecurityID",
        tag::SIDE => "Side",
        tag::TEXT => "Text",
        tag::TIME_IN_FORCE => "TimeInForce",
        tag::TRANSACT_TIME => "TransactTime",
        tag::POSITION_EFFECT => "PositionEffect",
        tag::CXL_REJ_REASON => "CxlRejReason",
        tag::ORD_REJ_REASON => "OrdRejReason",
        tag::EXEC_TYPE => "ExecType",
        tag::LEAVES_QTY => "LeavesQty",
        tag::EXEC_ID => "ExecID",
        tag::LAST_PX => "LastPx",
        tag::LAST_QTY => "LastQty",
        tag::COVERED_OR_UNCOVERED => "CoveredOrUncovered",
        tag::UNDERLYING_SECURITY_ID => "UnderlyingSecurityID",
        tag::SECURITY_ALT_ID => "SecurityAltID",
        tag::CXL_REJ_RESPONSE_TO => "CxlRejResponseTo",
        tag::PARTY_ID => "PartyID",
        tag::PARTY_ROLE => "PartyRole",
        tag::NO_PARTY_IDS => "NoPartyIDs",
        tag::OWNER_TYPE => "OwnerType",
        tag::NO_LEGS => "NoLeges",
        tag::NO_DATES => "NoDates",
        tag::PRICE2 => "Price2",
        tag::TRADE_REPORT_TYPE => "TradeReportType",
        tag::NUM_TRADES => "NumTrades",
        tag::TOTAL_VALUE_TRADED => "TotalValueTraded",
        tag::MARGIN_AMT_TYPE => "MarginAmtType",
        tag::NO_PARTY_DETAILS => "NoPartyDetails",
        _ => "Unknown",
    }
}

/// 消息类型中文名
fn msg_type_name(t: &str) -> &'static str {
    match t {
        MSG_NEW_ORDER => "新委托",
        MSG_CANCEL => "撤单请求",
        MSG_EXEC_REPORT => "执行报告",
        MSG_CANCEL_REJECT => "撤单拒绝",
        _ => "未知",
    }
}

/// 枚举字段附中文含义，其它字段原样展示
fn readable_value(t: u32, v: &str) -> String {
    match t {
        tag::SIDE => match v {
            "1" => "1 (买)".into(),
            "2" => "2 (卖)".into(),
            _ => v.into(),
        },
        tag::ORD_STATUS => match v {
            "0" => "0 (已报)".into(),
            "1" => "1 (部分成交)".into(),
            "2" => "2 (全部成交)".into(),
            "6" => "6 (已撤)".into(),
            "8" => "8 (已拒绝)".into(),
            _ => v.into(),
        },
        tag::EXEC_TYPE => match v {
            "0" => "0 (已报/确认)".into(),
            "6" => "6 (已撤)".into(),
            "8" => "8 (拒绝)".into(),
            "F" => "F (成交)".into(),
            _ => v.into(),
        },
        tag::PARTY_ROLE => match v {
            "1" => "1 (业务PBU)".into(),
            "5" => "5 (证券账户)".into(),
            "4001" => "4001 (营业部)".into(),
            _ => v.into(),
        },
        _ => v.into(),
    }
}

/// 业务类型（reqresp.reqid）对应的成交回报广播类型与产品集。
///
/// 数据来自《上海证券交易所综合业务平台市场参与者接口规格说明书 1.51》
/// 第 3~4 页的“业务类型与执行报告数据广播类型及产品集关系表”。
/// `has_exec_report=false` 的业务规范上只回确认（N/A），即使策略是
/// “全部成交”也不生成成交记录（与深交所无成交回报业务同样的降级规则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BizMapping {
    /// 执行报告的 bcasttype（execreport.bcasttype 列），如 "01"、"19"
    pub bcast_type: &'static str,
    /// 产品集编号（execreport.setid 列），如 "102"
    pub set_id: &'static str,
    /// 该业务是否有执行报告（成交）回报
    pub has_exec_report: bool,
}

impl BizMapping {
    const fn new(bcast_type: &'static str, set_id: &'static str, has: bool) -> Self {
        Self {
            bcast_type,
            set_id,
            has_exec_report: has,
        }
    }
}

/// 按 reqid 查业务映射。未知业务按 ETF 现金申赎（01/102）兜底并允许成交，
/// 方便联调新业务；调用方可在日志里提示“未知业务类型”。
pub fn biz_mapping(reqid: &str) -> BizMapping {
    match reqid.trim().to_ascii_uppercase().as_str() {
        // ETF 现金申赎 / 黄金 ETF 实物申赎 / ETF 实物申赎
        "EEC" | "EER" => BizMapping::new("01", "102", true),
        "GEC" | "GER" => BizMapping::new("16", "102", true),
        "IEC" | "IER" => BizMapping::new("20", "102", true),
        // 大宗交易
        "BPT" => BizMapping::new("02", "101", true),
        "FPE" => BizMapping::new("14", "101", true),
        "BII" => BizMapping::new("03", "101", false),
        // 转融通（执行报告有 05/07/09 与 06/08/10 多种，取首个）
        "ZPT" => BizMapping::new("05", "104", true),
        "ZII" => BizMapping::new("06", "104", true),
        // 货币市场基金
        "OFC" | "OFR" => BizMapping::new("12", "105", true),
        // 上证 LOF / 公募 REITs
        "LFS" | "LFC" | "LFR" | "LFT" | "LFP" | "LFM" => BizMapping::new("13", "105", true),
        // 国债预发行
        "BIP" => BizMapping::new("17", "106", true),
        "BIY" => BizMapping::new("18", "106", true),
        // 盘后固定价格
        "PFP" => BizMapping::new("19", "103", true),
        // 债券质押式回购出入库
        "BPD" | "BPW" => BizMapping::new("21", "121", true),
        // 规范上无执行报告的业务（只回确认）
        "QBD" | "QBW" | "QNE" | "QCA" | "RNE" | "RNR" | "SPN" | "SPS" | "SPR" | "SPT"
        | "SPD" | "SPB" => BizMapping::new("00", "100", false),
        "VTE" | "CFC" | "PWS" => BizMapping::new("00", "109", false),
        "CBT" | "CBR" | "BES" | "BSS" | "BSB" | "BSC" => BizMapping::new("00", "121", false),
        "ECP" | "EMR" => BizMapping::new("00", "102", false),
        // 参考柜台的指定交易/指定撤销（不在 1.51 业务表里）：只回确认
        "ADT" | "ADC" => BizMapping::new("00", "102", false),
        // 未知业务：按 ETF 现金申赎兜底，允许成交，便于新业务联调
        _ => BizMapping::new("01", "102", true),
    }
}

/// 当前时间的 STEP 时间戳：`YYYYMMDD-HH:MM:SS.mmm`。
/// 与参考柜台 recordtimestamp 的格式一致（也从第 9 位起是 HH:MM:SS）。
pub fn now_timestamp() -> String {
    chrono::Local::now()
        .format("%Y%m%d-%H:%M:%S%.3f")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_order() -> StepOrder {
        StepOrder {
            msg_type: MSG_NEW_ORDER.into(),
            cl_ord_id: "ORDER00001".into(),
            security_id: "510300".into(),
            side: "1".into(),
            ord_type: "2".into(),
            price: 4.123,
            qty: 1000.0,
            parties: vec![
                Party::new("A123456789", party_role::ACCOUNT),
                Party::new("PBU01", party_role::PBU),
                Party::new("888", party_role::BRANCH),
            ],
            account: "A123456789".into(),
            pbu: "PBU01".into(),
            branch: "888".into(),
            ..Default::default()
        }
    }

    #[test]
    fn builder_and_parser_roundtrip() {
        let msg = StepBuilder::new(MSG_EXEC_REPORT)
            .str(tag::CL_ORD_ID, "ORDER00001")
            .str(tag::SECURITY_ID, "510300")
            .ch(tag::EXEC_TYPE, exec_type::NEW)
            .ch(tag::ORD_STATUS, ord_status::NEW)
            .parties(&[
                Party::new("A123456789", party_role::ACCOUNT),
                Party::new("PBU01", party_role::PBU),
            ])
            .build();
        // 头部格式：9=<bodylen><SOH>35=8<SOH>
        assert!(msg.starts_with("9="));
        let first_soh = msg.find(SOH).unwrap();
        let declared: usize = msg[2..first_soh].parse().unwrap();
        let body = &msg[first_soh + 1..];
        assert_eq!(declared, body.as_bytes().len(), "9= 必须等于其后字节数");
        assert!(body.starts_with("35=8\u{1}"));

        let parsed = StepMessage::parse(&msg).unwrap();
        assert_eq!(parsed.msg_type, "8");
        assert_eq!(parsed.get(tag::CL_ORD_ID), Some("ORDER00001"));
        assert_eq!(parsed.get(tag::EXEC_TYPE), Some("0"));
        let parties = parsed.parties();
        assert_eq!(parties.len(), 2);
        assert_eq!(parties[0], Party::new("A123456789", 5));
        assert_eq!(parties[1], Party::new("PBU01", 1));
    }

    #[test]
    fn empty_string_uses_space_placeholder() {
        let msg = StepBuilder::new(MSG_EXEC_REPORT)
            .str(tag::UNDERLYING_SECURITY_ID, "")
            .build();
        assert!(msg.contains("308= \u{1}"));
        let parsed = StepMessage::parse(&msg).unwrap();
        assert_eq!(parsed.get(tag::UNDERLYING_SECURITY_ID), Some(""));
    }

    #[test]
    fn parse_order_extracts_common_fields() {
        let msg = StepBuilder::new(MSG_NEW_ORDER)
            .str(tag::CL_ORD_ID, "ORDER00001")
            .str(tag::SECURITY_ID, "510300")
            .str(tag::SIDE, "1")
            .str(tag::ORD_TYPE, "2")
            .price(tag::PRICE, 4.123)
            .int(tag::ORDER_QTY, 1000)
            .parties(&[
                Party::new("A123456789", party_role::ACCOUNT),
                Party::new("PBU01", party_role::PBU),
                Party::new("888", party_role::BRANCH),
            ])
            .build();
        let parsed = StepMessage::parse(&msg).unwrap();
        let order = StepOrder::parse(&parsed, "PFP").unwrap();
        assert_eq!(order.cl_ord_id, "ORDER00001");
        assert_eq!(order.security_id, "510300");
        assert_eq!(order.side, "1");
        assert!((order.price - 4.123).abs() < 1e-9);
        assert_eq!(order.qty, 1000.0);
        assert_eq!(order.account, "A123456789");
        assert_eq!(order.pbu, "PBU01");
        assert_eq!(order.branch, "888");
    }

    #[test]
    fn parse_cancel_requires_orig_cl_ord_id() {
        let msg = StepBuilder::new(MSG_CANCEL)
            .str(tag::CL_ORD_ID, "CANCEL0001")
            .str(tag::SECURITY_ID, "510300")
            .build();
        let parsed = StepMessage::parse(&msg).unwrap();
        assert!(StepOrder::parse(&parsed, "PFP").is_err());

        let msg = StepBuilder::new(MSG_CANCEL)
            .str(tag::CL_ORD_ID, "CANCEL0001")
            .str(tag::ORIG_CL_ORD_ID, "ORDER00001")
            .build();
        let parsed = StepMessage::parse(&msg).unwrap();
        let cancel = StepOrder::parse(&parsed, "PFP").unwrap();
        assert_eq!(cancel.orig_cl_ord_id, "ORDER00001");
    }

    #[test]
    fn response_builders_parse_back() {
        let order = sample_order();
        let ack = build_ack(&order, "0000000000000001", "20260101-09:30:00.000");
        let msg = StepMessage::parse(&ack).unwrap();
        assert_eq!(msg.msg_type, "8");
        assert_eq!(msg.get(tag::EXEC_TYPE), Some("0"));
        assert_eq!(msg.get(tag::ORD_STATUS), Some("0"));
        assert_eq!(msg.get(tag::LEAVES_QTY), Some("1000"));

        let rej = build_reject(&order, "", 1025, "测试拒单", "20260101-09:30:00.000");
        let msg = StepMessage::parse(&rej).unwrap();
        assert_eq!(msg.get(tag::EXEC_TYPE), Some("8"));
        assert_eq!(msg.get(tag::ORD_STATUS), Some("8"));
        assert_eq!(msg.get(tag::ORD_REJ_REASON), Some("1025"));

        let tr = build_trade(
            &order,
            "0000000000000001",
            "0000000000000001",
            4.1,
            500.0,
            500.0,
            false,
            "20260101-09:30:01.000",
        );
        let msg = StepMessage::parse(&tr).unwrap();
        assert_eq!(msg.get(tag::EXEC_TYPE), Some("F"));
        assert_eq!(msg.get(tag::ORD_STATUS), Some("1"));
        assert_eq!(msg.get(tag::LAST_QTY), Some("500"));
        assert_eq!(msg.get(tag::LEAVES_QTY), Some("500"));

        let cxl = build_cancel_success(&order, 500.0, "20260101-09:31:00.000");
        let msg = StepMessage::parse(&cxl).unwrap();
        assert_eq!(msg.get(tag::EXEC_TYPE), Some("6"));
        assert_eq!(msg.get(tag::ORD_STATUS), Some("6"));
        assert_eq!(msg.get(tag::ORDER_ID), Some(""));
    }

    #[test]
    fn body_length_ignores_trailing_db_padding() {
        let text = format!("{}   \0", StepBuilder::new("D").build());
        let msg = StepMessage::parse(&text).unwrap();
        assert_eq!(msg.msg_type, "D");
    }

    #[test]
    fn biz_mapping_covers_common_businesses() {
        assert_eq!(biz_mapping("EEC").bcast_type, "01");
        assert_eq!(biz_mapping("PFP").bcast_type, "19");
        assert_eq!(biz_mapping("PFP").set_id, "103");
        assert!(biz_mapping("PFP").has_exec_report);
        assert!(!biz_mapping("CBT").has_exec_report);
        assert!(!biz_mapping("ADT").has_exec_report);
        assert_eq!(biz_mapping("UNKNOWN").bcast_type, "01");
    }
}
