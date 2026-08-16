//! 收发报文捕获：把每个 TCP 连接收发的原始字节记录成 16 进制文本行，
//! 供前端弹窗实时展示，并可选持久化到文件。
//!
//! # 设计要点
//!
//! - 每个柜台连接对应一个 `ConnRecorder`，收/发的每一条完整报文都记一行。
//! - 展示与文件内容完全一致，格式为：`时:分:秒.毫秒 RECV/SEND  十六进制字节`。
//! - 每条报文同时携带“按交易所字段名解析”的结果（`ParsedField` 列表，由
//!   各协议的 protocol 模块在记录前解析好传入）：界面展示时原始报文与解析
//!   字段分两行呈现，持久化文件里解析字段作为缩进续行写在原始报文下方，
//!   便于对照阅读。解析失败或未知消息类型时字段列表为空，只记原始报文。
//! - 是否启用捕获、是否写文件由平台配置的两个开关决定（见 config.rs）：
//!     * show_packets：允许在界面弹窗查看
//!     * persist_packets：额外把报文追加写入文件
//!   只要任一开关打开就会创建记录器。
//! - 内存缓冲策略：
//!     * 持久化开：保留从连接建立起的全部报文（弹窗要能回看全部历史）。
//!     * 持久化关：只保留最近 MEM_CAP 条（环形缓冲），避免长时间运行占内存。
//! - 报文序号平台内全局唯一：同一平台的所有连接共享一个序号发生器，
//!   前端用 `after_seq` 游标做平台级增量拉取（弹窗每次只取 seq 大于游标
//!   的新报文，跨连接合并后追加到界面）。
//! - 连接断开后记录器不删除（仅标记离线）：平台弹窗要能按连接时间顺序
//!   回看历史连接的报文，每条连接一个分组标题（对端地址/接入时间）。

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// 报文方向。序列化为 camelCase → "recv" / "send"，前端据此上色。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Dir {
    /// 收到柜台发来的报文
    Recv,
    /// 发送给柜台的报文
    Send,
}

impl Dir {
    /// 文本标签（写入文件与内存记录的 hex 前缀保持一致）
    fn label(self) -> &'static str {
        match self {
            Dir::Recv => "RECV",
            Dir::Send => "SEND",
        }
    }
}

/// 报文解析出的一个字段（交易所规范字段名 + 可读值）。
/// 由各协议模块的 describe_fields 生成，界面按名称着色、收发底色区分。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedField {
    /// 交易所规范字段名，如 ClOrdID、SecurityID、Side
    pub name: String,
    /// 可读值：价格/数量换算成自然单位、时间戳转可读时间、
    /// 枚举字符附中文含义，如 "1 (买)"
    pub value: String,
}

/// 一条报文记录（收或发的一条完整报文）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PacketRecord {
    /// 平台内单调递增序号（跨连接全局唯一，前端增量拉取的游标）
    pub seq: u64,
    /// 所属连接序号（平台级报文弹窗里标注是哪条连接的报文）
    pub conn_id: u64,
    /// 时间戳（时:分:秒.毫秒）
    pub ts: String,
    /// 收 / 发方向
    pub dir: Dir,
    /// 报文原始字节的大写十六进制（空格分隔）
    pub hex: String,
    /// 按交易所字段名解析出的字段列表（空 = 未解析成功，只展示原始报文）
    pub fields: Vec<ParsedField>,
}

/// 分页返回给前端的一批报文
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PacketPage {
    /// 本批报文（seq 大于请求游标的部分）
    pub packets: Vec<PacketRecord>,
    /// 当前最大 seq（前端把游标推进到这里）
    pub latest_seq: u64,
    /// 该连接是否持久化（前端据此决定是否展示“完整历史”徽标）
    pub persist: bool,
    /// 平台下各连接的摘要（前端按连接分组展示时做分组标题；
    /// 含已断开的连接——记录器保留供回看，alive 标记是否在线）
    pub conns: Vec<ConnBrief>,
}

/// 一条连接的摘要（平台报文弹窗里的分组标题信息）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnBrief {
    /// 连接序号（与 PacketRecord.connId 对应）
    pub conn_id: u64,
    /// 对端地址（柜台的 IP:端口）
    pub peer: String,
    /// 接入时间（HH:MM:SS）
    pub since: String,
    /// 是否仍在线（断开的连接保留历史报文，但标记为已离线）
    pub alive: bool,
}

/// 非持久化模式下内存缓冲上限（超过则丢弃最旧的）
const MEM_CAP: usize = 5000;

/// 单个连接的报文记录器：线程安全，收发两侧任务共享同一份。
/// 连接断开后记录器保留（平台弹窗回看历史连接用），只把 alive 标记为离线。
pub struct ConnRecorder {
    /// 本连接所属的连接序号（写进每条记录，平台级弹窗据此标注）
    conn_id: u64,
    /// 平台级全局报文序号发生器（同一平台所有连接共享，跨连接唯一）
    seq_src: Arc<AtomicU64>,
    /// 是否持久化（决定内存是否保留全部历史）
    persist: bool,
    /// 对端地址（柜台 IP:端口，平台弹窗分组标题展示）
    peer: String,
    /// 接入时间（HH:MM:SS，平台弹窗分组标题展示）
    since: String,
    /// 是否仍在线（连接结束时 mark_dead，记录器本身保留）
    alive: AtomicBool,
    /// 内存缓冲（供前端弹窗读取）
    buf: Mutex<VecDeque<PacketRecord>>,
    /// 已发送回报帧缓存：分区号 → (回报记录号, 完整帧)。
    /// 供“回报同步”请求按 begin 重发历史回报（重发帧保留原 ReportIndex，
    /// 柜台按分区+记录号对账）；记录器在连接断开后保留，因此同一平台
    /// 历史连接发过的回报也能补发。BTreeMap 按键有序，天然支持
    /// “记录号 >= begin”区间查询，重发帧同号覆盖即去重。
    reports: Mutex<HashMap<i32, BTreeMap<i64, Vec<u8>>>>,
    /// 持久化文件写入器（persist 为 true 且成功打开文件时才有）
    file: Option<Mutex<BufWriter<File>>>,
}

impl ConnRecorder {
    /// 新建记录器。file_path 为 Some 时尝试创建/追加打开文件（失败则退化为仅内存）。
    /// conn_id 标注记录归属；seq_src 为平台级共享序号源，保证跨连接序号全局唯一
    /// （平台级弹窗以 seq 为增量游标，各连接的报文才能合并排序）。
    /// peer/since 记录柜台来源与接入时间，供平台弹窗分组标题展示。
    pub fn new(
        conn_id: u64,
        seq_src: Arc<AtomicU64>,
        persist: bool,
        file_path: Option<&Path>,
        peer: String,
        since: String,
    ) -> Self {
        let file = file_path
            .and_then(|p| {
                if let Some(dir) = p.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                OpenOptions::new().create(true).append(true).open(p).ok()
            })
            .map(|f| Mutex::new(BufWriter::new(f)));
        Self {
            conn_id,
            seq_src,
            persist,
            peer,
            since,
            alive: AtomicBool::new(true),
            buf: Mutex::new(VecDeque::new()),
            reports: Mutex::new(HashMap::new()),
            file,
        }
    }

    /// 连接摘要（平台弹窗分组标题的信息来源）
    pub fn brief(&self) -> ConnBrief {
        ConnBrief {
            conn_id: self.conn_id,
            peer: self.peer.clone(),
            since: self.since.clone(),
            alive: self.alive.load(Ordering::Relaxed),
        }
    }

    /// 标记本连接已断开（记录器保留，供平台弹窗回看该连接的历史报文）
    pub fn mark_dead(&self) {
        self.alive.store(false, Ordering::Relaxed);
    }

    /// 记录一条收到的报文（fields 为按交易所字段名解析的结果，可为空）
    pub fn record_recv(&self, raw: &[u8], fields: Vec<ParsedField>) {
        self.record(Dir::Recv, raw, fields);
    }

    /// 记录一条发送的报文（fields 为按交易所字段名解析的结果，可为空）
    pub fn record_send(&self, raw: &[u8], fields: Vec<ParsedField>) {
        self.record(Dir::Send, raw, fields);
    }

    /// 记录一条报文：写文件 + 存内存缓冲
    fn record(&self, dir: Dir, raw: &[u8], fields: Vec<ParsedField>) {
        let seq = self.seq_src.fetch_add(1, Ordering::Relaxed) + 1;
        let ts = chrono::Local::now().format("%H:%M:%S%.3f").to_string();
        let hex = to_hex(raw);
        // 写文件：格式与界面展示一致，解析字段作为缩进续行写在原始报文
        // 下方（与原始报文同一文件，方便对照阅读）；逐行 flush 保证掉电不丢
            if let Some(f) = &self.file {
                let mut w = f.lock();
                let _ = writeln!(w, "{} {}  {}", ts, dir.label(), hex);
                for fd in &fields {
                    let _ = writeln!(w, "{:<19} {:<24}: {}", "", fd.name, fd.value);
                }
                let _ = w.flush();
            }
        let rec = PacketRecord { seq, conn_id: self.conn_id, ts, dir, hex, fields };
        let mut buf = self.buf.lock();
        buf.push_back(rec);
        // 非持久化模式：只保留最近 MEM_CAP 条
        if !self.persist {
            while buf.len() > MEM_CAP {
                buf.pop_front();
            }
        }
    }

    /// 登记一条已发送的回报帧（writer 在补写 ReportIndex 后调用）。
    /// 相同 (分区, 记录号) 只保留一份：回报同步重发的历史帧也会走
    /// 本方法再次登记，同号覆盖避免下次同步重复补发。
    pub fn record_report(&self, partition: i32, report_index: i64, frame: &[u8]) {
        let mut m = self.reports.lock();
        m.entry(partition)
            .or_default()
            .insert(report_index, frame.to_vec());
    }

    /// 取某分区“记录号 >= begin”的全部已发送回报帧（按记录号升序）。
    /// 回报同步按 begin 重发时调用；无缓存（捕获开关未开）时返回空。
    pub fn reports_since(&self, partition: i32, begin: i64) -> Vec<(i64, Vec<u8>)> {
        let m = self.reports.lock();
        match m.get(&partition) {
            Some(map) => map.range(begin..).map(|(k, v)| (*k, v.clone())).collect(),
            None => Vec::new(),
        }
    }

    /// 取 seq 大于 after_seq 的报文（after_seq=0 表示取当前缓冲的全部）
    pub fn page(&self, after_seq: u64) -> PacketPage {
        let buf = self.buf.lock();
        let latest_seq = buf.back().map(|r| r.seq).unwrap_or(0);
        let packets: Vec<PacketRecord> =
            buf.iter().filter(|r| r.seq > after_seq).cloned().collect();
        PacketPage {
            packets,
            latest_seq,
            persist: self.persist,
            // 单记录器视角不填连接摘要（engine 的 conn_packets/platform_packets
            // 拉取后按需填充）
            conns: Vec::new(),
        }
    }
}

/// 字节切片 → 大写十六进制、空格分隔，如 "FE 3F 86 0C"
fn to_hex(raw: &[u8]) -> String {
    let mut s = String::with_capacity(raw.len().saturating_mul(3));
    for (i, b) in raw.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{:02X}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_format() {
        assert_eq!(to_hex(&[0xFE, 0x3F, 0x86, 0x0C]), "FE 3F 86 0C");
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn ring_buffer_caps_when_not_persist() {
        let r = ConnRecorder::new(
            1,
            Arc::new(AtomicU64::new(0)),
            false,
            None,
            "127.0.0.1:10001".into(),
            "09:00:00".into(),
        );
        for _ in 0..(MEM_CAP + 100) {
            r.record_recv(&[0x01], vec![]);
        }
        let page = r.page(0);
        assert_eq!(page.packets.len(), MEM_CAP);
        assert!(!page.persist);
        // latest_seq 仍是真实累计值
        assert_eq!(page.latest_seq, (MEM_CAP + 100) as u64);
    }

    #[test]
    fn after_seq_incremental() {
        let r = ConnRecorder::new(
            1,
            Arc::new(AtomicU64::new(0)),
            true,
            None,
            "127.0.0.1:10001".into(),
            "09:00:00".into(),
        );
        r.record_recv(&[0x01], vec![]);
        r.record_send(&[0x02], vec![]);
        r.record_recv(&[0x03], vec![]);
        let page = r.page(1);
        assert_eq!(page.packets.len(), 2);
        assert_eq!(page.packets[0].seq, 2);
        assert_eq!(page.latest_seq, 3);
        // 记录里带连接序号（平台级弹窗标注用）
        assert_eq!(page.packets[0].conn_id, 1);
    }

    #[test]
    fn brief_reports_alive_and_dead() {
        // 摘要携带对端地址/接入时间，mark_dead 后 alive 变 false（记录器仍可查）
        let r = ConnRecorder::new(
            7,
            Arc::new(AtomicU64::new(0)),
            true,
            None,
            "192.168.1.100:8080".into(),
            "09:28:30".into(),
        );
        r.record_recv(&[0x01], vec![]);
        let b = r.brief();
        assert_eq!(b.conn_id, 7);
        assert_eq!(b.peer, "192.168.1.100:8080");
        assert_eq!(b.since, "09:28:30");
        assert!(b.alive);
        // 断开后：标记离线，但缓冲里的报文依然可以拉取
        r.mark_dead();
        assert!(!r.brief().alive);
        assert_eq!(r.page(0).packets.len(), 1);
    }

    #[test]
    fn seq_global_across_conns() {
        // 两个连接共享同一平台级序号源：seq 跨连接全局唯一
        let src = Arc::new(AtomicU64::new(0));
        let a = ConnRecorder::new(
            1,
            src.clone(),
            true,
            None,
            "127.0.0.1:10001".into(),
            "09:00:00".into(),
        );
        let b = ConnRecorder::new(
            2,
            src,
            true,
            None,
            "127.0.0.1:10002".into(),
            "09:01:00".into(),
        );
        a.record_recv(&[0x01], vec![]);
        b.record_recv(&[0x02], vec![]);
        b.record_send(&[0x03], vec![]);
        let pa = a.page(0);
        let pb = b.page(0);
        assert_eq!(pa.packets[0].seq, 1);
        assert_eq!(pa.packets[0].conn_id, 1);
        assert_eq!(pb.packets[0].seq, 2);
        assert_eq!(pb.packets[0].conn_id, 2);
        assert_eq!(pb.packets[1].seq, 3);
    }

    #[test]
    fn report_cache_since_and_dedup() {
        // 回报同步重发缓存：按分区 + begin 过滤、同号覆盖去重
        let r = ConnRecorder::new(
            1,
            Arc::new(AtomicU64::new(0)),
            true,
            None,
            "127.0.0.1:10001".into(),
            "09:00:00".into(),
        );
        r.record_report(1, 1, b"frame-1");
        r.record_report(1, 2, b"frame-2");
        r.record_report(2, 1, b"other-part");
        // begin 过滤（含等于 begin 的记录）
        let got = r.reports_since(1, 2);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], (2, b"frame-2".to_vec()));
        // 全量按记录号升序
        let all = r.reports_since(1, 1);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, 1);
        assert_eq!(all[1].0, 2);
        // 同号覆盖去重：重发帧再登记不会产生重复条目
        r.record_report(1, 2, b"frame-2-resend");
        assert_eq!(r.reports_since(1, 1).len(), 2);
        assert_eq!(r.reports_since(1, 2)[0].1, b"frame-2-resend");
        // 未登记的记录号范围/分区返回空
        assert!(r.reports_since(1, 3).is_empty());
        assert!(r.reports_since(3, 1).is_empty());
    }
}
