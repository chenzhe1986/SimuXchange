//! 配置模型：网关 / 平台 / 模拟回报策略。
//!
//! # 层级关系
//!
//! ```text
//! AppConfig（应用级：用本地引擎还是连远程后端）
//! GatewayConfig（交易网关，可建多个）
//!   └─ PlatformConfig（平台，每个平台监听一个 TCP 端口）
//!        └─ StrategyConfig（该平台收到委托后怎么回报）
//! ```
//!
//! # 关于 serde 注解
//!
//! 这些结构体会序列化成 JSON 与前端交互、保存到 gateways.json：
//! - `rename_all = "camelCase"`：Rust 的 snake_case 字段名（如 listen_host）
//!   在 JSON 中自动变成 camelCase（listenHost），与前端 TypeScript 习惯一致
//! - `default`：JSON 里缺少某字段时用默认值填充，而不是报错，
//!   这样日后新增配置项时旧配置文件仍能正常加载（向后兼容）

use serde::{Deserialize, Serialize};

/// 应用配置（simx.config.json，位于可执行程序目录）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    /// 当前选中的后端 id（"local" 或 backends 列表里某个远程项的 id）
    pub backend: String,
    /// 远程后端 WebSocket 地址（兼容旧字段：backends 列表为空时的回退值）
    pub remote_url: String,
    /// 已配置的后端列表（含固定项“本地引擎”），界面可随时切换
    pub backends: Vec<BackendEntry>,
    /// 更新服务器根地址（桌面端启动时自动检查新版本；留空则不检查）
    pub update_url: String,
}

/// 一个已配置的后端：本地引擎（固定）或远程 simx-server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BackendEntry {
    /// 唯一标识："local" 固定代表本地引擎，远程项由前端生成（如 b-xxxx）
    pub id: String,
    /// 显示名称（界面后端列表里展示）
    pub name: String,
    /// 类型：local（内嵌引擎）/ remote（远程 simx-server）
    pub kind: String,
    /// 远程地址（kind 为 remote 时有效）
    pub url: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            backend: "local".into(),
            remote_url: "ws://127.0.0.1:9800/ws".into(),
            backends: Vec::new(),
            update_url: String::new(),
        }
    }
}

impl Default for BackendEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            kind: "remote".into(),
            url: String::new(),
        }
    }
}

/// 网关分类：决定网关下所有平台说哪套协议。
/// serde 的 camelCase 重命名后，JSON 中分别是 "sz"、"shjj" 和 "shbond"；
/// 旧配置文件里没有这个字段时默认按深圳统一网关处理（向后兼容）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GatewayCategory {
    /// 深圳统一网关（深交所 Binary 协议）
    #[default]
    Sz,
    /// 上海竞价网关（上交所竞价平台 Binary 0.54 协议）
    Shjj,
    /// 上海新债券网关（上交所新债券平台 Binary 1.90 协议）
    Shbond,
}

/// 网关分类名称（日志与界面展示用）
pub fn gateway_category_name(c: GatewayCategory) -> &'static str {
    match c {
        GatewayCategory::Sz => "深圳统一网关",
        GatewayCategory::Shjj => "上海竞价网关",
        GatewayCategory::Shbond => "上海新债券网关",
    }
}

/// 交易网关配置。一个网关 = 一组平台的集合，启动/停止以网关为单位。
/// 上次运行状态（was_running）也存这里，与配置同文件（gateways.json），
/// 数据目录不再需要单独的 gateway_state.json。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GatewayConfig {
    /// 唯一标识（后端自动生成，前端新建时传空串）
    pub id: String,
    /// 网关名称，如 N000628Y0016
    pub name: String,
    /// 网关分类（深圳统一网关 / 上海竞价网关）
    pub category: GatewayCategory,
    pub platforms: Vec<PlatformConfig>,
    /// 上次退出时是否在运行（--auto-start 恢复依据：启动成功置 true，停止置 false）
    pub was_running: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "新建网关".into(),
            category: GatewayCategory::Sz,
            platforms: Vec::new(),
            was_running: false,
        }
    }
}

/// 平台配置（每个平台一个独立监听端口，柜台连到这个端口后登录、报单）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlatformConfig {
    /// 唯一标识（后端自动生成）
    pub id: String,
    /// 平台名称
    pub name: String,
    /// 平台号：1=现货集中竞价 2=综合金融服务 3=非交易处理
    /// 4=衍生品集中竞价 5=国际市场互联 6=固定收益
    pub platform_type: u16,
    /// 监听地址（0.0.0.0 表示接受任意网卡来的连接；127.0.0.1 仅限本机）
    pub listen_host: String,
    /// 监听端口
    pub port: u16,
    /// TGW 回复 Logon 时使用的 SenderCompID
    pub comp_id: String,
    /// 是否校验登录密码
    pub check_password: bool,
    /// 登录密码（check_password 为 true 时生效）
    pub password: String,
    /// 平台分区号（深：回报中的 PartitionNo；沪：分区号 SetID）
    pub partition_no: i32,
    /// 是否在界面上展示该平台各连接的收发报文（打开后连接可点击弹窗查看）
    pub show_packets: bool,
    /// 是否把收发报文持久化到文件（内容与界面展示一致，弹窗可回看全部历史）
    pub persist_packets: bool,
    /// 是否缓存收到的所有订单（默认开启）：界面可查看订单列表，
    /// 且撤单按订单真实状态回复成功/失败（在途→成功，终态→失败）
    pub cache_orders: bool,
    /// 模拟回报策略
    pub strategy: StrategyConfig,
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "现货集中竞价交易平台".into(),
            platform_type: 1,
            listen_host: "0.0.0.0".into(),
            port: 7001,
            comp_id: "SIMX_TGW".into(),
            check_password: false,
            password: String::new(),
            partition_no: 1,
            show_packets: true,
            persist_packets: true,
            cache_orders: true,
            strategy: StrategyConfig::default(),
        }
    }
}

/// 平台类型名称
pub fn platform_type_name(t: u16) -> &'static str {
    match t {
        1 => "现货集中竞价交易平台",
        2 => "综合金融服务平台",
        3 => "非交易处理平台",
        4 => "衍生品集中竞价交易平台",
        5 => "国际市场互联平台",
        6 => "固定收益交易平台",
        _ => "未知平台",
    }
}

/// 模拟回报策略模式（本软件的核心：收到委托后按哪种套路回报）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StrategyMode {
    /// 全部成交（单笔）：1 条确认 + 1 条全额成交
    FullSingle,
    /// 全部成交（多笔拆单）：1 条确认 + N 条成交，数量随机拆分，价格按档位递增/递减
    FullSplit,
    /// 部分成交（单笔）：1 条确认 + 1 条部分成交，剩余挂单
    PartialSingle,
    /// 部分成交（多笔拆单）
    PartialSplit,
    /// 自定义成交：按 custom_fills 逐条回报指定数量/价格
    Custom,
    /// 不成交挂单：只回确认，永不成交
    AckOnly,
    /// 拒单：回拒绝回报（方式见 RejectVia）
    Reject,
}

/// 拒单回报方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RejectVia {
    /// 执行报告 200102，ExecType=8
    ExecutionReport,
    /// 业务拒绝消息 MsgType=4
    BusinessReject,
}

/// 延迟配置（毫秒）。三种情况：
/// - min==max==0：同步回报（在处理委托的流程内立即发送）
/// - min==max>0：固定延迟
/// - min<max：每次在 [min, max] 区间内随机取一个延迟
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DelayConfig {
    pub min_ms: u64,
    pub max_ms: u64,
}

impl DelayConfig {
    /// 是否零延迟（决定同步还是异步发送回报）
    pub fn is_zero(&self) -> bool {
        self.min_ms == 0 && self.max_ms == 0
    }

    /// 取样一个延迟值（毫秒）；自动容忍 min/max 写反的情况
    pub fn sample(&self) -> u64 {
        use rand::Rng;
        let (lo, hi) = if self.min_ms <= self.max_ms {
            (self.min_ms, self.max_ms)
        } else {
            (self.max_ms, self.min_ms)
        };
        if lo == hi {
            lo
        } else {
            rand::thread_rng().gen_range(lo..=hi)
        }
    }
}

/// 自定义成交明细（Custom 模式下每一笔成交的数量与价格，
/// 这里用“股/元”自然单位，发送时才换算成协议的放大整数）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CustomFill {
    /// 成交数量（股）
    pub qty: f64,
    /// 成交价格（元）
    pub price: f64,
}

impl Default for CustomFill {
    fn default() -> Self {
        Self { qty: 100.0, price: 10.0 }
    }
}

/// 模拟回报策略配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StrategyConfig {
    pub mode: StrategyMode,
    /// 拆单笔数下限（多笔拆单模式）
    pub split_count_min: u32,
    /// 拆单笔数上限（多笔拆单模式）
    pub split_count_max: u32,
    /// 价格档位（元），拆单时按档位递增/递减
    pub price_tick: f64,
    /// 自定义成交明细（Custom 模式）
    pub custom_fills: Vec<CustomFill>,
    /// 拒单回报方式
    pub reject_via: RejectVia,
    /// 拒单原因代码
    pub reject_reason: u16,
    /// 拒单原因说明（仅业务拒绝消息携带文本）
    pub reject_text: String,
    /// 确认回报延迟
    pub ack_delay: DelayConfig,
    /// 成交回报延迟（多笔成交时为相邻两笔间隔）
    pub trade_delay: DelayConfig,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            mode: StrategyMode::FullSingle,
            split_count_min: 2,
            split_count_max: 5,
            price_tick: 0.01,
            custom_fills: vec![CustomFill::default()],
            reject_via: RejectVia::ExecutionReport,
            reject_reason: 1,
            reject_text: "模拟拒单".into(),
            ack_delay: DelayConfig::default(),
            trade_delay: DelayConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧版 simx.config.json（只有 backend/remoteUrl）必须仍能解析，
    /// backends 缺省时按空列表处理（前端会按需合成）
    #[test]
    fn app_config_parses_legacy_format() {
        let old = r#"{"backend": "remote", "remoteUrl": "ws://192.168.1.10:9800/ws"}"#;
        let cfg: AppConfig = serde_json::from_str(old).expect("旧格式必须可解析");
        assert_eq!(cfg.backend, "remote");
        assert_eq!(cfg.remote_url, "ws://192.168.1.10:9800/ws");
        assert!(cfg.backends.is_empty());
    }

    /// 新格式（含多后端列表）序列化往返必须完整保留
    #[test]
    fn app_config_roundtrip_with_backends() {
        let cfg = AppConfig {
            backend: "b-srv1".into(),
            remote_url: "ws://192.168.1.10:9800/ws".into(),
            backends: vec![
                BackendEntry {
                    id: "local".into(),
                    name: "本地引擎".into(),
                    kind: "local".into(),
                    url: String::new(),
                },
                BackendEntry {
                    id: "b-srv1".into(),
                    name: "测试机房".into(),
                    kind: "remote".into(),
                    url: "ws://192.168.1.10:9800/ws".into(),
                },
            ],
            update_url: "http://192.168.1.10/update".into(),
        };
        let json = serde_json::to_string(&cfg).expect("序列化");
        let back: AppConfig = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.backend, "b-srv1");
        assert_eq!(back.backends.len(), 2);
        assert_eq!(back.backends[1].name, "测试机房");
        assert_eq!(back.backends[1].url, "ws://192.168.1.10:9800/ws");
        assert_eq!(back.update_url, "http://192.168.1.10/update");
    }
}
