// simx-update-server：SimuXchange 升级服务器（单文件程序，Windows/Linux 均可运行）
//
// 设计动机（为什么需要它）：
//   桌面端需要一个 HTTP 可见的目录（version.json + 发布文件）就能完成升级
//   检查与下载。本程序把这个目录自动化：
//     1. 会自动按版本号扫描发布目录生成 version.json（文件清单 + 每个文件的 size/sha256）；
//     2. 发布目录里的每个文件（含子目录）都提供下载；
//     3. 一个程序完成"目录 + 清单"两件事。
//
// 部署方式（唯一支持的方式）：
//   simx-update-server
//   ├─ release.txt          ← 内容只有一行版本号（如 "2.1.0"）
//   └─ 2.1.0/               ← 以版本号命名的发布目录（simuxchange.exe 与其它文件/子目录）
//   服务器按 release.txt 的版本号找到同名目录，每次 GET /update/version.json 都会
//   实时编出全量文件清单（含子目录、大小、sha256），客户端据此把整个目录下载下来；
//   换版本只需改 release.txt 并放好对应目录，无需重启。
//   客户端"更新服务器地址"填 http://<IP>:9860/update（默认端口 9860）。
//
// 设计要点：
//   - 目录内文件是唯一事实来源，无数据库；release.txt 在发布目录之外，
//     不会混进用户的程序目录。
//   - 客户端下载时"主 exe 最后"由客户端负责（清单顺序无要求）。

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, StatusCode},
    routing::{delete, get},
    Json, Router,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

/// 全局共享状态：程序目录（release.txt 与各版本发布目录都在这下面）
#[derive(Clone)]
struct AppState {
    exe_dir: Arc<PathBuf>,
}

/// 服务器上的版本文件：内容只有一行版本号（如 "2.1.0"）
const RELEASE_TXT: &str = "release.txt";

#[tokio::main]
async fn main() {
    // 命令行参数解析（--listen 监听地址），不引入 clap 保持依赖精简
    let mut listen = "0.0.0.0:9860".to_string();
    // 部署约定（均以 exe 所在目录为基准，从任意目录启动都能找到）：
    //   release.txt  版本号文件，与程序同级
    //   <版本号>/     发布文件目录，如 2.1.0/
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                if let Some(v) = args.next() {
                    listen = v;
                }
            }
            "--help" | "-h" => {
                println!("用法: simx-update-server [--listen IP:端口] [-V|--version]");
                println!("默认: --listen 0.0.0.0:9860");
                println!("发布：release.txt 与程序同目录（内容一行版本号）；发布文件放同名版本目录（如 2.1.0/）");
                return;
            }
            // 打印本程序版本号后退出（与发布目录里的“发布版本号”是两回事：
            // 这里是升级服务器自身版本，来自本 crate 的 Cargo.toml）
            "-V" | "--version" => {
                println!("simx-update-server {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => {}
        }
    }

    println!("  版本文件 : {}", exe_dir.join(RELEASE_TXT).display());
    match resolve_release(&exe_dir) {
        Some((version, dir)) => {
            println!("  发布目录 : {}（release.txt = {}）", dir.display(), version)
        }
        None => println!("  发布目录 : 未发布（release.txt 缺失、为空或版本号非法）"),
    }
    let state = AppState {
        exe_dir: Arc::new(exe_dir),
    };

    let app = Router::new()
        .route("/", get(index))                           // 管理页面
        .route("/update/version.json", get(version_json)) // 升级清单（客户端按 <更新地址>/version.json 拉取）
        .route("/update/*rest", get(download_file))       // 发布文件下载（含子目录）
        .route("/api/files", get(list_files))             // 管理页文件列表
        .route("/api/file/:name", delete(delete_file))    // 删除文件（管理页用）
        // 未匹配的路由记录日志（404 时便于排查客户端配置错误）
        .fallback(|uri: axum::http::Uri| async move {
            eprintln!("[simx-update-server] 未匹配路由: {}", uri.path());
            (StatusCode::NOT_FOUND, "not found")
        })
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .expect("端口被占用，请换一个端口（如 9861）");
    println!("升级服务器已启动 v{}，浏览器打开 http://{} 管理发布", env!("CARGO_PKG_VERSION"), listen);
    println!("客户端升级地址填: http://{}/update （或本机局域网 IP 同端口）", listen);
    axum::serve(listener, app).await.expect("服务器异常退出");
}

// ---------------------------------------------------------------------------
// 核心逻辑：版本读取 + 清单生成
// ---------------------------------------------------------------------------

/// 读取版本号文件 release.txt：内容只有一行版本号（如 "2.1.0"）。
/// 文件缺失或内容为空 → None（此时 version.json 返回 404，客户端静默跳过）。
fn read_release_version(path: &FsPath) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let v = s.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// 解析当前发布：返回 (版本号, 发布目录 = <程序目录>/<版本号>)。
/// 版本号会直接作为目录名，必须是单级名称（拒绝 "."、""、".." 与路径分隔符），
/// 防止 release.txt 被写成路径导致越权读取。
/// 每次请求都重新解析，换版本只需改 release.txt 并放好对应目录，无需重启。
fn resolve_release(exe_dir: &FsPath) -> Option<(String, PathBuf)> {
    let version = read_release_version(&exe_dir.join(RELEASE_TXT))?;
    if version == "." || version == ".." || version.contains('/') || version.contains('\\') {
        eprintln!("[simx-update-server] release.txt 版本号不能作为目录名: {version}");
        return None;
    }
    let dir = exe_dir.join(&version);
    Some((version, dir))
}

/// 递归收集目录下所有"发布文件"（相对路径 + 大小 + sha256）。
/// rel 是相对目录根的路径（空表示根），Windows 分隔符统一转成 '/'，
/// 客户端下载时用相同相对路径落盘，保证目录结构原样还原。
fn collect_release_files(dir: &FsPath, rel: &FsPath, out: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        // 隐藏文件（.git 等）跳过
        if name_str.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_release_files(&path, &rel.join(&name), out);
            continue;
        }
        // 相对路径统一用 "/" 分隔（HTTP 路径与客户端落盘都按这个习惯）
        let rel_name = rel.join(&name).to_string_lossy().replace('\\', "/");
        let meta = entry.metadata().ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let sha256 = file_sha256(&path).unwrap_or_default();
        out.push(json!({
            "name": rel_name,
            "size": size,
            "sha256": sha256,
        }));
    }
}

/// 计算文件 sha256 十六进制小写串；读失败（少见）返回空串，
/// 客户端遇到空哈希会跳过哈希校验（大小校验仍生效）。
fn file_sha256(path: &FsPath) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let digest = Sha256::digest(&data);
    Some(hex(&digest))
}

/// sha256 摘要 → 十六进制小写字符串
fn hex(digest: &[u8]) -> String {
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 生成升级清单（version.json 的内容）：
///   { "version": "2.1.0", "files": [ {name,size,sha256}, ... ] }
/// 版本号来自与程序同目录的 release.txt，发布文件扫描同名版本目录；
/// 版本文件缺失/非法或版本目录为空 → None（404）
fn build_manifest(exe_dir: &FsPath) -> Option<Value> {
    let (version, dir) = resolve_release(exe_dir)?;
    let mut files: Vec<Value> = Vec::new();
    collect_release_files(&dir, &PathBuf::new(), &mut files);
    if files.is_empty() {
        return None;
    }
    Some(json!({
        "version": version,
        "files": files,
    }))
}

// ---------------------------------------------------------------------------
// HTTP 接口
// ---------------------------------------------------------------------------

/// 管理页面（内嵌 HTML，纯静态 JS 调接口，不依赖任何前端构建）
async fn index() -> &'static str {
    INDEX_HTML
}

/// GET /update/version.json：按 release.txt 的版本号扫描对应目录生成升级清单
async fn version_json(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    build_manifest(&state.exe_dir)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// GET /update/*rest：下载当前版本目录里的发布文件（支持多级子目录）
/// path 相对版本目录逐段清洗（拒绝 .. 等穿越），并再以 canonicalize 校验
/// 落在版本目录内。
async fn download_file(
    State(state): State<AppState>,
    Path(rest): Path<String>,
) -> Result<(StatusCode, [(header::HeaderName, String); 2], Bytes), StatusCode> {
    let (_, dir) = resolve_release(&state.exe_dir).ok_or(StatusCode::NOT_FOUND)?;
    let rel = safe_rel_path(&rest).ok_or(StatusCode::BAD_REQUEST)?;
    let path = dir.join(&rel);
    // 防路径穿过的最后一层兜底：解析后的路径必须仍在版本目录内
    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
    let base = dir.canonicalize().unwrap_or_else(|_| dir.clone());
    if !canonical.starts_with(&base) || !canonical.is_file() {
        eprintln!("[simx-update-server] 下载未命中: /update/{}", rest);
        return Err(StatusCode::NOT_FOUND);
    }
    let file_name = rel
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    let data = tokio::fs::read(&path).await.map_err(|_| StatusCode::NOT_FOUND)?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", file_name),
            ),
        ],
        data.into(),
    ))
}

/// GET /api/files：当前版本目录内的文件列表（管理页面展示用）
async fn list_files(State(state): State<AppState>) -> Json<Vec<Value>> {
    let Some((_, dir)) = resolve_release(&state.exe_dir) else {
        return Json(Vec::new());
    };
    let mut files: Vec<Value> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let meta = e.metadata().ok()?;
            let modified = meta
                .modified()
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            Some(json!({
                "name": name,
                "size": meta.len(),
                "modified": modified,
            }))
        })
        .collect();
    files.sort_by(|a, b| b["modified"].as_u64().cmp(&a["modified"].as_u64()));
    Json(files)
}

/// DELETE /api/file/{name}：删除当前版本目录里的文件（误上传时整理用）
async fn delete_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let Some((_, dir)) = resolve_release(&state.exe_dir) else {
        return Err((
            StatusCode::NOT_FOUND,
            "尚未发布（release.txt 缺失、为空或版本号非法）".to_string(),
        ));
    };
    // 只取文件名字尾段，防止路径穿越
    let file_name = FsPath::new(&name)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or((StatusCode::BAD_REQUEST, "文件名非法".to_string()))?
        .to_string();
    let path = dir.join(&file_name);
    if !path.is_file() {
        return Err((StatusCode::NOT_FOUND, "文件不存在".to_string()));
    }
    tokio::fs::remove_file(&path)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("删除失败: {e}")))?;
    Ok(Json(json!({ "ok": true })))
}

/// 相对路径安全清洗：按 / 拆段，逐段验证（拒绝 .. 与空串），返回相对路径
fn safe_rel_path(raw: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for seg in raw.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return None, // 路径穿越：拒绝
            s => out.push(s),
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// 管理页面（内嵌 HTML；管理地址也就是客户端填的地址）
// ---------------------------------------------------------------------------

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>SimuXchange 升级服务器</title>
<style>
  * { box-sizing: border-box; }
  body { font-family: "Microsoft YaHei", "PingFang SC", sans-serif; background: #f5f6f8; margin: 0; padding: 24px; color: #333; }
  .wrap { max-width: 860px; margin: 0 auto; }
  h1 { font-size: 22px; margin: 0 0 4px; }
  .hint { color: #888; font-size: 13px; margin-bottom: 20px; }
  .card { background: #fff; border: 1px solid #e3e5e8; border-radius: 8px; padding: 16px 20px; margin-bottom: 16px; }
  .card h2 { font-size: 15px; margin: 0 0 12px; }
  .kv { display: flex; margin: 6px 0; font-size: 14px; }
  .kv .k { width: 110px; color: #666; flex-shrink: 0; }
  .kv .v { word-break: break-all; }
  .empty { color: #aaa; font-size: 13px; }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th, td { text-align: left; padding: 7px 8px; border-bottom: 1px solid #eef0f2; }
  th { color: #666; font-weight: normal; }
  .del { background: none; color: #d9534f; border: 1px solid #d9534f; padding: 3px 10px; font-size: 12px; border-radius: 4px; cursor: pointer; }
  pre { background: #f6f8fa; border: 1px solid #e3e5e8; border-radius: 6px; padding: 10px 12px; font-size: 12px; overflow-x: auto; }
</style>
</head>
<body>
<div class="wrap">
  <h1>SimuXchange 升级服务器</h1>
  <div class="hint">客户端升级地址填：<b id="updUrl">http://&lt;本机IP&gt;:9860/update</b>（即本页面地址加 /update）</div>

  <div class="card">
    <h2>当前升级清单</h2>
    <div id="manifest" class="empty">加载中…</div>
  </div>

  <div class="card">
    <h2>发布新版本</h2>
    <div class="hint" style="margin:0 0 10px">
      把整个程序目录拷进以版本号命名的目录（如 2.1.0/），release.txt 与程序同目录：
      <pre>release.txt  内容仅一行版本号，如：2.1.0&#10;2.1.0/      发布文件（simuxchange.exe 与其它文件/子目录）</pre>
      服务器自动扫描目录生成文件清单（含子目录与校验值），客户端整目录下载更新；
      版本号小于客户端当前版本时客户端才会更新（数字点分比较）。
    </div>
  </div>

  <div class="card">
    <h2>目录文件</h2>
    <table id="fileTable">
      <tr><th>文件名</th><th>大小</th><th>修改时间</th><th></th></tr>
    </table>
  </div>
</div>

<script>
const $ = (id) => document.getElementById(id);
// 管理页可能通过 localhost/局域网 IP、非默认端口访问，升级地址按当前页面地址推导
$("updUrl").textContent = location.origin + "/update";

function fmtSize(n) {
  if (n < 1024) return n + " B";
  if (n < 1048576) return (n / 1024).toFixed(1) + " KB";
  return (n / 1048576).toFixed(1) + " MB";
}
function fmtTime(s) {
  const d = new Date(s * 1000);
  return d.getFullYear() + "-" + String(d.getMonth() + 1).padStart(2, "0") + "-" + String(d.getDate()).padStart(2, "0") + " " + String(d.getHours()).padStart(2, "0") + ":" + String(d.getMinutes()).padStart(2, "0");
}

async function refresh() {
  const man = await fetch("/update/version.json").then(r => r.ok ? r.json() : null).catch(() => null);
  if (man) {
    $("manifest").innerHTML =
      "<div class='kv'><span class='k'>当前版本</span><span class='v'><b>" + man.version + "</b></span></div>" +
      "<div class='kv'><span class='k'>发布文件数</span><span class='v'>" + (man.files || []).length + "</span></div>";
  } else {
    $("manifest").innerHTML = "<div class='empty'>发布目录中还没有内容（缺 release 版本文件或文件为空），上传后自动生成清单</div>";
  }
  const files = await fetch("/api/files").then(r => r.json()).catch(() => []);
  const rows = files.length ? files.map(f =>
    "<tr><td>" + f.name + "</td><td>" + fmtSize(f.size) + "</td><td>" + fmtTime(f.modified) +
    "</td><td><button class='del' onclick=\"delFile('" + encodeURIComponent(f.name) + "')\">删除</button></td></tr>"
  ).join("") : "<tr><td colspan='4' class='empty'>（空）</td></tr>";
  $("fileTable").innerHTML = "<tr><th>文件名</th><th>大小</th><th>修改时间</th><th></th></tr>" + rows;
}

async function delFile(name) {
  if (!confirm("确认删除 " + decodeURIComponent(name) + " ？")) return;
  await fetch("/api/file/" + name, { method: "DELETE" });
  refresh();
}

refresh();
</script>
</body>
</html>"#;

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_version_from_txt() {
        let dir = std::env::temp_dir().join(format!("relver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // release.txt 内容只有一行版本号（允许首尾空白）
        std::fs::write(dir.join(RELEASE_TXT), " 1.10.0 \n").unwrap();
        assert_eq!(
            read_release_version(&dir.join(RELEASE_TXT)).as_deref(),
            Some("1.10.0")
        );

        // 文件缺失 → None
        std::fs::remove_file(dir.join(RELEASE_TXT)).unwrap();
        assert!(read_release_version(&dir.join(RELEASE_TXT)).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_manifest_scan_version_dir_with_hash() {
        // 部署布局：release.txt 与程序同级，发布文件在 <版本号>/ 下
        let dir = std::env::temp_dir().join(format!("rel-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let publish = dir.join("1.3.0");
        std::fs::create_dir_all(publish.join("resources")).unwrap();
        std::fs::write(dir.join(RELEASE_TXT), "1.3.0").unwrap();
        std::fs::write(publish.join("simuxchange.exe"), b"hello").unwrap();
        std::fs::write(publish.join("resources").join("data.bin"), b"world!").unwrap();
        // 旧版本目录存在也不应参与清单
        std::fs::create_dir_all(dir.join("1.2.9")).unwrap();
        std::fs::write(dir.join("1.2.9").join("old.bin"), b"old").unwrap();

        let manifest = build_manifest(&dir).unwrap();
        assert_eq!(manifest["version"], "1.3.0");
        let files = manifest["files"].as_array().unwrap();
        assert_eq!(files.len(), 2, "只应包含当前版本目录下的文件: {files:?}");
        let exe = files
            .iter()
            .find(|f| f["name"] == "simuxchange.exe")
            .unwrap();
        assert_eq!(exe["size"], 5);
        // "hello" 的已知 sha256，校验哈希算法正确
        assert_eq!(
            exe["sha256"],
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_manifest_missing_release() {
        // 版本目录有文件但程序同目录没有 release.txt → 视为还没有发布（404）
        let dir = std::env::temp_dir().join(format!("rel-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let publish = dir.join("1.3.0");
        std::fs::create_dir_all(&publish).unwrap();
        std::fs::write(publish.join("x.txt"), "hi").unwrap();
        assert!(build_manifest(&dir).is_none());

        // release.txt 写成路径（非单级目录名）同样视为未发布
        std::fs::write(dir.join(RELEASE_TXT), "../evil").unwrap();
        assert!(build_manifest(&dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_release_uses_version_dir() {
        let dir = std::env::temp_dir().join(format!("rel-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join(RELEASE_TXT), "2.1.0\n").unwrap();
        let (version, publish) = resolve_release(&dir).unwrap();
        assert_eq!(version, "2.1.0");
        assert_eq!(publish, dir.join("2.1.0"));

        // 版本号不能当路径用
        std::fs::write(dir.join(RELEASE_TXT), "../evil").unwrap();
        assert!(resolve_release(&dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_rel_path_ok() {
        let p = safe_rel_path("resources/a.bin").unwrap();
        let parts: Vec<String> = p.iter().map(|c| c.to_string_lossy().to_string()).collect();
        assert_eq!(parts, vec!["resources", "a.bin"]);
        assert_eq!(safe_rel_path("../evil").map(|p| p.iter().count()), None);
        assert_eq!(safe_rel_path("").is_none(), true);
        assert!(safe_rel_path("a/./b").is_some());
    }

    /// 验证 axum 0.7（matchit 0.7）通配路由语法：`*rest` 匹配多段路径。
    /// 注意 `{name}` 是 axum 0.8 才支持的语法，升级 axum 0.8 时需同步改回。
    #[test]
    fn wildcard_route_syntax_supported() {
        let mut wild = matchit::Router::new();
        wild.insert("/update/*rest", 2).unwrap();
        let m = wild.at("/update/resources/data.bin");
        assert!(m.is_ok(), "*rest 语法匹配失败: {:?}", m.err());
        assert_eq!(m.unwrap().params.get("rest"), Some("resources/data.bin"));
    }

    /// /update 前缀下的选路优先级（axum 的匹配同样基于 matchit）：
    /// 精确路由 /update/version.json 优先于通配 /update/*rest，
    /// 管理页 / 与 /api/* 不受通配影响。
    #[test]
    fn update_prefix_route_priority() {
        let mut r = matchit::Router::new();
        r.insert("/", 1u8).unwrap();
        r.insert("/update/version.json", 2).unwrap();
        r.insert("/update/*rest", 3).unwrap();
        r.insert("/api/files", 4).unwrap();
        r.insert("/api/file/:name", 5).unwrap();

        assert_eq!(r.at("/").unwrap().value, &1);
        assert_eq!(r.at("/update/version.json").unwrap().value, &2);
        assert_eq!(r.at("/update/simuxchange.exe").unwrap().value, &3);
        assert_eq!(r.at("/update/Linux/simx-server").unwrap().value, &3);
        assert_eq!(r.at("/api/files").unwrap().value, &4);
        assert_eq!(r.at("/api/file/x.exe").unwrap().value, &5);
    }
}