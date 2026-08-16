//! 前端操作日志：把前端（本地 Tauri 桌面端或远程 WebSocket 客户端）的每个
//! 操作追加写入 `<data_dir>/log/<YYYYMMDD>/ops.log`，与报文文件 packets/
//! 一样按日期归档。
//!
//! # 设计要点
//!
//! - 记录内容：前端接入/断开 + 每个操作（新建网关、启停网关、手动回复等），
//!   每行带前端 IP（远程模式取 WebSocket 对端地址；本地桌面端固定 127.0.0.1）
//!   与操作描述，如 `2026-08-16 10:00:00.123  192.168.0.1  新建网关 测试网关`。
//! - 写入时机：api::dispatch 每次处理非轮询命令时记一行（轮询类命令——
//!   快照/报文/订单列表——每秒都在发，记了只会刷屏，不记）。
//! - 跨日切换：每天第一次写入时按当天日期换一个文件（写之前判断当前日期
//!   与上次写文件时是否相同）。
//! - 写失败静默忽略：操作日志是辅助记录，不能因为磁盘问题影响主流程。
//! - 与 packet 文件相同的“逐行 flush”：掉电不丢最近的操作记录。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use parking_lot::Mutex;

/// 操作日志写入器（引擎持有，api::dispatch 与 WebSocket 连接层共用）。
/// 内部用 Mutex 串行化写入：多前端并发操作时逐行追加，不会互相穿插。
pub struct OpLog {
    /// 数据目录（log/<YYYYMMDD>/ops.log 建在它下面）
    data_dir: PathBuf,
    /// 当前打开文件的日期（YYYYMMDD，跨日时重建 writer）
    date: Mutex<String>,
    /// 当前日期的文件写入器（尚未写过当天日志时为 None）
    file: Mutex<Option<BufWriter<File>>>,
}

impl OpLog {
    /// 创建写入器（不立即打开文件，等第一条日志时才建目录/文件）
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            date: Mutex::new(String::new()),
            file: Mutex::new(None),
        }
    }

    /// 记一条操作日志：`<时间>  <前端IP>  <操作描述>`。
    /// 跨日自动切换到新日期的文件；写失败只忽略，不打扰主流程。
    pub fn log(&self, ip: &str, msg: &str) {
        let today = chrono::Local::now().format("%Y%m%d").to_string();
        let mut cur = self.date.lock();
        if *cur != today {
            // 跨日（或首次）：按当天日期重建文件
            *cur = today.clone();
            let dir = self.data_dir.join("log").join(&today);
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("ops.log");
            *self.file.lock() = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
                .map(|f| BufWriter::new(f));
        }
        if let Some(w) = self.file.lock().as_mut() {
            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            let _ = writeln!(w, "{}  {}  {}", ts, ip, msg);
            let _ = w.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_log_writes_dated_file() {
        // 操作日志应写到 <data_dir>/log/<YYYYMMDD>/ops.log，行内含 IP 与描述
        let dir = std::env::temp_dir().join(format!("simx_oplog_{}", std::process::id()));
        let oplog = OpLog::new(dir.clone());
        oplog.log("192.168.0.1", "新建网关 测试网关（1 个平台）");
        oplog.log("192.168.0.1", "启动网关 测试网关");
        // 跨日换文件的逻辑用同一日期覆盖不到，这里验证当天文件内容
        let date = chrono::Local::now().format("%Y%m%d");
        let path = dir.join("log").join(date.to_string()).join("ops.log");
        let content = std::fs::read_to_string(&path).expect("日志文件应存在");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "两条操作应各占一行");
        assert!(lines[0].contains("192.168.0.1"), "行内应含前端 IP");
        assert!(lines[0].contains("新建网关 测试网关"), "行内应含操作描述");
        assert!(lines[1].contains("启动网关 测试网关"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
