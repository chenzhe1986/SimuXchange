// simx-update-server：SimuXchange 升级服务器（单文件程序，Windows/Linux 均可运行）
//
// 设计动机（为什么需要它）：
//   桌面端只需要一个 HTTP 可见的目录（version.json + 发布文件）就能完成升级
//   检查与下载。本程序把这个目录自动化：
//     1. 会自动扫描目录生成 version.json（文件清单 + 每个文件的 size/sha256）；
//     2. 目录里的每个文件（含子目录）都提供下载；
//     3. 一个程序完成"目录 + 清单"两件事，客户端直接填 http://<IP>:8080 即可。
//
// 发布流程（服务器上怎样放文件）：
//   1. 把整个发布目录（simuxchange.exe 与其它文件/子目录）拷进 --dir 目录；
//   2. 放一个仅含版本号的文件：release.json（{"version": "1.3.0"}）
//      或用纯文本 release.txt（内容就是一行 "1.3.0"）—— 二选一即可，
//      服务器通过它知道当前发布版本（客户端拿它和自己比版本号）。
//   不需要手写任何文件清单：服务器每次 GET /version.json 都会实时编出
//   全量文件清单（含子目录、大小、sha256），客户端据此把整个目录下载下来。
//
// 设计要点：
//   - 目录内文件是唯一事实来源，无数据库；服务器文档性质文件
//     （version.json / release.json / release.txt / notes.json）自动排除在
//     下载清单之外，不会混进用户的程序目录。
//   - 客户端下载时"主 exe 最后"由客户端负责（清单顺序无要求）。

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, StatusCode},
    routing::{delete, get},
    Json, Router,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

/// 全局共享状态：发布文件目录
#[derive(Clone)]
struct AppState {
    dir: Arc<PathBuf>,
}

/// 服务器上的版本文件（二选一，内容就是版本号）：
///   release.json -> {"version": "1.3.0"}
///   release.txt  -> 行为字符串 "1.3.0"
const RELEASE_JSON: &str = "release.json";
const RELEASE_TXT: &str = "release.txt";
/// 不进发布清单的文件（服务器自己管的文件，客户端不得下载）
const SKIP_FILES: [&str; 4] = [RELEASE_JSON, RELEASE_TXT, "notes.json", "version.json"];

#[tokio::main]
async fn main() {
    // 命令行参数解析（--listen 监听地址、--dir 发布目录），不引入 clap 保持依赖精简
    let mut listen = "0.0.0.0:8080".to_string();
    let mut dir = PathBuf::from("update");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                if let Some(v) = args.next() {
                    listen = v;
                }
            }
            "--dir" => {
                if let Some(v) = args.next() {
                    dir = PathBuf::from(v);
                }
            }
            "--help" | "-h" => {
                println!("用法: simx-update-server [--listen IP:端口] [--dir 发布目录] [-V|--version]");
                println!("默认: --listen 0.0.0.0:8080  --dir ./update");
                println!("发布：把程序目录拷进 --dir，再放 release.txt（一行版本号）或 release.json（{{\"version\":\"1.3.0\"}}）");
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

    // 目录不存在则自动创建（与 simx-server 自动建数据目录的行为一致）
    std::fs::create_dir_all(&dir).expect("创建发布目录失败");
    let state = AppState {
        dir: Arc::new(dir),
    };

    let app = Router::new()
        .route("/", get(index))                  // 管理页面
        .route("/version.json", get(version_json)) // 升级清单（客户端检查/下载用）
        .route("/files/*rest", get(download_file)) // 发布文件下载（含子目录）
        .route("/api/files", get(list_files))      // 管理页文件列表
        .route("/api/file/:name", delete(delete_file)) // 删除文件（管理页用）
        // 未匹配的路由记录日志（404 时便于排查客户端配置错误）
        .fallback(|uri: axum::http::Uri| async move {
            eprintln!("[simx-update-server] 未匹配路由: {}", uri.path());
            (StatusCode::NOT_FOUND, "not found")
        })
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .expect("端口被占用，请换一个端口（如 8081）");
    println!("升级服务器已启动 v{}，浏览器打开 http://{} 管理发布", env!("CARGO_PKG_VERSION"), listen);
    println!("客户端升级地址填: http://{} （或本机局域网 IP 同端口）", listen);
    axum::serve(listener, app).await.expect("服务器异常退出");
}

// ---------------------------------------------------------------------------
// 核心逻辑：版本读取 + 清单生成
// ---------------------------------------------------------------------------

/// 从发布目录读版本号：release.txt（纯文本）或 release.json 的 version 字段。
/// 两个文件都没有 → None（此时 version.json 返回 404，客户端静默跳过）。
fn read_release_version(dir: &FsPath) -> Option<String> {
    // 纯文本优先（最简：内容就是一行版本号）
    if let Ok(s) = std::fs::read_to_string(dir.join(RELEASE_TXT)) {
        let v = s.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    // JSON 兼容（{"version": "1.3.0"}）
    if let Ok(s) = std::fs::read_to_string(dir.join(RELEASE_JSON)) {
        if let Ok(v) = serde_json::from_str::<Map<String, Value>>(&s) {
            if let Some(ver) = v.get("version").and_then(|x| x.as_str()) {
                let ver = ver.trim();
                if !ver.is_empty() {
                    return Some(ver.to_string());
                }
            }
        }
    }
    None
}

/// 递归收集目录下所有"发布文件"（相对路径 + 大小 + sha256）。
/// rel 是相对目录根的路径（空表示根），Windows 分隔符统一转成 '/'，
/// 客户端下载时用相同相对路径落盘，保证目录结构原样还原。
fn collect_release_files(dir: &FsPath, rel: &FsPath, out: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        // 服务器自己的管理工作区/描述文件不进发布清单（用户说了"json 等文件不用下载"）
        if rel.as_os_str().is_empty() && SKIP_FILES.contains(&name_str.as_str()) {
            continue;
        }
        // 隐藏文件（.git 等）也跳过
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
///   { "version": "1.3.0", "files": [ {name,size,sha256}, ... ] }
/// 没有 release 版本文件或目录里没有发布文件 → None（version.json 返回 404）
fn build_manifest(dir: &FsPath) -> Option<Value> {
    let version = read_release_version(dir)?;
    let mut files: Vec<Value> = Vec::new();
    collect_release_files(dir, &PathBuf::new(), &mut files);
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

/// GET /version.json：扫描发布目录生成升级清单（客户端检查/下载入口）
async fn version_json(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    build_manifest(&state.dir).map(Json).ok_or(StatusCode::NOT_FOUND)
}

/// GET /files/{path...}：下载发布文件（支持多级子目录）
/// path 逐段清洗（拒绝 .. 等穿越），并再以 canonicalize 校验落在发布目录内。
async fn download_file(
    State(state): State<AppState>,
    Path(rest): Path<String>,
) -> Result<(StatusCode, [(header::HeaderName, String); 2], Bytes), StatusCode> {
    let rel = safe_rel_path(&rest).ok_or(StatusCode::BAD_REQUEST)?;
    // 服务器自己的版本/清单文件一律不提供下载（用户明确"json 等文件不用下载"）；
    // 多级路径（子目录里同名文件）不受影响
    if rel.components().count() == 1 {
        if let Some(name) = rel.file_name().and_then(|n| n.to_str()) {
            if SKIP_FILES.contains(&name) {
                return Err(StatusCode::NOT_FOUND);
            }
        }
    }
    let path = state.dir.join(&rel);
    // 防路径穿过的最后一层兜底：解析后的路径必须仍在发布目录内
    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
    let base = state.dir.canonicalize().unwrap_or_else(|_| (*state.dir).clone());
    if !canonical.starts_with(&base) || !canonical.is_file() {
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

/// GET /api/files：目录内文件列表（管理页面展示用）
async fn list_files(State(state): State<AppState>) -> Json<Vec<Value>> {
    let mut files: Vec<Value> = std::fs::read_dir(&*state.dir)
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

/// DELETE /api/file/{name}：删除目录里文件（误上传时整理用）
async fn delete_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // 只取文件名字尾段，防止路径穿越
    let file_name = FsPath::new(&name)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or((StatusCode::BAD_REQUEST, "文件名非法".to_string()))?
        .to_string();
    if SKIP_FILES.contains(&file_name.as_str()) {
        return Err((StatusCode::FORBIDDEN, "系统文件不允许删除".to_string()));
    }
    let path = state.dir.join(&file_name);
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
  <div class="hint">客户端升级地址填：<b>http://&lt;本机IP&gt;:端口</b>（即本页面地址，去掉 / 后的部分）</div>

  <div class="card">
    <h2>当前升级清单</h2>
    <div id="manifest" class="empty">加载中…</div>
  </div>

  <div class="card">
    <h2>发布新版本</h2>
    <div class="hint" style="margin:0 0 10px">
      把整个程序目录拷进本服务器 --dir 目录，再放一个版本文件即可（二选一）：
      <pre>release.txt  内容仅一行，如：1.3.0&#10;release.json 内容：{"version": "1.3.0"}</pre>
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
  const man = await fetch("/version.json").then(r => r.ok ? r.json() : null).catch(() => null);
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
    fn release_version_from_txt_and_json() {
        let dir = std::env::temp_dir().join(format!("relver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // release.txt 纯文本
        std::fs::write(dir.join(RELEASE_TXT), " 1.10.0 \n").unwrap();
        assert_eq!(read_release_version(&dir).as_deref(), Some("1.10.0"));

        // release.json 的 version 字段
        std::fs::remove_file(dir.join(RELEASE_TXT)).unwrap();
        std::fs::write(dir.join(RELEASE_JSON), r#"{"version": "1.3.0"}"#).unwrap();
        assert_eq!(read_release_version(&dir).as_deref(), Some("1.3.0"));

        // 都没有 → None
        std::fs::remove_file(dir.join(RELEASE_JSON)).unwrap();
        assert!(read_release_version(&dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_manifest_scan_dir_with_hash() {
        let dir = std::env::temp_dir().join(format!("rel-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("resources")).unwrap();
        std::fs::write(dir.join(RELEASE_TXT), "1.3.0").unwrap();
        std::fs::write(dir.join("simuxchange.exe"), b"hello").unwrap();
        std::fs::write(dir.join("resources").join("data.bin"), b"world!").unwrap();
        std::fs::write(dir.join("version.json"), "{}").unwrap(); // 系统文件不进清单

        let manifest = build_manifest(&dir).unwrap();
        assert_eq!(manifest["version"], "1.3.0");
        let files = manifest["files"].as_array().unwrap();
        assert_eq!(files.len(), 2, "系统/描述文件不应进清单: {files:?}");
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
        // 目录有文件但没有 release 版本文件 → 视为还没有发布（404）
        let dir = std::env::temp_dir().join(format!("rel-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.txt"), "hi").unwrap();
        assert!(build_manifest(&dir).is_none());
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
        wild.insert("/files/*rest", 2).unwrap();
        let m = wild.at("/files/resources/data.bin");
        assert!(m.is_ok(), "*rest 语法匹配失败: {:?}", m.err());
        assert_eq!(m.unwrap().params.get("rest"), Some("resources/data.bin"));
    }
}