// 桌面端"绿色版目录自更新"：下载 / 应用（重启 + 覆盖）。
//
// 与旧版"下载安装包→启动安装向导"不同，新流程把服务器上整个发布目录
// （simuxchange.exe + 其它文件/子目录）按清单下载到本地临时目录，程序
// 退出时由"更新器"就地覆盖程序目录，实现真正的绿色版原地更新。
// （旧安装包模式已彻底删除，不再有"下载安装包"分支。）
//
// # 设计要点（与 crates/simx-update-server 约定）
//
// 服务器 version.json（目录模式）：
//   { "version": "1.3.0",
//     "files": [ { "name": "simuxchange.exe", "size": 123, "sha256": ".." }, ... ] }
// name 是相对发布根的路径（含子目录，"/" 分隔），场景：
//
// 1. 下载（download_update_files）：全部文件下载到
//    %LOCALAPPDATA%\SimuXchange\updates\<版本号>\，逐个校验 size/sha256。
//    【下载顺序让主 exe（simuxchange.exe）恒排最后】——运行中的 exe 无法被
//    覆盖，其它文件先就位、exe 最后落地，中途失败也不会出现"exe 是新的、
//    配套文件是旧的"的不一致状态。完成后写 pending.json 待处理标记。
// 2. 应用（--apply-update）：Windows 锁住运行中的 exe，覆盖必须等主进程
//    退出后由"第三方"执行——这个第三方就是下载目录里的新 exe 本身
//    （带 --apply-update 参数启动，不进入主程序逻辑）。它等主进程（父 PID）
//    退出 → 把下载目录文件逐一覆盖到 exe 所在目录（同样主 exe 最后）→
//    按需重启 → 删除标记，然后自身退出。
//
//    触发时机：点右上角红色"重启以更新"（restart=1，覆盖后自动重启）或
//    关闭窗口（restart=0，下次启动生效）；两者都由 main.rs 的窗口关闭
//    事件/命令统一 spawn 更新器，本模块只提供函数。
//
// # 安全边界
//
// 更新只覆盖清单里列出的文件：用户数据（gateways.json、packets/、log/ 等）
// 不在清单、不在发布目录，就永远不会被碰。清单 = 服务器扫描发布目录生成，
// 所以发布目录里放什么、客户端就覆盖什么。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// 服务器清单里的单个文件条目（与 simx-update-server 的 JSON 手工对应）
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFile {
    /// 相对发布根的路径（可含子目录，"/" 分隔）
    pub name: String,
    /// 文件大小（服务器可能没算，为空跳过大小校验）
    #[serde(default)]
    pub size: Option<u64>,
    /// sha256 十六进制（为空跳过哈希校验）
    #[serde(default)]
    pub sha256: Option<String>,
}

/// 服务器 version.json 清单（格式见文件头；Serialize 供命令把清单返回给前端展示）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteManifest {
    pub version: String,
    pub files: Vec<RemoteFile>,
}

/// 待完成更新（写盘 pending.json，重启后可恢复红色"重启更新"提示）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingUpdate {
    pub version: String,
    /// 当前 exe 所在目录（覆盖目标；用户数据目录不在此列，见文件头）
    pub install_dir: String,
    /// 下载目录（新版本文件的本地副本）
    pub update_dir: String,
    /// 待覆盖的文件清单（只碰清单内的文件）
    pub files: Vec<RemoteFile>,
}

/// 待应用状态（共享给 Tauri command 与窗口关闭事件）
#[derive(Default)]
pub struct UpdateState {
    /// 已下载完成、尚未应用的更新
    pub pending: Option<PendingUpdate>,
    /// 更新器已启动（防止窗口关闭事件重复 spawn 造成两份更新器对打）
    pub dispatched: bool,
}

// ---------------------------------------------------------------------------
// 基础工具
// ---------------------------------------------------------------------------

/// 版本号比较：返回 a 相对 b 的新旧（1 新 / -1 旧 / 0 相同）。
/// 数字点分段逐段比较（1.10.0 > 1.9.0），与服务器端 version_cmp 同规则。
pub fn version_cmp(a: &str, b: &str) -> i32 {
    let (mut x, mut y) = (a.split('.'), b.split('.'));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return 0,
            (None, _) => return -1,
            (_, None) => return 1,
            (Some(pa), Some(pb)) => {
                let (na, nb) = (pa.parse::<u64>().unwrap_or(0), pb.parse::<u64>().unwrap_or(0));
                if na != nb {
                    return if na > nb { 1 } else { -1 };
                }
            }
        }
    }
}

/// 当前 exe 文件名（列simuxchange.exe），也是清单里"主文件"的判定依据
pub fn main_exe_name() -> Option<String> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
}

/// 更新文件区根目录：%LOCALAPPDATA%\SimuXchange\updates。
/// 放 AppData 而不是系统临时目录——临时目录会被系统定期清理，
/// 用户下载完隔天再点"重启更新"会找不到文件。
pub fn updates_root() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("SimuXchange").join("updates")
}

/// 待更新标记文件路径（在版本目录外，清理版本目录不影响标记）
pub fn pending_path() -> PathBuf {
    updates_root().join("pending.json")
}

/// 从磁盘读回待更新标记（启动时恢复界面"重启更新"提示）
pub fn load_pending() -> Option<PendingUpdate> {
    std::fs::read_to_string(pending_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// 写待更新标记（下载完成时落盘）
pub fn save_pending(p: &PendingUpdate) -> Result<(), String> {
    let path = pending_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text =
        serde_json::to_string_pretty(p).map_err(|e| format!("序列化待更新清单失败: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入 {} 失败: {}", path.display(), e))
}

/// 删除待更新标记（更新器应用完成后调用）
pub fn remove_pending() {
    let _ = std::fs::remove_file(pending_path());
}

/// 调整文件顺序：主 exe（main_name）恒排最后，其余保持原顺序。
/// 下载与覆盖共用，保证"先其它文件、后主 exe"，见文件头设计要点。
pub fn order_files(files: &mut [RemoteFile], main_name: &str) {
    files.sort_by_key(|f| if f.name.eq_ignore_ascii_case(main_name) { 1 } else { 0 });
}

/// URL 路径段百分号编码（文件名字眼光标兜底，只放行 ASCII 字母数字与 /_-~.）
fn url_encode_path(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 拼接下载 URL：根地址 + 编码后的相对路径
fn join_url(base: &str, rel: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), url_encode_path(rel))
}

fn file_sha256_hex(path: &Path) -> String {
    let data = std::fs::read(path).unwrap_or_default();
    format!("{:x}", Sha256::digest(&data))
}

// ---------------------------------------------------------------------------
// 检查 / 下载（Tauri command 后端的实际逻辑）
// ---------------------------------------------------------------------------

/// 检查更新：拉 version.json 并与当前版本比较。
/// 检查放在 Rust（ureq）而非 Web 前端 fetch，是为了避开 WebView
/// 跨域（CORS）限制（浏览器模式的前端路径保留在 updater.ts）。
pub fn check_update_remote(
    url: &str,
    current_version: &str,
) -> Result<Option<RemoteManifest>, String> {
    let resp = ureq::get(&join_url(url, "version.json"))
        .timeout(std::time::Duration::from_secs(10))
        .call()
        .map_err(|e| format!("连接更新服务器失败: {e}"))?;
    let text = resp
        .into_string()
        .map_err(|e| format!("读取更新清单失败: {e}"))?;
    let manifest: RemoteManifest =
        serde_json::from_str(&text).map_err(|e| format!("更新清单格式错误: {e}"))?;
    if manifest.version.is_empty() {
        return Err("更新清单缺少版本号".to_string());
    }
    // 不比当前大就不算有新版本
    if version_cmp(&manifest.version, current_version) <= 0 {
        return Ok(None);
    }
    Ok(Some(manifest))
}

/// 下载清单中的全部文件到版本目录，返回待处理更新（含落地目录）。
/// 主 exe 恒排最后；任何一步失败返回 Err 并带着上下文，由调用方展示。
pub fn download_update_files(
    url: &str,
    manifest: &RemoteManifest,
) -> Result<PendingUpdate, String> {
    let mut files = manifest.files.clone();
    if files.is_empty() {
        return Err("服务器未提供文件清单".to_string());
    }
    order_files(&mut files, &main_exe_name().unwrap_or_default());

    // 覆盖目标 = 当前 exe 所在目录（用户目录不在此，安全）
    let install_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .ok_or_else(|| "定位程序目录失败".to_string())?;
    let update_dir = updates_root().join(&manifest.version);

    // 同一版本目录存在（上次半途失败）先清空重下，保证落地目录干净
    let _ = std::fs::remove_dir_all(&update_dir);
    std::fs::create_dir_all(&update_dir)
        .map_err(|e| format!("创建下载目录失败: {e}"))?;

    for (i, f) in files.iter().enumerate() {
        let dest = update_dir.join(&f.name);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let resp = ureq::get(&join_url(url, &f.name))
            .timeout(std::time::Duration::from_secs(120))
            .call()
            .map_err(|e| {
                format!("下载 {} 失败（第 {}/{} 项）: {}", f.name, i + 1, files.len(), e)
            })?;
        let mut reader = resp.into_reader();
        let mut file = std::fs::File::create(&dest)
            .map_err(|e| format!("创建 {} 失败: {}", dest.display(), e))?;
        std::io::copy(&mut reader, &mut file)
            .map_err(|e| format!("写入 {} 失败: {}", dest.display(), e))?;

        // 完整性校验：有 size/sha256 就校（防止下载中断/篡改）
        if let Some(want) = f.size {
            let got = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            if got != want {
                return Err(format!("{} 大小不符（预期 {}，实际 {}）", f.name, want, got));
            }
        }
        if let Some(want) = f.sha256.as_deref().filter(|s| !s.is_empty()) {
            if !file_sha256_hex(&dest).eq_ignore_ascii_case(want) {
                return Err(format!("{} 校验和不符（sha256）", f.name));
            }
        }
    }

    let pending = PendingUpdate {
        version: manifest.version.clone(),
        install_dir: install_dir.to_string_lossy().to_string(),
        update_dir: update_dir.to_string_lossy().to_string(),
        files,
    };
    save_pending(&pending)
        .map_err(|e| format!("保存待更新清单失败: {e}"))?;
    Ok(pending)
}

/// 启动更新器（新 exe 带 --apply-update 参数），随后由调用方退出主进程。
pub fn spawn_applier(pending: &PendingUpdate, restart: bool) -> Result<(), String> {
    let main = main_exe_name().ok_or_else(|| "找不到程序名".to_string())?;
    let applier = PathBuf::from(&pending.update_dir).join(&main);
    if !applier.is_file() {
        return Err(format!("下载目录缺少主程序 {}", applier.display()));
    }
    let pid = std::process::id().to_string();
    let pend_file = pending_path();
    if !pend_file.is_file() {
        return Err("待更新标记不存在".to_string());
    }
    std::process::Command::new(&applier)
        .args([
            "--apply-update",
            &pid,
            if restart { "1" } else { "0" },
            &pend_file.to_string_lossy(),
        ])
        .spawn()
        .map_err(|e| format!("启动更新器失败: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 更新器（后面 --apply-update 命令行入口）
// ---------------------------------------------------------------------------

/// --apply-update 模式的主流程：等主进程退出 → 覆盖文件 → 可选重启 → 清理。
/// 由 main() 在初始化 Tauri 之前调用，完成即退出进程（无窗口）。
pub fn run_apply_update(args: &[String]) -> i32 {
    if args.len() < 3 {
        return 1;
    }
    let parent_pid: u32 = args[0].parse().unwrap_or(0);
    let restart = args[1] == "1";
    let pending_file = PathBuf::from(&args[2]);

    let log_path = updates_root().join("apply-update.log");
    let mut log: Vec<String> = Vec::new();
    let log_line = |log: &mut Vec<String>, msg: &str| {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        log.push(format!("[{ts}] {msg}"));
    };
    let finish = |code: i32, log: &mut Vec<String>, log_path: &Path| {
        if let Some(dir) = log_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(log_path, log.join("\n"));
        code
    };

    log_line(
        &mut log,
        &format!("更新器启动 pid={parent_pid} restart={restart} {}", pending_file.display()),
    );

    // 1. 等主进程退出：它的 exe 被 Windows 锁定，没退出无法覆盖
    wait_parent_exit(parent_pid);

    // 2. 读待更新清单
    let Ok(text) = std::fs::read_to_string(&pending_file) else {
        log_line(&mut log, "读待更新清单失败");
        return finish(1, &mut log, &log_path);
    };
    let Ok(pending) = serde_json::from_str::<PendingUpdate>(&text) else {
        log_line(&mut log, "待更新清单解析失败");
        return finish(2, &mut log, &log_path);
    };

    // 3. 覆盖：先非主程序，最后主 exe（与下载顺序一致）
    let mut files = pending.files.clone();
    order_files(&mut files, &main_exe_name().unwrap_or_default());
    let mut ok = 0u32;
    for f in files.iter() {
        let src = PathBuf::from(&pending.update_dir).join(&f.name);
        let dst = PathBuf::from(&pending.install_dir).join(&f.name);
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if copy_file_force(&src, &dst) {
            ok += 1;
        } else {
            log_line(&mut log, &format!("覆盖失败: {}", dst.display()));
        }
    }
    log_line(&mut log, &format!("覆盖完成 {ok}/{} 个文件", files.len()));

    // 4. 需要时重启新版本（不等待其启动结果，它自己会正常起窗口）
    if restart {
        let new_exe = PathBuf::from(&pending.install_dir).join(main_exe_name().unwrap_or_default());
        match std::process::Command::new(&new_exe).spawn() {
            Ok(_) => log_line(&mut log, "已启动新版本"),
            Err(e) => log_line(&mut log, &format!("启动新版本失败: {e}")),
        }
    }

    // 5. 清理：删除待更新标记；清理版本目录（自身 exe 运行中删除会失败，
    //    保留残留（AppData 下的一个文件）不影响下一次更新）
    remove_pending();
    for f in files.iter() {
        let _ = std::fs::remove_file(PathBuf::from(&pending.update_dir).join(&f.name));
    }
    0
}

/// 阻塞等待父进程退出。
/// Windows：OpenProcess + WaitForSingleObject 挂句柄（进程退出即返回）；
/// 非 Windows 平台只是睡片刻（测试环境行为近似，实际只在 Windows 打包）。
fn wait_parent_exit(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        // OpenProcess 在 win32 平台，所以 Unsafe 块内调系统 API
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return; // 主进程已不存在（退出比更新器启动还快）
        }
        unsafe {
            WaitForSingleObject(handle, 60_000);
            CloseHandle(handle);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

/// 复制文件到目标（先删目标再试一次：目标可能是只读/残留属性）
fn copy_file_force(src: &Path, dst: &Path) -> bool {
    if std::fs::copy(src, dst).is_ok() {
        return true;
    }
    let _ = std::fs::remove_file(dst);
    std::fs::copy(src, dst).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn version_compare_rules() {
        assert!(version_cmp("1.10.0", "1.9.0") > 0);
        assert_eq!(version_cmp("1.2.0", "1.2.0"), 0);
        assert!(version_cmp("1.2.0", "1.2.1") < 0);
        assert!(version_cmp("2.0.0", "1.99.99") > 0);
    }

    #[test]
    fn order_main_exe_last() {
        let mut files = vec![
            RemoteFile { name: "simuxchange.exe".into(), size: None, sha256: None },
            RemoteFile { name: "resources/data.bin".into(), size: None, sha256: None },
            RemoteFile { name: "config.json".into(), size: None, sha256: None },
        ];
        order_files(&mut files, "simuxchange.exe");
        assert_eq!(files[0].name, "resources/data.bin");
        assert_eq!(files[1].name, "config.json");
        assert_eq!(files[2].name, "simuxchange.exe");
        let mut f2 = vec![RemoteFile { name: "SIMUXCHANGE.EXE".into(), size: None, sha256: None }];
        order_files(&mut f2, "simuxchange.exe");
        assert_eq!(f2[0].name, "SIMUXCHANGE.EXE");
    }

    #[test]
    fn url_encoding_keeps_subdirs() {
        assert_eq!(url_encode_path("resources/data.bin"), "resources/data.bin");
        assert_eq!(
            url_encode_path("a b/中文.dll"),
            "a%20b/%E4%B8%AD%E6%96%87.dll"
        );
    }

    /// 模拟更新器覆盖：只碰清单内的文件，目录里的"用户文件"原样保留
    #[test]
    fn applier_copy_preserves_foreign_files() {
        let root = std::env::temp_dir().join(format!("upd-test-{}", std::process::id()));
        let src = root.join("src");
        let dst = root.join("dst");
        fs::create_dir_all(src.join("resources")).unwrap();
        fs::create_dir_all(dst.join("resources")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("simuxchange.exe"), "new-exe").unwrap();
        fs::write(src.join("resources").join("a.bin"), "new-a").unwrap();
        fs::write(dst.join("simuxchange.exe"), "old-exe").unwrap();
        fs::write(dst.join("resources").join("a.bin"), "old-a").unwrap();
        fs::write(dst.join("user-data.db"), "keep-me").unwrap();

        let files = vec![
            RemoteFile { name: "resources/a.bin".into(), size: None, sha256: None },
            RemoteFile { name: "simuxchange.exe".into(), size: None, sha256: None },
        ];
        let mut ordered = files.clone();
        order_files(&mut ordered, "simuxchange.exe");
        for f in ordered.iter() {
            let dst_path = dst.join(&f.name);
            if let Some(parent) = dst_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            assert!(copy_file_force(&src.join(&f.name), &dst_path));
        }

        assert_eq!(fs::read_to_string(dst.join("simuxchange.exe")).unwrap(), "new-exe");
        assert_eq!(fs::read_to_string(dst.join("resources").join("a.bin")).unwrap(), "new-a");
        assert_eq!(fs::read_to_string(dst.join("user-data.db")).unwrap(), "keep-me");

        let _ = fs::remove_dir_all(&root);
    }
}