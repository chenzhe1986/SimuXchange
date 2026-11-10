//! 上海综合网关的 SQL Server 访问层（EzSTEP 数据库接口）。
//!
//! # 为什么用 tiberius
//!
//! 参考柜台用 ODBC（`SQLDriverConnect`）同步访问数据库，如果在 tokio 的
//! 异步任务里直接调用同步 ODBC，会把整个运行时线程卡住，影响其他 TCP 网关。
//! tiberius 是纯 Rust 的异步 TDS 实现：查询/写入都是 `await` 网络 I/O，
//! 不占用工作线程；数据库慢只会拖慢本平台自己的轮询任务。
//!
//! 选 0.12.x 而不是 0.13：0.13 的 rustls 链路会引入 aws-lc-rs（需要
//! CMake/NASM），与本项目 Linux musl 交叉打包冲突。0.12 用 rustls + ring，
//! Windows/Linux 都能直接编译。
//!
//! # 连接与重连
//!
//! [`DbHandle`] 内部用 `tokio::sync::Mutex` 串行化一条连接上的所有 SQL
//! （轮询、确认回写、成交插入、手动回复），避免并发写坏表。任何一次
//! 查询失败都会丢弃连接并置 `connected=false`，下一次操作自动重连。
//! 这里用异步 Mutex 而不是 `std::sync::Mutex`：锁要跨 `await` 存活，
//! 用标准库互斥锁会阻塞运行时线程（也是编译器不允许的）。
//!
//! # 表结构（EzSTEP 通用数据库接口）
//!
//! 委托表（reqresp）：`reqnum, reqid, reff, securityid, pbu, recordtimestamp,
//! ordstatus, remark, localrespnum, reqtext, resptext`
//! 成交表（execreport）：`execnum, bcasttype, setid, seqnum, recordtimestamp,
//! execreporttext`
//!
//! 柜台写入新委托时 `ordstatus='R'`；网关处理完后把它改成 `'O'`（已处理）
//! 并写 `resptext/localrespnum`。成交不是更新委托行，而是向成交表插新行。

use crate::config::DbConfig;
use parking_lot::Mutex as PlMutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;
use tiberius::{AuthMethod, Client, Config};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::protocol;

/// tiberius 客户端类型别名（TCP + tokio 兼容层）
type DbClient = Client<Compat<TcpStream>>;

/// 从委托表读到的一条待处理委托
#[derive(Debug, Clone)]
pub struct DbReqRow {
    /// 委托表记录编号（回写 resptext 时按它定位）
    pub reqnum: i32,
    /// 业务类型（EEC/PFP/CBT…）
    pub reqid: String,
    /// 柜台内部编号（ClOrdID）
    pub reff: String,
    /// 业务 PBU
    pub pbu: String,
    /// 证券代码
    pub security_id: String,
    /// 委托写入时间
    pub record_timestamp: String,
    /// STEP 委托报文
    pub reqtext: String,
}

/// 平台级数据库句柄：一条共享连接 + 序号发生器 + 订单号到 reqnum 的映射。
///
/// 被 engine 的 RunningPlatform 与 shzh::session 轮询任务共同持有（Arc），
/// 因此手动回复和自动回报会通过同一把异步锁串行写库。
pub struct DbHandle {
    cfg: DbConfig,
    /// 当前连接；None = 未连接/上次操作失败，下次操作自动重连
    client: Mutex<Option<DbClient>>,
    /// 数据库是否处于已连接状态（快照给前端展示）
    connected: AtomicBool,
    /// 下一个响应序号（reqresp.localrespnum）
    local_resp_seq: AtomicI64,
    /// 下一个执行编号（execreport.execnum）
    exec_seq: AtomicI64,
    /// 订单号（ClOrdID）→ 委托表 reqnum（手动回复时定位原始行）
    order_reqnums: PlMutex<HashMap<String, i32>>,
}

impl DbHandle {
    pub fn new(cfg: DbConfig) -> Self {
        Self {
            cfg,
            client: Mutex::new(None),
            connected: AtomicBool::new(false),
            local_resp_seq: AtomicI64::new(0),
            exec_seq: AtomicI64::new(0),
            order_reqnums: PlMutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &DbConfig {
        &self.cfg
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// 记录订单号与委托表 reqnum 的对应关系（手动回复/撤单定位原行用）。
    /// 映射只是加速缓存：超过 1 万条就整体清空，缺失时 find_reqnum 会回退查库，
    /// 避免长时间运行后内存无限增长。
    pub fn remember_order(&self, cl_ord_id: &str, reqnum: i32) {
        let mut m = self.order_reqnums.lock();
        if m.len() > 10_000 {
            m.clear();
        }
        m.insert(cl_ord_id.to_string(), reqnum);
    }

    /// 查订单号上次记录到的 reqnum（用于识别“同一 ClOrdID 的重复行”）
    pub fn order_reqnum(&self, cl_ord_id: &str) -> Option<i32> {
        self.order_reqnums.lock().get(cl_ord_id).copied()
    }

    /// 初始化响应/执行序号：从表里已有最大值继续，避免重启后序号回退，
    /// 导致柜台按 localrespnum 增量读取时漏掉回报。
    pub async fn init_sequences(&self) -> Result<(), String> {
        let sql_rr = format!(
            "SELECT ISNULL(MAX(localrespnum), 0) AS n FROM {} WITH (NOLOCK)",
            quote_table(&self.cfg.req_resp_table)
        );
        let sql_er = format!(
            "SELECT ISNULL(MAX(execnum), 0) AS n FROM {} WITH (NOLOCK)",
            quote_table(&self.cfg.exec_report_table)
        );
        let rr = self.query(&sql_rr, &[]).await?;
        let local = rr
            .first()
            .and_then(|r| r.get::<i32, _>("n"))
            .unwrap_or(0);
        let er = self.query(&sql_er, &[]).await?;
        let exec = er
            .first()
            .and_then(|r| r.get::<i32, _>("n"))
            .unwrap_or(0);
        self.local_resp_seq.store(local as i64, Ordering::Relaxed);
        self.exec_seq.store(exec as i64, Ordering::Relaxed);
        Ok(())
    }

    /// 读取待处理委托（ordstatus='R'），按记录号升序，最多 limit 条。
    /// 用 `WITH (NOLOCK)` 避免和柜台写入互相阻塞（测试工具可接受脏读）。
    pub async fn load_pending(&self, limit: usize) -> Result<Vec<DbReqRow>, String> {
        let limit = limit.clamp(1, 1000);
        let sql = format!(
            "SELECT TOP ({}) reqnum, reqid, reff, pbu, securityid, recordtimestamp, reqtext \
             FROM {} WITH (NOLOCK) WHERE ordstatus = 'R' ORDER BY reqnum ASC",
            limit,
            quote_table(&self.cfg.req_resp_table)
        );
        let rows = self.query(&sql, &[]).await?;
        Ok(rows
            .into_iter()
            .map(|r| DbReqRow {
                reqnum: r.get::<i32, _>("reqnum").unwrap_or(0),
                reqid: str_col(&r, "reqid"),
                reff: str_col(&r, "reff"),
                pbu: str_col(&r, "pbu"),
                security_id: str_col(&r, "securityid"),
                record_timestamp: str_col(&r, "recordtimestamp"),
                reqtext: str_col(&r, "reqtext"),
            })
            .collect())
    }

    /// 回写委托表的响应：设置 ordstatus、resptext、localrespnum。
    /// 返回本次分配的 localrespnum（柜台按它增量读取确认回报）。
    pub async fn write_reqresp_response(
        &self,
        reqnum: i32,
        ordstatus: &str,
        resptext: &str,
    ) -> Result<i32, String> {
        let local_resp = self.next_local_resp();
        let sql = format!(
            "UPDATE {} SET ordstatus = @P1, resptext = @P2, localrespnum = @P3 \
             WHERE reqnum = @P4",
            quote_table(&self.cfg.req_resp_table)
        );
        let res = self
            .execute(&sql, &[&ordstatus, &resptext, &local_resp, &reqnum])
            .await?;
        if res == 0 {
            return Err(format!("回写委托失败：reqnum={} 不存在", reqnum));
        }
        Ok(local_resp)
    }

    /// 插入一条成交执行报告，返回本次分配的 execnum。
    pub async fn insert_exec_report(
        &self,
        bcast_type: &str,
        set_id: &str,
        seqnum: i32,
        text: &str,
    ) -> Result<i32, String> {
        let execnum = self.next_exec();
        let ts = protocol::now_timestamp();
        let sql = format!(
            "INSERT INTO {} (execnum, bcasttype, setid, seqnum, recordtimestamp, execreporttext) \
             VALUES (@P1, @P2, @P3, @P4, @P5, @P6)",
            quote_table(&self.cfg.exec_report_table)
        );
        self.execute(
            &sql,
            &[&execnum, &bcast_type, &set_id, &seqnum, &ts.as_str(), &text],
        )
        .await?;
        Ok(execnum)
    }

    /// 只更新 ordstatus（不分配 localrespnum）：
    /// - “关闭自动回报”模式把行标记为已处理，等界面手动回复时再写 resptext；
    /// - 解析失败/不支持的消息标记为 'E'，避免轮询任务反复读到同一行。
    pub async fn mark_status(&self, reqnum: i32, status: &str) -> Result<(), String> {
        let sql = format!(
            "UPDATE {} SET ordstatus = @P1 WHERE reqnum = @P2",
            quote_table(&self.cfg.req_resp_table)
        );
        let res = self.execute(&sql, &[&status, &reqnum]).await?;
        if res == 0 {
            return Err(format!("更新委托状态失败：reqnum={} 不存在", reqnum));
        }
        Ok(())
    }

    /// 按 ClOrdID 查委托表 reqnum（内存映射没有时兜底查库）。
    pub async fn find_reqnum(&self, cl_ord_id: &str) -> Result<Option<i32>, String> {
        if let Some(n) = self.order_reqnums.lock().get(cl_ord_id).copied() {
            return Ok(Some(n));
        }
        let sql = format!(
            "SELECT TOP (1) reqnum FROM {} WITH (NOLOCK) WHERE reff = @P1 ORDER BY reqnum DESC",
            quote_table(&self.cfg.req_resp_table)
        );
        let rows = self.query(&sql, &[&cl_ord_id]).await?;
        Ok(rows.first().and_then(|r| r.get::<i32, _>("reqnum")))
    }

    /// 关闭连接（平台停止时调用；失败也静默）
    pub async fn close(&self) {
        let mut guard = self.client.lock().await;
        *guard = None;
        self.connected.store(false, Ordering::Relaxed);
    }

    fn next_local_resp(&self) -> i32 {
        (self.local_resp_seq.fetch_add(1, Ordering::Relaxed) + 1) as i32
    }

    fn next_exec(&self) -> i32 {
        (self.exec_seq.fetch_add(1, Ordering::Relaxed) + 1) as i32
    }

    /// 执行 SELECT，返回第一批结果集。
    /// 加 15 秒超时：SQL Server 网络半断/锁等待时不能让轮询任务无限挂起
    /// （超时后丢弃连接，下次操作自动重连）。
    async fn query(
        &self,
        sql: &str,
        params: &[&dyn tiberius::ToSql],
    ) -> Result<Vec<tiberius::Row>, String> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect_locked().await?);
        }
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            let client = guard.as_mut().expect("刚连接成功");
            run_query(client, sql, params).await
        })
        .await;
        match result {
            Ok(Ok(rows)) => {
                self.connected.store(true, Ordering::Relaxed);
                Ok(rows)
            }
            Ok(Err(e)) => {
                // 连接已不可靠：丢弃，下次操作重新建立（轮询循环会持续重试）
                *guard = None;
                self.connected.store(false, Ordering::Relaxed);
                Err(e)
            }
            Err(_) => {
                *guard = None;
                self.connected.store(false, Ordering::Relaxed);
                Err("SQL 查询超时（15 秒）".into())
            }
        }
    }

    /// 执行 UPDATE/INSERT，返回受影响总行数（同样有 15 秒超时）
    async fn execute(
        &self,
        sql: &str,
        params: &[&dyn tiberius::ToSql],
    ) -> Result<u64, String> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect_locked().await?);
        }
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            let client = guard.as_mut().expect("刚连接成功");
            run_execute(client, sql, params).await
        })
        .await;
        match result {
            Ok(Ok(n)) => {
                self.connected.store(true, Ordering::Relaxed);
                Ok(n)
            }
            Ok(Err(e)) => {
                *guard = None;
                self.connected.store(false, Ordering::Relaxed);
                Err(e)
            }
            Err(_) => {
                *guard = None;
                self.connected.store(false, Ordering::Relaxed);
                Err("SQL 写入超时（15 秒）".into())
            }
        }
    }

    /// 建立新连接（调用方已持有 client 锁）
    async fn connect_locked(&self) -> Result<DbClient, String> {
        let cfg = &self.cfg;
        let mut config = Config::new();
        config.host(cfg.host.trim());
        config.port(cfg.port);
        config.database(cfg.database.trim());
        config.authentication(AuthMethod::sql_server(
            cfg.user.trim(),
            cfg.password.as_str(),
        ));
        config.application_name("SimuXchange");
        // 登录阶段加密、数据通道不强制加密：测试环境里不少 SQL Server 未配置
        // 服务器证书，强制全链路 TLS 会直接连不上；登录密码仍然受保护。
        config.encryption(tiberius::EncryptionLevel::Off);
        // 本工具只在测试环境使用，连接目标都是测试库（SQL Server 默认自签名
        // 证书），因此固定信任服务器证书、不做证书链校验，避免使用者还要
        // 理解/配置证书；生产环境请勿用本工具直连。
        config.trust_cert();
        let addr = format!("{}:{}", cfg.host.trim(), cfg.port);
        let tcp = tokio::time::timeout(Duration::from_secs(8), TcpStream::connect(&addr))
            .await
            .map_err(|_| format!("连接 SQL Server 超时：{}", addr))?
            .map_err(|e| format!("连接 SQL Server 失败 {}：{}", addr, e))?;
        let _ = tcp.set_nodelay(true);
        let client = tokio::time::timeout(
            Duration::from_secs(12),
            Client::connect(config, tcp.compat_write()),
        )
        .await
        .map_err(|_| format!("SQL Server 登录超时：{}", addr))?
        .map_err(|e| format!("SQL Server 登录失败 {}：{}", addr, e))?;
        self.connected.store(true, Ordering::Relaxed);
        Ok(client)
    }
}

/// SELECT：取第一个结果集
async fn run_query(
    client: &mut DbClient,
    sql: &str,
    params: &[&dyn tiberius::ToSql],
) -> Result<Vec<tiberius::Row>, String> {
    let stream = client
        .query(sql.to_string(), params)
        .await
        .map_err(|e| format!("执行查询失败：{}（SQL: {}）", e, sql))?;
    stream
        .into_first_result()
        .await
        .map_err(|e| format!("读取查询结果失败：{}", e))
}

/// UPDATE/INSERT：返回受影响行数合计
async fn run_execute(
    client: &mut DbClient,
    sql: &str,
    params: &[&dyn tiberius::ToSql],
) -> Result<u64, String> {
    let result = client
        .execute(sql.to_string(), params)
        .await
        .map_err(|e| format!("执行写入失败：{}（SQL: {}）", e, sql))?;
    Ok(result.total())
}

/// 取字符串列并去掉数据库 char 字段右侧补的空格
fn str_col(row: &tiberius::Row, name: &str) -> String {
    row.get::<&str, _>(name)
        .unwrap_or("")
        .trim_end()
        .to_string()
}

/// 把表名安全地拼进动态 SQL：按 . 拆成 schema/table 各段加方括号，
/// 段内的 ] 转义成 ]]。配置保存时已限制字符集，这里再做一层防护。
fn quote_table(name: &str) -> String {
    name.trim()
        .split('.')
        .map(|part| format!("[{}]", part.replace(']', "]]")))
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_table_handles_schema_and_brackets() {
        assert_eq!(quote_table("reqresp"), "[reqresp]");
        assert_eq!(quote_table("dbo.reqresp"), "[dbo].[reqresp]");
        assert_eq!(quote_table("dbo.re]q"), "[dbo].[re]]q]");
    }

    #[test]
    fn sequence_counters_start_from_initialized_value() {
        let h = DbHandle::new(DbConfig::default());
        h.local_resp_seq.store(10, Ordering::Relaxed);
        h.exec_seq.store(20, Ordering::Relaxed);
        assert_eq!(h.next_local_resp(), 11);
        assert_eq!(h.next_local_resp(), 12);
        assert_eq!(h.next_exec(), 21);
    }

    #[test]
    fn order_reqnum_cache_roundtrip() {
        let h = DbHandle::new(DbConfig::default());
        h.remember_order("ORDER001", 42);
        assert_eq!(h.order_reqnums.lock().get("ORDER001"), Some(&42));
    }
}
