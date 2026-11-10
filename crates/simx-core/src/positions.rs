//! ETF 申购持仓同步与扣减。
//!
//! # 职责
//!
//! 1. 通过 Oracle 查询资金账号的持仓（[`query_oracle`]，同步阻塞函数，
//!    由上层放进 tokio 的 blocking 线程池执行，不占用异步工作线程）；
//! 2. 在内存中维护**唯一一份**已同步持仓（[`PositionStore`]，全局单例式
//!    共享：引擎创建一次，所有网关/平台会话共用同一个 Arc）；
//! 3. 收到需要实物交付的 ETF 申购（深圳 101201 / 上海综合 IEC）时，
//!    按 PCF 清单与持仓计算每只成份券的“实际过户数量 / 现金替代金额”
//!    （[`PositionStore::adjust_creation_pcf`]）。
//!
//! # 业务规则（与界面「同步持仓数据」配套）
//!
//! - 现金替代标志 **0（禁止）**：必须全部用持仓过户。任一成份券的
//!   可申赎数量不足 → 整笔委托拒单；
//! - 标志 **1（允许）**：优先扣减持仓，不足部分用现金替代；
//! - 标志 **2（必须）/ 3（退补）**：不扣持仓，全部用现金替代；
//! - 未同步过持仓（`rows == None`）时不启用本功能，回报与旧行为一致；
//!   同步并保存后，持仓以最新一份为准（整份替换，不做增量累加）。
//!
//! # 为什么调整的是一份 PcfFile 克隆
//!
//! 深交所与上海综合网关的回报生成器都只认 [`PcfFile`]：深交所把
//! `component_share` 与替代金额直接写进 201202 成份股明细，上海综合按
//! 替代标志汇总类型 2/3。把“可用持仓”折算回每篮子字段（数量按覆盖比例
//! 缩小、替代金额按未覆盖比例保留）后，两个网关的既有报文构造代码无需
//! 知道持仓的存在，规则只在本模块实现一次。

use crate::pcf::PcfFile;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Oracle 连接配置（界面弹窗回填与查询用，serde → camelCase）。
///
/// 密码只保存在服务端进程内存中，不写入任何配置文件/日志；
/// 字段与前端 `types.ts` 的 `OracleConnectConfig` 手工对应。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OracleConnectConfig {
    /// 环境名（仅界面标识，如 “qa环境”，不参与连接）
    pub env_name: String,
    /// EZConnect 连接串，如 `10.210.50.63:2151/spdtrderbak`
    pub connect_string: String,
    /// 登录用户名
    pub user: String,
    /// 登录密码
    pub password: String,
}

/// 一条持仓记录（字段对应查询 SQL 的六列）。
///
/// 数量统一用 f64：Oracle NUMBER 可能是小数，且后续按篮子数折算时
/// 本来就需要浮点运算；界面以“股”为单位展示与编辑。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PositionRow {
    /// 证券代码（TRA_SEC_POSITION.SYMBOL，如 "300122"）
    pub security_id: String,
    /// 证券名称（EX_TRADING_SECURITY.NAME）
    pub security_name: String,
    /// 持有数量
    pub holding_qty: f64,
    /// 可用数量
    pub available_qty: f64,
    /// 可申赎数量（ETF 申购扣减只认这个字段）
    pub creation_redemption_qty: f64,
    /// 冻结数量
    pub frozen_qty: f64,
}

/// 前端打开“同步持仓数据”弹窗时拉取的快照。
///
/// `synced == false` 表示服务端还没有保存过持仓（ETF 申购不扣减）；
/// 连接信息与资金账号无论是否查询过都回填最后一次使用的值。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PositionSnapshot {
    pub oracle: OracleConnectConfig,
    pub account_id: String,
    /// 是否已经保存过一份持仓（true 时 rows 为服务端内存中的当前持仓）
    pub synced: bool,
    pub rows: Vec<PositionRow>,
}

/// 持仓存储内部状态（由 Mutex 保护，见 [`PositionStore`]）。
#[derive(Default)]
struct StoreInner {
    oracle: OracleConnectConfig,
    account_id: String,
    /// None = 从未保存过持仓；Some = 当前生效的唯一一份持仓
    rows: Option<Vec<PositionRow>>,
}

/// 全局唯一一份 ETF 持仓存储。
///
/// 引擎创建后以 `Arc<PositionStore>` 注入所有网关的 `SessionCtx`：
/// - 查询/保存命令从 API 线程访问；
/// - 每笔 ETF 申购在生成回报计划时短暂加锁扣减。
///
/// 用 `parking_lot::Mutex`（同步锁）而不是 tokio 的异步锁：临界区只有
/// 内存读写（几十条记录），没有 await；若用异步锁会让会话代码跨 await
/// 持有 guard，既不符合项目“非 Send guard 不跨 await”的约定，也会把
/// 简单的内存操作复杂化。
#[derive(Default)]
pub struct PositionStore {
    inner: Mutex<StoreInner>,
}

impl PositionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 前端弹窗初始化：返回最后一次连接信息、资金账号与当前持仓。
    pub fn snapshot(&self) -> PositionSnapshot {
        let inner = self.inner.lock();
        match &inner.rows {
            Some(rows) => PositionSnapshot {
                oracle: inner.oracle.clone(),
                account_id: inner.account_id.clone(),
                synced: true,
                rows: rows.clone(),
            },
            None => PositionSnapshot {
                oracle: inner.oracle.clone(),
                account_id: inner.account_id.clone(),
                synced: false,
                rows: Vec::new(),
            },
        }
    }

    /// 查询成功后记住连接信息与资金账号（不替换持仓，持仓要等“保存”）。
    pub fn remember_query(&self, oracle: &OracleConnectConfig, account_id: &str) {
        let mut inner = self.inner.lock();
        inner.oracle = oracle.clone();
        inner.account_id = account_id.to_string();
    }

    /// 保存（整份替换）持仓数据：下一次 ETF 申购以新数据为准。
    pub fn replace(&self, oracle: OracleConnectConfig, account_id: String, rows: Vec<PositionRow>) {
        let mut inner = self.inner.lock();
        inner.oracle = oracle;
        inner.account_id = account_id;
        inner.rows = Some(rows);
    }

    /// 是否已经保存过持仓（ETF 申购是否启用持仓扣减）。
    pub fn is_synced(&self) -> bool {
        self.inner.lock().rows.is_some()
    }

    /// 按持仓调整一份**申购**用的 PCF 清单（赎回不调用本函数）。
    ///
    /// 返回的 `PcfFile` 中：
    /// - 禁止现金替代的成份券保持每篮子数量不变（调用前已校验持仓足够）；
    /// - 允许现金替代的成份券：`component_share` 缩为实际覆盖部分，
    ///   申购替代金额缩为未覆盖部分；
    /// - 必须/退补现金替代的成份券：`component_share` 置 0，替代金额不变。
    ///
    /// 同时在内存持仓中扣减实际覆盖的“可申赎数量”。
    /// 未同步过持仓时原样返回（功能未启用），不做任何扣减。
    ///
    /// 返回 `Err(原因)` 表示存在禁止现金替代的成份券持仓不足，调用方
    /// 应整笔拒单（此时尚未扣减任何持仓，不会产生“半笔扣款”）。
    pub fn adjust_creation_pcf(&self, pcf: &PcfFile, etf_shares: f64) -> Result<PcfFile, String> {
        let mut inner = self.inner.lock();
        if inner.rows.is_none() {
            return Ok(pcf.clone()); // 未同步持仓：不启用扣减
        }
        // 篮子数 = 申购份额 ÷ 最小申赎单位；无效清单/数量时保持原样
        let baskets = if pcf.creation_redemption_unit > 0.0 && etf_shares > 0.0 {
            etf_shares / pcf.creation_redemption_unit
        } else {
            return Ok(pcf.clone());
        };
        let rows = inner.rows.as_ref().expect("上面已判断为 Some");
        // 代码 → 持仓行下标（同一代码出现多行时取第一行，与界面展示一致）
        let mut indices: HashMap<&str, usize> = HashMap::with_capacity(rows.len());
        for (i, r) in rows.iter().enumerate() {
            indices.entry(r.security_id.trim()).or_insert(i);
        }
        let available = |code: &str| -> f64 {
            indices
                .get(code.trim())
                .map(|&i| rows[i].creation_redemption_qty)
                .unwrap_or(0.0)
        };

        // ---- 第一遍：校验“禁止现金替代”的成份券必须全部有持仓 ----
        // 先全量校验再扣减，避免某只券不足时前面的券已经被扣掉
        for c in &pcf.components {
            if is_forbidden_flag(c.substitute_flag) && c.component_share > 0.0 {
                let required = c.component_share * baskets;
                let have = available(&c.security_id);
                if have + POSITION_EPS < required {
                    return Err(format!(
                        "成份股 {} 不允许现金替代，可申赎数量不足（需要 {:.0}，当前 {:.0}）",
                        c.security_id, required, have
                    ));
                }
            }
        }

        // ---- 第二遍：生成调整后的清单并累计扣减量 ----
        let mut adjusted = pcf.clone();
        // 代码 → 应扣减的可申赎数量（同一代码出现在多个成份券项时累加）。
        // 用 String 而不是 &str：这里一边遍历 &mut components 一边记录代码，
        // 借用列表元素的 &str 会与后续可变借用冲突
        let mut deductions: HashMap<String, f64> = HashMap::new();
        for c in &mut adjusted.components {
            match c.substitute_flag {
                // 允许现金替代：能覆盖多少过户多少，剩余全部现金替代
                1 if c.component_share > 0.0 => {
                    let required = c.component_share * baskets;
                    let have = available(&c.security_id);
                    let covered = have.max(0.0).min(required);
                    let covered_ratio = covered / required;
                    c.component_share *= covered_ratio;
                    // 替代金额是“每篮子该成份券整体”的金额，按未覆盖比例保留；
                    // 申购只用 CreationCashSubstitute，赎回字段同步缩放避免误用
                    c.creation_cash_substitute *= 1.0 - covered_ratio;
                    c.redemption_cash_substitute *= 1.0 - covered_ratio;
                    deductions
                        .entry(c.security_id.clone())
                        .and_modify(|v| *v += covered)
                        .or_insert(covered);
                }
                // 必须现金替代（2）/ 退补现金替代（3）：不扣持仓，全部现金替代
                2 | 3 => {
                    c.component_share = 0.0;
                }
                // 禁止现金替代（0）与未知标志：数量保持不变（已通过第一遍
                // 校验），按需扣减实际过户的持仓
                _ => {
                    let required = c.component_share * baskets;
                    if required > 0.0 {
                        deductions
                            .entry(c.security_id.clone())
                            .and_modify(|v| *v += required)
                            .or_insert(required);
                    }
                }
            }
        }

        // 写回扣减后的可申赎数量（扣到 0 为止，负数只可能来自浮点误差）
        if let Some(rows) = inner.rows.as_mut() {
            // 用 String 键重建索引：上面还想同时读 rows，但接下来要可变借用
            let mut idx: HashMap<String, usize> = HashMap::with_capacity(rows.len());
            for (i, r) in rows.iter().enumerate() {
                idx.entry(r.security_id.trim().to_string()).or_insert(i);
            }
            for (code, qty) in deductions {
                if let Some(&i) = idx.get(&code) {
                    rows[i].creation_redemption_qty =
                        (rows[i].creation_redemption_qty - qty).max(0.0);
                }
            }
        }
        Ok(adjusted)
    }
}

/// 浮点比较容差：PCF 数量与持仓数量都是整数股，折算后只可能有
/// 极小的二进制浮点误差；用 1e-6 判断“刚好够”。
const POSITION_EPS: f64 = 1e-6;

/// 是否“禁止现金替代”（标志 0；未知值按最保守的禁止处理）。
fn is_forbidden_flag(flag: u8) -> bool {
    !matches!(flag, 1 | 2 | 3)
}

/// Oracle 查询语句：与界面需求给定的 SQL 一致，资金账号用绑定变量传入
/// （避免拼接字符串产生 SQL 注入）。
const POSITION_SQL: &str = "SELECT tsp.SYMBOL, ets.NAME, tsp.HOLDING_QTY, \
     tsp.AVAILABLE_QTY, tsp.CREATION_REDEMPTION_QTY, tsp.FROZEN_QTY \
     FROM TRA_SEC_POSITION tsp \
     INNER JOIN EX_TRADING_SECURITY ets ON tsp.SYMBOL = ets.SYMBOL \
     WHERE ACCOUNT_ID = :1";

/// 从 Oracle 查询指定资金账号的全部持仓（**阻塞函数**）。
///
/// # 线程模型
///
/// rust-oracle 是同步/阻塞驱动，必须在 tokio 的 blocking 线程池里调用
/// （`tokio::task::spawn_blocking`），不能直接在 async 任务中执行——
/// 否则数据库慢查询会占住异步工作线程，拖慢所有 TCP 网关与前端命令。
///
/// # 运行时依赖
///
/// `oracle` crate 通过 ODPI-C 在运行时动态加载 Oracle Client 库
/// （Windows 需安装 Oracle Instant Client，Linux 需配置 LD_LIBRARY_PATH）；
/// 未安装时连接会返回错误，前端弹窗会展示错误文本，不影响其他网关。
pub fn query_oracle(
    cfg: &OracleConnectConfig,
    account_id: &str,
) -> Result<Vec<PositionRow>, String> {
    let account = account_id.trim();
    if cfg.connect_string.trim().is_empty() {
        return Err("Oracle 连接串不能为空".into());
    }
    if cfg.user.trim().is_empty() {
        return Err("Oracle 用户名不能为空".into());
    }
    if account.is_empty() {
        return Err("资金账号不能为空".into());
    }
    let conn = oracle::Connection::connect(&cfg.user, &cfg.password, &cfg.connect_string)
        .map_err(|e| format!("连接 Oracle 失败：{}", e))?;
    let rows = conn
        .query(POSITION_SQL, &[&account])
        .map_err(|e| format!("查询持仓失败：{}", e))?;
    let mut out = Vec::new();
    for row in rows {
        let row = row.map_err(|e| format!("读取持仓行失败：{}", e))?;
        // 数值列允许 NULL（按 0 处理）；代码/名称去尾部空格（CHAR 定长列）
        let security_id: String = row.get(0).unwrap_or_default();
        let security_name: Option<String> = row.get(1).unwrap_or(None);
        let holding_qty: Option<f64> = row.get(2).unwrap_or(None);
        let available_qty: Option<f64> = row.get(3).unwrap_or(None);
        let creation_redemption_qty: Option<f64> = row.get(4).unwrap_or(None);
        let frozen_qty: Option<f64> = row.get(5).unwrap_or(None);
        out.push(PositionRow {
            security_id: security_id.trim().to_string(),
            security_name: security_name.unwrap_or_default().trim().to_string(),
            holding_qty: holding_qty.unwrap_or(0.0),
            available_qty: available_qty.unwrap_or(0.0),
            creation_redemption_qty: creation_redemption_qty.unwrap_or(0.0),
            frozen_qty: frozen_qty.unwrap_or(0.0),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pcf::{PcfComponent, PcfFile};

    /// 构造一份测试清单：最小申赎单位 1,000,000 份（1 个篮子）
    fn test_pcf() -> PcfFile {
        PcfFile {
            security_id: "510300".into(),
            creation_redemption_unit: 1_000_000.0,
            mechanism: 0,
            components: vec![
                // 禁止现金替代：必须全有持仓
                PcfComponent {
                    security_id: "600001".into(),
                    component_share: 1000.0,
                    substitute_flag: 0,
                    ..Default::default()
                },
                // 允许现金替代：优先扣持仓
                PcfComponent {
                    security_id: "300122".into(),
                    component_share: 8000.0,
                    substitute_flag: 1,
                    creation_cash_substitute: 100_000.0,
                    redemption_cash_substitute: 90_000.0,
                    ..Default::default()
                },
                // 必须现金替代：不扣持仓
                PcfComponent {
                    security_id: "600002".into(),
                    component_share: 0.0,
                    substitute_flag: 2,
                    creation_cash_substitute: 50_000.0,
                    ..Default::default()
                },
                // 退补现金替代：不扣持仓、金额照算
                PcfComponent {
                    security_id: "600003".into(),
                    component_share: 500.0,
                    substitute_flag: 3,
                    creation_cash_substitute: 20_000.0,
                    ..Default::default()
                },
            ],
        }
    }

    fn row(code: &str, qty: f64) -> PositionRow {
        PositionRow {
            security_id: code.into(),
            security_name: format!("{}名称", code),
            holding_qty: qty,
            available_qty: qty,
            creation_redemption_qty: qty,
            frozen_qty: 0.0,
        }
    }

    /// 未同步持仓：清单原样返回，不做任何扣减
    #[test]
    fn no_position_sync_keeps_pcf() {
        let store = PositionStore::new();
        let pcf = test_pcf();
        let out = store.adjust_creation_pcf(&pcf, 1_000_000.0).unwrap();
        assert_eq!(out.components[1].component_share, 8000.0);
        assert_eq!(out.components[1].creation_cash_substitute, 100_000.0);
    }

    /// 禁止现金替代的成份券不足：整笔拒单，且不扣减任何持仓
    #[test]
    fn forbidden_component_insufficient_rejects_without_deduction() {
        let store = PositionStore::new();
        store.replace(
            OracleConnectConfig::default(),
            "60500052".into(),
            vec![row("600001", 999.0), row("300122", 8000.0)],
        );
        let err = store
            .adjust_creation_pcf(&test_pcf(), 1_000_000.0)
            .unwrap_err();
        assert!(err.contains("600001"), "错误信息应指出不足的证券：{}", err);
        let snap = store.snapshot();
        assert_eq!(
            snap.rows
                .iter()
                .find(|r| r.security_id == "600001")
                .unwrap()
                .creation_redemption_qty,
            999.0,
            "拒单时不能扣减持仓"
        );
        assert_eq!(
            snap.rows
                .iter()
                .find(|r| r.security_id == "300122")
                .unwrap()
                .creation_redemption_qty,
            8000.0,
            "拒单时后续成份券也不能被扣减"
        );
    }

    /// 允许现金替代：持仓 3000 / 需要 8000 → 过户 3000，剩 5000 转现金替代
    #[test]
    fn allowed_component_partially_deducts_and_cash_substitutes() {
        let store = PositionStore::new();
        store.replace(
            OracleConnectConfig::default(),
            "60500052".into(),
            vec![row("600001", 1000.0), row("300122", 3000.0)],
        );
        let out = store.adjust_creation_pcf(&test_pcf(), 1_000_000.0).unwrap();
        let c = &out.components[1];
        assert_eq!(c.component_share, 3000.0, "过户数量应为可用持仓 3000");
        // 未覆盖比例 5000/8000 = 0.625
        assert_eq!(c.creation_cash_substitute, 62_500.0);
        assert_eq!(c.redemption_cash_substitute, 56_250.0);
        // 必须/退补现金替代不扣持仓、数量置 0、金额保持
        assert_eq!(out.components[2].component_share, 0.0);
        assert_eq!(out.components[2].creation_cash_substitute, 50_000.0);
        assert_eq!(out.components[3].component_share, 0.0);
        assert_eq!(out.components[3].creation_cash_substitute, 20_000.0);
        let snap = store.snapshot();
        assert_eq!(
            snap.rows
                .iter()
                .find(|r| r.security_id == "300122")
                .unwrap()
                .creation_redemption_qty,
            0.0,
            "第二次申购时 300122 已无持仓"
        );
        assert_eq!(
            snap.rows
                .iter()
                .find(|r| r.security_id == "600001")
                .unwrap()
                .creation_redemption_qty,
            0.0,
            "禁止现金替代的成份券按需扣减"
        );
    }

    /// 连续两笔申购：第一笔扣 3000，第二笔全部现金替代（0 过户）
    #[test]
    fn second_creation_uses_remaining_position() {
        let store = PositionStore::new();
        store.replace(
            OracleConnectConfig::default(),
            "60500052".into(),
            vec![row("600001", 2000.0), row("300122", 3000.0)],
        );
        let first = store.adjust_creation_pcf(&test_pcf(), 1_000_000.0).unwrap();
        assert_eq!(first.components[1].component_share, 3000.0);
        let second = store.adjust_creation_pcf(&test_pcf(), 1_000_000.0).unwrap();
        assert_eq!(second.components[1].component_share, 0.0);
        assert_eq!(second.components[1].creation_cash_substitute, 100_000.0);
    }

    /// 保存新持仓后整份替换旧持仓（不累加）
    #[test]
    fn replace_clears_previous_positions() {
        let store = PositionStore::new();
        store.replace(
            OracleConnectConfig::default(),
            "60500052".into(),
            vec![row("300122", 8000.0)],
        );
        store.replace(
            OracleConnectConfig::default(),
            "60500052".into(),
            vec![row("300122", 1000.0)],
        );
        let snap = store.snapshot();
        assert_eq!(snap.rows.len(), 1);
        assert_eq!(snap.rows[0].creation_redemption_qty, 1000.0);
    }

    /// 资金账号为 SQL 绑定变量：构造含特殊字符的账号不应影响 SQL 语义
    #[test]
    fn account_id_is_bound_not_interpolated() {
        // 只验证 SQL 常量里没有字符串插值（占位符存在），不连接数据库
        assert!(POSITION_SQL.contains(":1"));
        assert!(!POSITION_SQL.contains("{}"));
    }
}
