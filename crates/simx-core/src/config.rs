//! 配置模型：网关 / 平台 / 模拟回报策略。
//!
//! # 层级关系
//!
//! ```text
//! AppConfig（应用级：本地引擎/远程后端、更新服务器、AI 生成脚本的模型接口）
//! GatewayConfig（交易网关，可建多个）
//!   └─ PlatformConfig（平台：TCP 网关为一个监听端口；上海综合网关为一套数据库连接）
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
    /// “AI 生成脚本”的大模型接口配置：桌面端读取后交给前端 llm.ts 调用。
    /// 旧配置文件没有该字段时按内置默认值加载（见 LlmConfig::default）
    pub llm: LlmConfig,
}

/// “AI 生成脚本”使用的大模型接口配置（OpenAI 兼容的 chat/completions 协议）。
///
/// # 为什么放在应用配置里
///
/// 该调用完全由前端发起（模型接口没有 CORS 头，桌面端经 Tauri 壳 llm_chat
/// 透传 HTTPS，模拟引擎不参与），配置原本写死在 src/llm.ts 顶部；
/// 放到 simx.config.json 后，换模型/换服务商只需改配置文件，不用重新打包。
/// 纯浏览器模式（无 Tauri 壳）读不到该文件，回退 llm.ts 里的同一份内置默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LlmConfig {
    /// 接口根地址（写到版本号为止，前端自动拼 /chat/completions）
    pub base_url: String,
    /// 服务商鉴权密钥（请求头 Authorization: Bearer <apiKey>）
    pub api_key: String,
    /// 模型 id
    pub model: String,
    /// 采样温度：脚本生成要求稳定，取低值
    pub temperature: f64,
    /// 单次输出的 token 上限（脚本很短，8K 足够）
    pub max_tokens: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        // 默认接入 AMD Radeon 模型服务；在 simx.config.json 里写 llm 字段
        // 即可覆盖（未写该字段时用这里的值）
        Self {
            base_url: "https://developer.amd.com.cn/radeon/v1".into(),
            api_key: "rc-b824e20605e5c9f4a9ce7d3155f93fd00590b0958be59c2f".into(),
            model: "MiMo-V2.6-Flash".into(),
            temperature: 0.2,
            max_tokens: 8192,
        }
    }
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
            llm: LlmConfig::default(),
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
/// serde 的 camelCase 重命名后，JSON 中分别是 "sz"、"shjj"、"shbond" 和 "shzh"；
/// 旧配置文件里没有这个字段时默认按深圳统一网关处理（向后兼容）。
///
/// 前三类是 TCP 直连网关；Shzh（上海综合网关）不走 TCP，而是通过
/// SQL Server 接口表与柜台间接通信（STEP 消息存在 reqresp/execreport 表里），
/// 因此它的平台配置用 db 字段而不是监听地址/端口。
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
    /// 上海综合网关（上交所综合业务平台 EzSTEP 数据库接口）
    Shzh,
}

/// 网关分类名称（日志与界面展示用）
pub fn gateway_category_name(c: GatewayCategory) -> &'static str {
    match c {
        GatewayCategory::Sz => "深圳统一网关",
        GatewayCategory::Shjj => "上海竞价网关",
        GatewayCategory::Shbond => "上海新债券网关",
        GatewayCategory::Shzh => "上海综合网关",
    }
}

/// 网关目录（界面侧栏里的文件夹）：把网关分组展示，与网关配置同存
/// gateways.json。目录最多嵌套两层——根级目录（parent_id 为空）内可以
/// 再建子目录（parent_id 指向某个根级目录 id），子目录内不能再建目录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DirConfig {
    /// 唯一标识（后端自动生成，前端新建时传空串）
    pub id: String,
    /// 目录名称
    pub name: String,
    /// 父目录 id：空 = 根级目录；非空 = 挂在某个根级目录下的子目录
    pub parent_id: String,
}

impl Default for DirConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "新建目录".into(),
            parent_id: String::new(),
        }
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
    /// 所属目录 id（空 = 根层级，不归入任何目录）。目录定义存在
    /// gateways.json 的 dirs 数组里，网关只保存归属 id；
    /// 旧配置文件没有该字段时按默认空串处理（向后兼容）
    pub dir_id: String,
    /// 创建时间（yyyy-MM-dd HH:mm:ss）。旧配置没有该字段时由引擎
    /// 启动回填当前时间并落盘一次，让时间固定下来
    pub created_at: String,
    /// 修改时间（yyyy-MM-dd HH:mm:ss）：配置内容（含标签/备注/目录归属）
    /// 每次修改时刷新；启停网关只记运行状态不视为修改
    pub updated_at: String,
    /// 标签：逗号分隔的短词组（如 "生产,联调"），界面侧栏按标签过滤展示
    pub tags: String,
    /// 备注：自由文本，说明该网关的用途/环境等
    pub notes: String,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "新建网关".into(),
            category: GatewayCategory::Sz,
            platforms: Vec::new(),
            was_running: false,
            dir_id: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            tags: String::new(),
            notes: String::new(),
        }
    }
}

/// 上海综合网关的 SQL Server 连接与接口表配置（EzSTEP 数据库接口）。
///
/// # 为什么单独一个结构体
///
/// 前三类网关的平台配置是“监听地址 + 端口”；上海综合网关没有 TCP，
/// 通信完全依赖两张接口表，字段性质完全不同，放成嵌套对象后：
/// - 旧配置（没有 db 字段）仍能正常加载（Option + default）；
/// - 界面上按网关分类决定显示 TCP 字段还是数据库字段；
/// - 表名可配置，方便柜台与网关共用同一个库但用不同表（不同操作员/测试环境）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DbConfig {
    /// SQL Server 主机名或 IP（命名实例请填写实例实际监听端口，并在主机栏只写主机）
    pub host: String,
    /// SQL Server 端口（默认 1433）
    pub port: u16,
    /// 数据库名
    pub database: String,
    /// 登录账号（SQL Server 身份验证）
    pub user: String,
    /// 登录密码
    pub password: String,
    /// 委托表（请求及响应接口表）名称，默认 reqresp
    pub req_resp_table: String,
    /// 成交表（执行报告表）名称，默认 execreport
    pub exec_report_table: String,
    /// 轮询间隔（毫秒）：每隔多久查一次委托表里 ordstatus='R' 的新委托
    pub poll_interval_ms: u64,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 1433,
            database: String::new(),
            user: "sa".into(),
            password: String::new(),
            req_resp_table: "reqresp".into(),
            exec_report_table: "execreport".into(),
            poll_interval_ms: 100,
        }
    }
}

impl DbConfig {
    /// 配置是否完整到可以尝试连接（保存配置与启动网关时各校验一次）。
    /// 表名只校验字符集，防止把 SQL 关键字/分隔符拼进动态 SQL：
    /// 允许字母、数字、下划线、点（schema.table）与 #（临时表）。
    pub fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() {
            return Err("数据库主机不能为空".into());
        }
        if self.port == 0 {
            return Err("数据库端口不能为 0".into());
        }
        if self.database.trim().is_empty() {
            return Err("数据库名不能为空".into());
        }
        for (label, name) in [
            ("委托表名", &self.req_resp_table),
            ("成交表名", &self.exec_report_table),
        ] {
            let n = name.trim();
            if n.is_empty() {
                return Err(format!("{}不能为空", label));
            }
            if !n
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '#'))
            {
                return Err(format!(
                    "{}「{}」只能包含字母、数字、下划线、点和 #",
                    label, n
                ));
            }
        }
        Ok(())
    }
}

/// 平台配置（TCP 网关：每个平台一个独立监听端口；上海综合网关：一套数据库连接与接口表）
/// 派生 PartialEq 供引擎比较“平台内容是否变化”（判断修改时间是否刷新，
/// 比较时需剔除两个时间字段，见 engine.rs save_gateway）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlatformConfig {
    /// 唯一标识（后端自动生成）
    pub id: String,
    /// 平台名称
    pub name: String,
    /// 平台号：1=现货集中竞价 2=综合金融服务 3=非交易处理
    /// 4=衍生品集中竞价 5=国际市场互联 6=固定收益
    pub platform_type: u16,
    /// 监听地址（0.0.0.0 表示接受任意网卡来的连接；127.0.0.1 仅限本机）。
    /// 上海综合网关不用该字段（走数据库）
    pub listen_host: String,
    /// 监听端口（上海综合网关不用该字段）
    pub port: u16,
    /// 上海综合网关的数据库配置（仅 category == Shzh 时使用；
    /// 旧配置/其他分类为 None）
    pub db: Option<DbConfig>,
    /// TGW 回复 Logon 时使用的 SenderCompID
    pub comp_id: String,
    /// 是否校验登录密码
    pub check_password: bool,
    /// 登录密码（check_password 为 true 时生效）
    pub password: String,
    /// 平台分区号（深：回报中的 PartitionNo；沪：分区号 SetID）。
    /// 单分区兼容字段：partition_nos 为空/无效时的回退值，也供旧配置加载
    pub partition_no: i32,
    /// 分区号列表（逗号分隔，如 "115,116,117,118"）：
    /// 回报按证券代码哈希分配到其中一个分区（同一证券恒落同一分区）；
    /// 为空时按单分区 partition_no 处理
    pub partition_nos: String,
    /// 是否在界面上展示该平台各连接的收发报文（打开后连接可点击弹窗查看）
    pub show_packets: bool,
    /// 是否把收发报文持久化到文件（内容与界面展示一致，弹窗可回看全部历史）
    pub persist_packets: bool,
    /// 是否缓存收到的所有订单（默认开启）：界面可查看订单列表，
    /// 且撤单按订单真实状态回复成功/失败（在途→成功，终态→失败）
    pub cache_orders: bool,
    /// 模拟回报策略
    pub strategy: StrategyConfig,
    /// 创建时间（yyyy-MM-dd HH:mm:ss）；旧配置缺省时引擎启动回填，
    /// 见 GatewayConfig.created_at 的说明
    pub created_at: String,
    /// 修改时间（yyyy-MM-dd HH:mm:ss）：平台配置内容变化（含策略热更新）时刷新
    pub updated_at: String,
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: "现货集中竞价交易平台".into(),
            platform_type: 1,
            listen_host: "0.0.0.0".into(),
            port: 7001,
            db: None,
            comp_id: "SIMX_TGW".into(),
            check_password: false,
            password: String::new(),
            partition_no: 1,
            partition_nos: String::new(),
            show_packets: true,
            persist_packets: true,
            cache_orders: true,
            strategy: StrategyConfig::default(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
}

impl PlatformConfig {
    /// 解析分区号列表（支持逗号/顿号/空格分隔；容错：空或全部无效时
    /// 回退到单分区 partition_no——旧配置没有 partition_nos 字段也能正常工作）。
    /// 返回去重后的有序列表。
    pub fn partitions(&self) -> Vec<i32> {
        let mut list: Vec<i32> = self
            .partition_nos
            .split(|c| c == ',' || c == '，' || c == '、' || c == ' ' || c == ';' || c == '；')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        list.sort_unstable();
        list.dedup();
        if list.is_empty() {
            vec![self.partition_no]
        } else {
            list
        }
    }

    /// 按证券代码哈希分配分区号：同一证券恒落同一分区（保证该证券的
    /// 确认/成交/撤单回报都在同一分区，柜台按分区校验序号才成立）。
    /// 哈希用简单的字节加权和（djb2 变体），确定性、无状态。
    pub fn partition_for(&self, security_id: &str) -> i32 {
        let parts = self.partitions();
        let h = security_id
            .bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
        parts[(h as usize) % parts.len()]
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

/// 报单自动回报模式（本软件的核心：收到委托后按哪种套路回报）
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
    /// 不自动回复（挂单手动回复）：收到委托只登记订单缓存，不自动回任何回报，
    /// 由用户在订单界面手动回复确认/成交/拒单/撤单
    NoAutoReply,
    /// 不成交挂单：只回确认，永不成交。
    /// 旧配置里的 custom（自定义成交，已废弃）按此模式加载，保证旧配置不失效
    #[serde(alias = "custom")]
    AckOnly,
    /// 拒单：回拒绝回报（方式见 RejectVia）
    Reject,
    /// Lua 脚本回报：收到委托后执行用户编写的 Lua 脚本（StrategyConfig.lua_script），
    /// 按脚本返回值生成确认/成交/拒单回报（详见 docs/Lua脚本回报编写说明.md）。
    /// 只接管新委托的回报；撤单仍按 CancelMode 配置走。
    /// 脚本在保存时由后端做语法校验，运行时沙箱化执行（无 io/os 等库）。
    LuaScript,
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

/// 撤单自动回报模式（收到柜台撤单请求后怎么回）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CancelMode {
    /// 默认：原单在途（已报/部分成交）→ 撤单成功；找不到原单或原单已是终态 → 撤单拒单
    #[default]
    Default,
    /// 全部撤单成功：无论原单状态（在途/终态/找不到）一律回撤单成功
    AlwaysSuccess,
    /// 全部撤单拒绝：无论原单状态一律回撤单拒单
    AlwaysReject,
}

/// 延迟配置（毫秒）。三种情况：
/// - min==max==0：同步回报（在处理委托的流程内立即发送）
/// - min==max>0：固定延迟
/// - min<max：每次在 [min, max] 区间内随机取一个延迟
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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

/// 模拟回报策略配置
/// 派生 PartialEq 供引擎比较“平台配置内容是否有变化”（判断修改时间是否刷新）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StrategyConfig {
    pub mode: StrategyMode,
    /// 拆单笔数下限（多笔拆单模式）
    pub split_count_min: u32,
    /// 拆单笔数上限（多笔拆单模式）
    pub split_count_max: u32,
    /// 价格档位（元），拆单时按档位递增/递减
    pub price_tick: f64,
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
    /// 撤单自动回报模式（默认按原单状态撤单成功/拒单；也可配置一律拒单）
    pub cancel_mode: CancelMode,
    /// 撤单拒单原因代码（回填到撤单拒单/业务拒绝消息）
    pub cancel_reject_reason: u16,
    /// 前台拒单：撤单拒单时改发业务拒绝消息（深市 MsgType=4 / 沪市 204）
    pub cancel_front_reject: bool,
    /// 撤单回报延迟（撤单成功与撤单拒单回报均适用）
    pub cancel_delay: DelayConfig,
    /// Lua 回报脚本源码（仅 mode == LuaScript 时使用；serde camelCase → luaScript）。
    /// 保存时后端校验语法（engine 的 update_strategy / save_gateway），
    /// 运行时由 lua_engine 模块沙箱化编译执行
    pub lua_script: String,
    /// ETF 清单文件目录（深圳非交易处理平台 / 上海综合平台用；serde → etfPcfDir）。
    /// - 深圳：收到 ETF 实时申赎委托时从该目录按证券代码查找最新的
    ///   `pcf_<代码>_<YYYYMMDD>.xml`，把成份股明细填入执行报告 201202；
    /// - 上海综合：收到 EEC/EER/IEC/IER 申赎委托时查找
    ///   `ssepcf_<代码>_<YYYYMMDD>.xml`，生成 1.51 规范的份券/资金替代回报；
    /// 留空 = 不启用。随策略配置保存/热更新（运行中改目录即时生效）
    pub etf_pcf_dir: String,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            mode: StrategyMode::FullSingle,
            split_count_min: 2,
            split_count_max: 5,
            price_tick: 0.01,
            reject_via: RejectVia::ExecutionReport,
            reject_reason: 1,
            reject_text: "模拟拒单".into(),
            ack_delay: DelayConfig::default(),
            trade_delay: DelayConfig::default(),
            cancel_mode: CancelMode::Default,
            cancel_reject_reason: 1,
            cancel_front_reject: false,
            cancel_delay: DelayConfig::default(),
            lua_script: String::new(),
            etf_pcf_dir: String::new(),
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
        // 没有 llm 字段时按内置默认（AMD Radeon 模型服务）加载
        assert_eq!(cfg.llm.model, "MiMo-V2.6-Flash");
        assert!(cfg.llm.base_url.ends_with("/v1"));
    }

    /// llm 字段：显式配置覆盖默认值；只写部分字段时其余字段仍按默认补齐
    /// （保证配置文件里只改模型名/地址也能正常工作）
    #[test]
    fn app_config_parses_llm_section() {
        let raw = r#"{
            "backend": "local",
            "llm": {
                "baseUrl": "https://example.com/v1",
                "apiKey": "sk-test",
                "model": "demo-model"
            }
        }"#;
        let cfg: AppConfig = serde_json::from_str(raw).expect("llm 配置可解析");
        assert_eq!(cfg.llm.base_url, "https://example.com/v1");
        assert_eq!(cfg.llm.api_key, "sk-test");
        assert_eq!(cfg.llm.model, "demo-model");
        // 未写的字段沿用默认值
        assert_eq!(cfg.llm.temperature, LlmConfig::default().temperature);
        assert_eq!(cfg.llm.max_tokens, LlmConfig::default().max_tokens);
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
            llm: LlmConfig {
                model: "custom-model".into(),
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&cfg).expect("序列化");
        let back: AppConfig = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.backend, "b-srv1");
        assert_eq!(back.backends.len(), 2);
        assert_eq!(back.backends[1].name, "测试机房");
        assert_eq!(back.backends[1].url, "ws://192.168.1.10:9800/ws");
        assert_eq!(back.update_url, "http://192.168.1.10/update");
        // llm 配置随应用配置一起往返
        assert_eq!(back.llm.model, "custom-model");
        assert_eq!(back.llm.base_url, LlmConfig::default().base_url);
    }

    /// 多分区号解析与按证券哈希分配：同一证券恒落同一分区；
    /// 空/无效配置回退单分区 partition_no
    #[test]
    fn partitions_parse_and_hash_assign() {
        let cfg = PlatformConfig {
            partition_nos: "101, 102，103、104;105".into(),
            partition_no: 1,
            ..Default::default()
        };
        // 支持逗号/中文逗号/顿号/空格/分号分隔，自动去重排序
        assert_eq!(cfg.partitions(), vec![101, 102, 103, 104, 105]);
        // 同一证券恒同分区；不同证券可能不同分区
        assert_eq!(cfg.partition_for("000001"), cfg.partition_for("000001"));
        let p1 = cfg.partition_for("000001");
        let p2 = cfg.partition_for("000002");
        assert!(cfg.partitions().contains(&p1) && cfg.partitions().contains(&p2));

        // 无 partition_nos：回退单分区 partition_no
        let single = PlatformConfig::default();
        assert_eq!(single.partitions(), vec![1]);
        assert_eq!(single.partition_for("000001"), 1);

        // 全无效的 partition_nos 也回退
        let bad = PlatformConfig {
            partition_nos: "abc,,".into(),
            partition_no: 7,
            ..Default::default()
        };
        assert_eq!(bad.partitions(), vec![7]);
    }

    /// 上海综合网关配置：category 序列化为 shzh，db 字段完整往返；
    /// 旧平台配置（没有 db）仍按 None 加载
    #[test]
    fn shzh_category_and_db_config_roundtrip() {
        // 旧版配置里的 trustCert 字段已废弃（连接固定信任服务器证书）：
        // 保存在旧 gateways.json 里的该字段按未知字段忽略，不影响加载
        let raw = r#"{
            "id": "p1",
            "name": "上海综合业务平台",
            "platformType": 2,
            "listenHost": "",
            "port": 0,
            "db": {
                "host": "10.1.1.5",
                "port": 1433,
                "database": "ezstep",
                "user": "sa",
                "password": "pw",
                "reqRespTable": "dbo.reqresp",
                "execReportTable": "dbo.execreport",
                "pollIntervalMs": 50,
                "trustCert": false
            },
            "strategy": {}
        }"#;
        let p: PlatformConfig = serde_json::from_str(raw).expect("综合平台配置可解析");
        let db = p.db.as_ref().expect("db 字段存在");
        assert_eq!(db.database, "ezstep");
        assert_eq!(db.req_resp_table, "dbo.reqresp");
        assert_eq!(db.poll_interval_ms, 50);
        assert!(db.validate().is_ok());

        let gw = GatewayConfig {
            category: GatewayCategory::Shzh,
            platforms: vec![p],
            ..Default::default()
        };
        let json = serde_json::to_string(&gw).expect("序列化");
        assert!(json.contains("\"category\":\"shzh\""));

        let old = r#"{"id":"p2","name":"老平台","port":10001,"strategy":{}}"#;
        let p2: PlatformConfig = serde_json::from_str(old).expect("旧配置可解析");
        assert!(p2.db.is_none());
    }

    /// 表名只允许安全字符，防止动态 SQL 拼接被注入
    #[test]
    fn db_config_rejects_unsafe_table_name() {
        let mut db = DbConfig::default();
        db.database = "ezstep".into();
        assert!(db.validate().is_ok());
        db.req_resp_table = "reqresp; DROP TABLE x".into();
        assert!(db.validate().is_err());
        db.req_resp_table = "dbo.reqresp".into();
        assert!(db.validate().is_ok());
    }
}
