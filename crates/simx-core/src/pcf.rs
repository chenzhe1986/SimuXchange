//! 深交所 ETF 实时申购赎回清单（PCF）文件解析。
//!
//! # 背景与职责
//!
//! ETF 实时申购赎回业务（ApplID=120）的执行报告（201202）扩展字段携带
//! “成份股明细”重复组：每只成份券的交付数量（DeliveryQty）与现金替代
//! 金额（SubstCash）。真实交易所按每日发布的申购赎回清单（PCF 文件，
//! 文件名 `pcf_<ETF代码>_<YYYYMMDD>.xml`，格式定义见深交所《ETF实时申购
//! 赎回业务参考信息 pcf 文件内容定义》表 3-10）生成该明细。
//!
//! 模拟器不接交易所行情，因此按用户配置的清单目录读取本地 PCF 文件：
//! 1. [`find_latest_pcf`]：在目录中按 ETF 代码查找**日期最新**的清单文件
//!    （目录下一般存有所有 ETF 代码多日的文件，取文件名日期最大者）；
//! 2. [`parse_pcf`]：解析清单 XML，取出最小申赎单位与全部成份券
//!    （代码 / 代码源 / 每篮子数量 / 替代标志 / 申购替代金额 / 赎回替代金额）；
//! 3. [`PcfCache`]：查找/解析结果缓存（会话级）。清单目录可能非常大
//!    （所有 ETF 代码 × 多日），逐笔委托全量扫描目录会拖慢收单路径，
//!    缓存按 (目录, 代码) 记忆：目录 mtime 未变时零扫描，文件未变时
//!    复用已解析结果（细节见 `PcfCache` 文档）。
//!
//! 换算成回报明细（按委托份额折算篮子数后缩放）由调用方
//! （sz/session.rs 的 handle_new_order）完成，本模块只做“找文件 + 解析”。

use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// 解析后的 PCF 清单文件（只保留生成执行报告明细需要的字段）。
#[derive(Debug, Clone, Default)]
pub struct PcfFile {
    /// 基金（ETF）证券代码，如 "159920"
    pub security_id: String,
    /// 最小申购赎回单位（一个篮子对应的 ETF 份数，文件中为正整数，如 2000000）
    pub creation_redemption_unit: f64,
    /// 成份券列表（按文件中的先后顺序，含“申赎现金”虚拟券）
    pub components: Vec<PcfComponent>,
}

/// 一只成份券（PCF 文件 Components/Component 节点）。
#[derive(Debug, Clone, Default)]
pub struct PcfComponent {
    /// 成份券证券代码（如 "000001"；“申赎现金”为 "159900"）
    pub security_id: String,
    /// 证券代码源（101=上交所 102=深交所 103=港交所 105=外汇交易中心
    /// 106=北交所 9999=其他）
    pub security_id_source: String,
    /// 每个申购篮子中该成份证券的数量（替代标志为 2 时为 0）
    pub component_share: f64,
    /// 现金替代标志（0=禁止 1=可以 2=必须用现金替代）
    pub substitute_flag: u8,
    /// 申购替代金额（替代标志为 2 时该证券申购所需总金额）
    pub creation_cash_substitute: f64,
    /// 赎回替代金额（替代标志为 2 时该证券赎回返还的总金额）
    pub redemption_cash_substitute: f64,
}

/// 解析 PCF 清单 XML 文本。
///
/// 用事件流（quick-xml 的拉取式解析）逐节点读取，只关心叶子字段文本：
/// - 顶层 `SecurityID` / `CreationRedemptionUnit`（注意顶层还有一个
///   `UnderlyingSecurityID` 表示拟合指数代码，与成份券内同名字段区分，
///   这里按“是否处于 Component 节点内”分别归位）；
/// - `Components/Component` 内的 `UnderlyingSecurityID` /
///   `UnderlyingSecurityIDSource` / `ComponentShare` / `SubstituteFlag` /
///   `CreationCashSubstitute` / `RedemptionCashSubstitute`。
///
/// 数值字段解析失败按 0 处理（宽容数据脏读，不因此整体失败）；
/// XML 结构损坏（标签不闭合等）返回 Err，由调用方告警并降级。
pub fn parse_pcf(xml: &str) -> Result<PcfFile, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = PcfFile::default();
    // 当前正在解析的成份券（处于 Component 节点内时 Some）
    let mut cur: Option<PcfComponent> = None;
    // 最近一个打开的标签名（文本事件按它归位到字段）
    let mut tag = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "Component" {
                    cur = Some(PcfComponent::default());
                }
                tag = name;
            }
            Ok(Event::Empty(_)) => {
                // 自闭合标签（无文本内容）：清掉当前标签名，避免后续文本误归位
                tag.clear();
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "Component" {
                    if let Some(c) = cur.take() {
                        out.components.push(c);
                    }
                }
                tag.clear();
            }
            Ok(Event::Text(t)) => {
                // decode 同时处理 XML 实体转义（&amp; 等）与编码
                let text = t
                    .decode()
                    .map_err(|e| format!("文本解码失败（位置 {}）: {}", reader.buffer_position(), e))?
                    .into_owned();
                match cur.as_mut() {
                    // ---- Component 节点内：成份券字段 ----
                    Some(c) => match tag.as_str() {
                        "UnderlyingSecurityID" => c.security_id = text,
                        "UnderlyingSecurityIDSource" => c.security_id_source = text,
                        "ComponentShare" => c.component_share = parse_num(&text),
                        "SubstituteFlag" => c.substitute_flag = text.trim().parse().unwrap_or(0),
                        "CreationCashSubstitute" => c.creation_cash_substitute = parse_num(&text),
                        "RedemptionCashSubstitute" => c.redemption_cash_substitute = parse_num(&text),
                        _ => {}
                    },
                    // ---- 顶层：基金代码与最小申赎单位 ----
                    None => match tag.as_str() {
                        "SecurityID" => out.security_id = text,
                        "CreationRedemptionUnit" => out.creation_redemption_unit = parse_num(&text),
                        _ => {}
                    },
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(format!("XML 解析失败（位置 {}）: {}", reader.buffer_position(), e))
            }
            _ => {}
        }
    }
    // 文件在 Component 中间截断时 quick-xml 不会自动报错，这里主动兜底
    if cur.is_some() {
        return Err("XML 不完整：Component 节点未闭合".into());
    }
    Ok(out)
}

/// 取标签的本地名（忽略命名空间前缀；PCF 文件用默认命名空间
/// `xmlns="http://ts.szse.cn/Fund"`，元素名本身无前缀）。
fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.into_owned(),
    }
}

/// 数值字段解析：允许前后空白，失败按 0 处理（宽容脏数据）
fn parse_num(s: &str) -> f64 {
    s.trim().parse().unwrap_or(0.0)
}

/// 在目录中查找指定 ETF 代码**日期最新**的清单文件。
///
/// 文件名须匹配 `pcf_<代码>_<YYYYMMDD>.xml`（日期为 8 位数字；
/// 大小写不敏感，兼容 `PCF_`/`.XML` 写法）。日期是定长
/// YYYYMMDD，直接转数值比较取最大。
///
/// 注意：`read_dir` 是“列出目录全部条目”的系统调用（POSIX 无按前缀
/// 枚举的接口），通配符/正则只是用户态过滤、省不了 I/O，因此单次扫描
/// 已尽量便宜：先按**期望长度**预检（长度不符的条目一次比较即排除），
/// 再用字节切片 `eq_ignore_ascii_case` 比较（零拷贝零分配，
/// 文件名合法 UTF-8 时 `to_string_lossy` 也是借用）。重复调用应走
/// [`PcfCache`]，避免每笔委托都全量遍历大目录。
///
/// 返回 `Ok(Some(路径))` = 找到；`Ok(None)` = 目录可读但没有该代码的
/// 清单文件；`Err` = 目录不存在/不可读（调用方据此推送不同告警）。
pub fn find_latest_pcf(dir: &Path, security_id: &str) -> Result<Option<PathBuf>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("读取目录失败: {}", e))?;
    let code = security_id.trim();
    if code.is_empty() {
        return Ok(None);
    }
    // 期望文件名形态："pcf_" + 代码 + "_" + 8 位日期 + ".xml"
    let need = 4 + code.len() + 1 + 8 + 4;
    let code_b = code.as_bytes();
    let mut best: Option<(u64, PathBuf)> = None; // (日期数值, 路径)，数值序 = 时间序
    for ent in entries.flatten() {
        let name = ent.file_name();
        // 合法 UTF-8 文件名零拷贝借用；非 UTF-8（不可能匹配）会被后续比较排除
        let name = name.to_string_lossy();
        let b = name.as_bytes();
        if b.len() != need
            || !b[..4].eq_ignore_ascii_case(b"pcf_")
            || !b[4..4 + code_b.len()].eq_ignore_ascii_case(code_b)
            || b[4 + code_b.len()] != b'_'
            || !b[5 + code_b.len()..need - 4].iter().all(|c| c.is_ascii_digit())
            || !b[need - 4..].eq_ignore_ascii_case(b".xml")
        {
            continue;
        }
        // 8 位数字直接折成数值（最大 99999999，不会溢出）
        let date = b[5 + code_b.len()..need - 4]
            .iter()
            .fold(0u64, |acc, c| acc * 10 + u64::from(*c - b'0'));
        if best.as_ref().is_none_or(|(d, _)| *d < date) {
            best = Some((date, ent.path()));
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// 读取并解析一份清单文件（[`load_latest_pcf`] 与 [`PcfCache`] 共用）。
fn read_and_parse(path: &Path) -> Result<PcfFile, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("读取文件失败: {}", e))?;
    parse_pcf(&content).map_err(|e| format!("{}: {}", path.display(), e))
}

/// 查找并解析指定 ETF 代码最新的清单文件（**无缓存**版本；
/// 会话收单路径应使用 [`PcfCache::load`]，本函数保留给测试与一次性调用）。
///
/// 返回 `Ok(Some((路径, 解析结果)))` = 成功（路径用于日志展示用了哪份清单）；
/// `Ok(None)` = 目录可读但无该代码的清单文件；`Err` = 目录/文件读取失败
/// 或 XML 解析失败。
pub fn load_latest_pcf(dir: &Path, security_id: &str) -> Result<Option<(PathBuf, PcfFile)>, String> {
    let Some(path) = find_latest_pcf(dir, security_id)? else {
        return Ok(None);
    };
    let pcf = read_and_parse(&path)?;
    Ok(Some((path, pcf)))
}

/// 目录扫描的兜底有效期：个别文件系统（NFS 客户端属性缓存、某些
/// FUSE/网络盘）目录 mtime 更新可能不及时，超过该时长强制重扫一次，
/// 保证新增清单文件最迟 30 秒内被发现；本地 ext4/xfs 几乎不触发。
const RESCAN_BACKSTOP: Duration = Duration::from_secs(30);

/// 一个 (目录, 代码) 的缓存条目。
struct CachedPcf {
    /// 上次扫描时目录的 mtime（判断目录内容是否变化）
    dir_mtime: Option<SystemTime>,
    /// 上次扫描时间（兜底重扫用）
    scanned_at: Instant,
    /// 命中结果；None = 当时目录里没有该代码的清单文件（负缓存）
    found: Option<FoundPcf>,
}

/// 命中并已解析的清单文件。
struct FoundPcf {
    path: PathBuf,
    /// 文件 mtime + 大小：判断文件是否被改过（同名文件盘中更新也能发现）
    file_mtime: Option<SystemTime>,
    file_len: u64,
    /// 已解析的清单（Arc 共享，命中时零拷贝复用）
    pcf: Arc<PcfFile>,
}

/// PCF 清单查找/解析缓存（按 (目录, 代码) 记忆）。
///
/// 挂在 `SessionCtx`（每个平台一份，网关重启自然清空），避免每笔 ETF
/// 申赎委托都对清单目录做全量扫描并重新读取解析——清单目录里可能有
/// 所有 ETF 代码多日的文件，全量遍历会很慢：
/// - **目录 mtime 未变**且距上次扫描未超兜底时长 → 直接复用缓存结果
///   （负缓存同样命中：该代码没有清单文件时也不再重复扫描）；
/// - **目录变化**（新增/删除文件）→ 重扫一次目录；若解析出的文件未变
///   （同路径且 mtime/大小一致，例如只是别人加了其他代码的文件）仍复用
///   已解析结果，不重读不重解析；新增清单文件下一笔委托即时生效；
/// - **同名文件内容更新**（文件 mtime/大小变化，盘中更新清单）→ 重新
///   读取解析。
///
/// 稳态下每笔委托只有 2 次 `stat`（目录 + 文件，微秒级），无目录遍历、
/// 无文件读取、无 XML 解析。
#[derive(Default)]
pub struct PcfCache {
    entries: HashMap<(String, String), CachedPcf>,
}

impl PcfCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取指定代码日期最新的清单文件（带缓存），语义与
    /// [`load_latest_pcf`] 一致，但命中缓存时不访问目录/不重新解析。
    pub fn load(
        &mut self,
        dir: &Path,
        security_id: &str,
    ) -> Result<Option<(PathBuf, Arc<PcfFile>)>, String> {
        let code = security_id.trim();
        if code.is_empty() {
            return Ok(None);
        }
        let key = (dir.to_string_lossy().into_owned(), code.to_string());
        // 目录本身必须可读（与无缓存版同样的错误语义：目录不存在/不可读报错）
        let dir_mtime = std::fs::metadata(dir)
            .map_err(|e| format!("读取目录失败: {}", e))?
            .modified()
            .ok();

        // ---- 1) 快速路径：目录 mtime 未变且未超兜底时长 → 直接复用缓存 ----
        let fresh = self.entries.get(&key).is_some_and(|c| {
            c.dir_mtime == dir_mtime && c.scanned_at.elapsed() < RESCAN_BACKSTOP
        });
        let found_path = if fresh {
            match &self.entries[&key].found {
                None => return Ok(None), // 负缓存命中：仍无该代码的清单文件
                Some(f) => f.path.clone(),
            }
        } else {
            // ---- 2) 失效：重扫目录，找该代码日期最新的文件 ----
            match find_latest_pcf(dir, code)? {
                None => {
                    // 负缓存：目录内容没再变之前不必重复扫描
                    self.entries.insert(
                        key,
                        CachedPcf { dir_mtime, scanned_at: Instant::now(), found: None },
                    );
                    return Ok(None);
                }
                Some(p) => p,
            }
        };

        // ---- 3) 文件级校验：同路径且 mtime/大小未变 → 复用已解析结果 ----
        let meta = std::fs::metadata(&found_path).map_err(|e| format!("读取文件失败: {}", e))?;
        let (file_mtime, file_len) = (meta.modified().ok(), meta.len());
        let reused = self
            .entries
            .get(&key)
            .and_then(|c| c.found.as_ref())
            .filter(|f| f.path == found_path && f.file_mtime == file_mtime && f.file_len == file_len)
            .map(|f| f.pcf.clone()); // 先克隆 Arc 结束对 entries 的借用，再更新缓存时间
        if let Some(pcf) = reused {
            // 文件未变（目录可能变过，只是本代码的文件没动）：
            // 刷新目录扫描时间后直接复用，不读文件不解析
            if let Some(c) = self.entries.get_mut(&key) {
                c.dir_mtime = dir_mtime;
                c.scanned_at = Instant::now();
            }
            return Ok(Some((found_path, pcf)));
        }

        // ---- 4) 读文件 + 解析并写入缓存 ----
        let pcf = Arc::new(read_and_parse(&found_path)?);
        self.entries.insert(
            key,
            CachedPcf {
                dir_mtime,
                scanned_at: Instant::now(),
                found: Some(FoundPcf {
                    path: found_path.clone(),
                    file_mtime,
                    file_len,
                    pcf: pcf.clone(),
                }),
            },
        );
        Ok(Some((found_path, pcf)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份贴近真实交易所文件的测试清单（含“申赎现金”与普通成份券）
    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<PCFFile xmlns="http://ts.szse.cn/Fund">
  <Version>1.0</Version>
  <SecurityID>159901</SecurityID>
  <SecurityIDSource>102</SecurityIDSource>
  <Symbol>深证100ETF易方达</Symbol>
  <UnderlyingSecurityID>399330</UnderlyingSecurityID>
  <CreationRedemptionUnit>700000.00</CreationRedemptionUnit>
  <Components>
    <Component>
      <UnderlyingSecurityID>159900</UnderlyingSecurityID>
      <UnderlyingSecurityIDSource>102</UnderlyingSecurityIDSource>
      <UnderlyingSymbol>申赎现金</UnderlyingSymbol>
      <ComponentShare>0.00</ComponentShare>
      <SubstituteFlag>2</SubstituteFlag>
      <CreationCashSubstitute>10000.0000</CreationCashSubstitute>
      <RedemptionCashSubstitute>200.0000</RedemptionCashSubstitute>
    </Component>
    <Component>
      <UnderlyingSecurityID>000001</UnderlyingSecurityID>
      <UnderlyingSecurityIDSource>102</UnderlyingSecurityIDSource>
      <UnderlyingSymbol>平安银行</UnderlyingSymbol>
      <ComponentShare>2200.00</ComponentShare>
      <SubstituteFlag>1</SubstituteFlag>
      <PremiumRatio>0.34000</PremiumRatio>
      <CreationCashSubstitute>0.0000</CreationCashSubstitute>
      <RedemptionCashSubstitute>0.0000</RedemptionCashSubstitute>
    </Component>
  </Components>
</PCFFile>"#;

    /// 更新后的清单（模拟盘中更新：最小申赎单位与成份券数量都不同，
    /// 且文件长度与 SAMPLE 不同，保证长度比较即可发现变化）
    const SAMPLE_UPDATED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<PCFFile xmlns="http://ts.szse.cn/Fund">
  <SecurityID>159901</SecurityID>
  <CreationRedemptionUnit>800000.00</CreationRedemptionUnit>
  <Components>
    <Component>
      <UnderlyingSecurityID>000002</UnderlyingSecurityID>
      <UnderlyingSecurityIDSource>102</UnderlyingSecurityIDSource>
      <ComponentShare>1700.00</ComponentShare>
      <SubstituteFlag>1</SubstituteFlag>
    </Component>
  </Components>
</PCFFile>"#;

    #[test]
    fn parse_sample_pcf() {
        let pcf = parse_pcf(SAMPLE).expect("解析必须成功");
        assert_eq!(pcf.security_id, "159901");
        assert_eq!(pcf.creation_redemption_unit, 700000.0);
        assert_eq!(pcf.components.len(), 2);
        let cash = &pcf.components[0];
        assert_eq!(cash.security_id, "159900");
        assert_eq!(cash.security_id_source, "102");
        assert_eq!(cash.substitute_flag, 2);
        assert_eq!(cash.component_share, 0.0);
        assert_eq!(cash.creation_cash_substitute, 10000.0);
        assert_eq!(cash.redemption_cash_substitute, 200.0);
        let stock = &pcf.components[1];
        assert_eq!(stock.security_id, "000001");
        assert_eq!(stock.component_share, 2200.0);
        assert_eq!(stock.substitute_flag, 1);
        // 顶层 UnderlyingSecurityID（拟合指数）不能污染成份券字段
        assert_eq!(stock.creation_cash_substitute, 0.0);
    }

    #[test]
    fn parse_broken_xml_errors() {
        // 标签不匹配：quick-xml 直接报错
        assert!(parse_pcf("<PCFFile><Components></PCFFile>").is_err());
        // 文件截断在 Component 中间：靠 EOF 兜底检查报错
        assert!(parse_pcf("<PCFFile><Components><Component>").is_err());
    }

    #[test]
    fn find_latest_picks_max_date() {
        let dir = std::env::temp_dir().join(format!("simx_pcf_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 同代码多日文件 + 干扰文件（其他代码、错误日期、无日期）
        for name in [
            "pcf_159901_20260612.xml",
            "pcf_159901_20260615.xml",
            "pcf_159901_20260614.xml",
            "pcf_159920_20260615.xml",
            "pcf_159901_2026061.xml",
            "pcf_159901.xml",
            "readme.txt",
        ] {
            std::fs::write(dir.join(name), "<x/>").unwrap();
        }
        let found = find_latest_pcf(&dir, "159901").unwrap().expect("应找到文件");
        assert_eq!(
            found.file_name().unwrap().to_string_lossy(),
            "pcf_159901_20260615.xml"
        );
        // 其他代码互不干扰；不存在的代码返回 None
        assert!(find_latest_pcf(&dir, "510300").unwrap().is_none());
        // 目录不存在返回 Err（调用方据此告警“目录格式错误”）
        assert!(find_latest_pcf(&dir.join("no_such_dir"), "159901").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_latest_parses_file() {
        let dir = std::env::temp_dir().join(format!("simx_pcf_load_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pcf_159901_20260615.xml"), SAMPLE).unwrap();
        let (path, pcf) = load_latest_pcf(&dir, "159901").unwrap().expect("应找到并解析");
        assert!(path.ends_with("pcf_159901_20260615.xml"));
        assert_eq!(pcf.components.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓存命中：目录未变化时应复用已解析结果（Arc 指针相同，不重读不重解析）
    #[test]
    fn cache_reuses_parsed_result() {
        let dir = std::env::temp_dir().join(format!("simx_pcf_cache_hit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pcf_159901_20260615.xml"), SAMPLE).unwrap();

        let mut cache = PcfCache::new();
        let (p1, a1) = cache.load(&dir, "159901").unwrap().expect("首次应找到");
        let (p2, a2) = cache.load(&dir, "159901").unwrap().expect("二次应命中缓存");
        assert_eq!(p1, p2);
        assert!(Arc::ptr_eq(&a1, &a2), "目录未变时应复用同一份解析结果");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同名文件内容更新（长度变化）时应重新读取解析，不返回旧数据
    #[test]
    fn cache_reparses_when_file_updated() {
        let dir = std::env::temp_dir().join(format!("simx_pcf_cache_upd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pcf_159901_20260615.xml");
        std::fs::write(&path, SAMPLE).unwrap();

        let mut cache = PcfCache::new();
        let (_, first) = cache.load(&dir, "159901").unwrap().unwrap();
        assert_eq!(first.components.len(), 2);

        // 盘中更新：同名文件替换为不同内容（长度不同，必然触发重新解析）
        std::fs::write(&path, SAMPLE_UPDATED).unwrap();
        let (_, second) = cache.load(&dir, "159901").unwrap().unwrap();
        assert_eq!(second.components.len(), 1, "文件更新后应重新解析");
        assert_eq!(second.creation_redemption_unit, 800000.0);
        assert!(!Arc::ptr_eq(&first, &second));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 负缓存：无清单文件时返回 None，文件出现后（目录 mtime 变化）立即找到
    #[test]
    fn cache_negative_then_file_appears() {
        let dir = std::env::temp_dir().join(format!("simx_pcf_cache_neg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut cache = PcfCache::new();
        assert!(cache.load(&dir, "159920").unwrap().is_none(), "无清单文件应为 None");
        std::fs::write(dir.join("pcf_159920_20260615.xml"), SAMPLE).unwrap();
        let (path, _) = cache.load(&dir, "159920").unwrap().expect("新增文件后应找到");
        assert!(path.ends_with("pcf_159920_20260615.xml"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录新增更新日期的文件后，下一笔即切换到日期最新的文件
    #[test]
    fn cache_picks_newest_date_after_dir_change() {
        let dir = std::env::temp_dir().join(format!("simx_pcf_cache_new_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pcf_159901_20260612.xml"), SAMPLE).unwrap();

        let mut cache = PcfCache::new();
        let (p1, _) = cache.load(&dir, "159901").unwrap().unwrap();
        assert!(p1.ends_with("pcf_159901_20260612.xml"));

        // 新一天的文件落盘（目录 mtime 变化）→ 立即切换到新文件
        std::fs::write(dir.join("pcf_159901_20260615.xml"), SAMPLE_UPDATED).unwrap();
        let (p2, a2) = cache.load(&dir, "159901").unwrap().unwrap();
        assert!(p2.ends_with("pcf_159901_20260615.xml"), "应切换到日期最新的文件");
        assert_eq!(a2.creation_redemption_unit, 800000.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
