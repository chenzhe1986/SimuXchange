// simx-update-server：SimuXchange 升级服务器（单文件程序，Windows/Linux 均可运行）
//
// 设计动机（为什么需要它）：
//   桌面端与后端只要求一个 HTTP 静态目录（version.json + 安装包）就能完成升级检查。
//   但"手写 version.json、手动拷贝安装包"容易出错，本程序把这些动作全部自动化：
//     1. 网页上传安装包（含 Linux 后端）与更新说明，无需登录服务器；
//     2. 自动扫描文件目录生成 version.json（版本号从安装包文件名解析，
//        如 SimuXchange_1.1.0_x64-setup.exe → 1.1.0），永远与磁盘实际文件一致；
//     3. 同时提供静态文件下载，一个程序完成"目录 + 清单"两件事，
//        客户端直接填 http://<IP>:8080 即可。
//
// 数据流（面向不熟 Rust 的维护者）：
//   目录内文件（安装包/simx-server/notes.json/version.json）是唯一事实来源，
//   所有 HTTP 接口只是"读目录 / 写目录"，无任何数据库：
//     - GET  /version.json  → 实时扫描目录生成（无文件时返回 404，客户端静默跳过）
//     - POST /api/upload    → 保存文件 + 记录更新说明，随后重写磁盘 version.json
//     - DELETE /api/file/x  → 删除文件，随后重写磁盘 version.json
//   notes.json 是"版本号 → 更新说明数组"的映射，由上传接口维护，
//   手工放入的安装包没有说明也能自动出现在清单里（notes 为空数组）。

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{header, StatusCode},
    routing::{delete, get, post},
    Json, Router,
};
use serde_json::{json, Map, Value};
use std::{
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

/// 全局共享状态：文件目录（安装包、simx-server、notes.json、version.json 都放这里）
#[derive(Clone)]
struct AppState {
    dir: Arc<PathBuf>,
}

/// 安装包文件名前缀/后缀，用于从文件名识别安装包并解析版本号
const PKG_PREFIX: &str = "SimuXchange_";
const PKG_SUFFIX: &str = "_x64-setup.exe";
/// Linux 后端在目录中的固定文件名
const LINUX_BIN: &str = "simx-server";

#[tokio::main]
async fn main() {
    // 命令行参数解析（--listen 监听地址、--dir 文件目录），不引入 clap 保持依赖精简
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
                println!("用法: simx-update-server [--listen IP:端口] [--dir 文件目录]");
                println!("默认: --listen 0.0.0.0:8080  --dir ./update");
                return;
            }
            _ => {}
        }
    }

    // 目录不存在则自动创建（与 simx-server 自动建数据目录的行为一致）
    std::fs::create_dir_all(&dir).expect("创建文件目录失败");
    let state = AppState {
        dir: Arc::new(dir),
    };

    let app = Router::new()
        .route("/", get(index_html))                                    // 管理页面
        .route("/version.json", get(version_json))                      // 升级清单（客户端检查用）
        .route("/files/:name", get(download_file))                      // 安装包下载
        .route("/api/files", get(list_files))                           // 管理页文件列表
        .route("/api/upload", post(upload))                             // 网页上传发布
        .route("/api/file/:name", delete(delete_file))                  // 删除文件
        // 未匹配的路由记录日志（404 时便于排查客户端配置错误）
        .fallback(|uri: axum::http::Uri| async move {
            eprintln!("[simx-update-server] 未匹配路由: {}", uri.path());
            (StatusCode::NOT_FOUND, "not found")
        })
        // axum 默认请求体上限 2MB，安装包远大于此，调大到 1GB（仅本服务，无对外暴露风险）
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .expect("端口被占用，请换一个端口（如 8081）");
    println!("升级服务器已启动，浏览器打开 http://{} 管理发布", listen);
    println!("客户端升级地址填: http://{} （或本机局域网 IP 同端口）", listen);
    axum::serve(listener, app).await.expect("服务器异常退出");
}

// ---------------------------------------------------------------------------
// 升级清单（version.json）生成
// ---------------------------------------------------------------------------

/// 版本号比较：返回 a 与 b 的大小关系（1 大 / -1 小 / 0 相等）。
/// 按数字点分段逐段比较（1.10.0 > 1.9.0），与客户端 updater.ts 的版本比较一致。
fn version_cmp(a: &str, b: &str) -> i32 {
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

/// 从安装包文件名解析版本号：SimuXchange_1.1.0_x64-setup.exe → "1.1.0"。
/// 解析失败（不是安装包、格式不对）返回 None，该文件不会被写进清单。
fn parse_version_from_filename(name: &str) -> Option<String> {
    let rest = name.strip_prefix(PKG_PREFIX)?.strip_suffix(PKG_SUFFIX)?;
    let ver = rest.split('_').next()?;
    // 校验是"数字.数字.数字"格式（至少三段，与项目版本号规范一致），防止乱起名的文件进入清单
    let parts: Vec<&str> = ver.split('.').collect();
    let valid = parts.len() >= 3
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if valid {
        Some(ver.to_string())
    } else {
        None
    }
}

/// 文件名安全清洗：只保留最后一段（去掉路径部分），防止路径穿越。
/// 上传方给的任何名字都必须经过这里才能拼进目录路径。
fn safe_name(name: &str) -> Option<String> {
    let base = FsPath::new(name).file_name()?.to_str()?.to_string();
    if base.is_empty() {
        None
    } else {
        Some(base)
    }
}

/// 扫描文件目录生成升级清单（version.json 的内容）。
/// 规则：
///   - 遍历目录下所有安装包，解析版本号，取**版本号最大**的作为当前版本；
///   - 目录里的 simx-server 文件作为 Linux 后端文件名（可缺省）；
///   - 更新说明从 notes.json 按版本号取（上传时填写的）。
/// 返回 None 表示目录里没有任何安装包（此时 version.json 返回 404，客户端静默）。
fn build_manifest(dir: &FsPath) -> Option<Value> {
    // 1. 扫描安装包：文件名 → 版本号，挑出版本号最大的
    let mut best: Option<(String, String)> = None; // (version, 文件名)
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(ver) = parse_version_from_filename(&name) {
                let newer = match &best {
                    Some((cur, _)) => version_cmp(&ver, cur) > 0,
                    None => true,
                };
                if newer {
                    best = Some((ver, name));
                }
            }
        }
    }
    let (version, win_installer) = best?;

    // 2. Linux 后端：目录里是否存在 simx-server 文件（子目录不算）
    let linux_server = dir.join(LINUX_BIN);
    let linux_name = if linux_server.is_file() { Some(LINUX_BIN.to_string()) } else { None };

    // 3. 更新说明：notes.json → { "1.1.0": ["...", "..."] }
    let notes_map: Map<String, Value> = read_json(&dir.join("notes.json"));
    let notes = notes_map
        .get(&version)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    Some(json!({
        "version": version,
        "notes": notes,
        "winInstaller": win_installer,
        "linuxServer": linux_name,
    }))
}

/// 读 JSON 文件，失败（不存在/格式坏）时返回空 Map，不 panic
fn read_json(path: &FsPath) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 重写磁盘上的 version.json（上传/删除后调用），
/// 保证目录里始终有一份与文件一致的最新清单，方便检查/外部静态服务复用
fn rewrite_version_json(dir: &FsPath) {
    if let Some(manifest) = build_manifest(dir) {
        let _ = std::fs::write(dir.join("version.json"), serde_json::to_string_pretty(&manifest).unwrap_or_default());
    } else {
        // 目录空了就把旧清单清掉，避免残留旧版本号误导
        let _ = std::fs::remove_file(dir.join("version.json"));
    }
}

/// 目录内所有文件列表（名字/大小/修改时间），供管理页面展示
fn list_dir_files(dir: &FsPath) -> Vec<Value> {
    let mut files: Vec<Value> = std::fs::read_dir(dir)
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
    files
}

// ---------------------------------------------------------------------------
// HTTP 接口
// ---------------------------------------------------------------------------

/// 管理页面（内嵌 HTML，纯静态 JS 调接口，不依赖任何前端构建）
async fn index_html() -> &'static str {
    INDEX_HTML
}

/// GET /version.json：实时扫描目录生成升级清单（客户端升级检查入口）
async fn version_json(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    build_manifest(&state.dir).map(Json).ok_or(StatusCode::NOT_FOUND)
}

/// GET /files/{name}：下载安装包等文件（文件不存在返回 404）
async fn download_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<(StatusCode, [(header::HeaderName, String); 2], Bytes), StatusCode> {
    let file_name = safe_name(&name).ok_or(StatusCode::BAD_REQUEST)?;
    let path = state.dir.join(&file_name);
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

/// GET /api/files：文件列表（管理页面展示用）
async fn list_files(State(state): State<AppState>) -> Json<Vec<Value>> {
    Json(list_dir_files(&state.dir))
}

/// POST /api/upload：网页上传发布。
/// multipart 字段：
///   - file  安装包（必需，可同时是 .exe 或任何格式，版本号从文件名解析）
///   - linux Linux 后端二进制（可选，固定保存为 simx-server）
///   - notes 更新说明（可选，多行文本，每行一条）
/// 完成后重写磁盘 version.json。
async fn upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, (StatusCode, String)> {
    // 先收集所有字段再落盘：任何一个字段读失败都不产生半成品文件
    let mut win_file: Option<(String, Bytes)> = None;
    let mut linux_file: Option<Bytes> = None;
    let mut notes: Vec<String> = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("读取上传失败: {e}")))?
    {
        match field.name() {
            Some("file") => {
                let name = field.file_name().unwrap_or("").to_string();
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("读取安装包失败: {e}")))?;
                win_file = Some((name, data));
            }
            Some("linux") => {
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("读取 Linux 后端失败: {e}")))?;
                linux_file = Some(data);
            }
            Some("notes") => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("读取更新说明失败: {e}")))?;
                notes = text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            }
            _ => {} // 其他字段忽略
        }
    }

    let win = win_file.ok_or((StatusCode::BAD_REQUEST, "缺少安装包文件（字段名 file）".to_string()))?;
    let file_name = safe_name(&win.0).ok_or((StatusCode::BAD_REQUEST, "安装包文件名非法".to_string()))?;

    // 解析版本号（决定 notes 挂到哪个版本下）；解析不出版本号的文件也能上传（仅提供下载），但不进清单
    let version = parse_version_from_filename(&file_name);

    // 落盘：先写安装包，再写 Linux 后端（若有）
    tokio::fs::write(state.dir.join(&file_name), &win.1)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("保存文件失败: {e}")))?;
    if let Some(data) = linux_file {
        tokio::fs::write(state.dir.join(LINUX_BIN), &data)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("保存 Linux 后端失败: {e}")))?;
    }

    // 记录更新说明：notes.json["版本"] = [说明...]
    if let (Some(ver), false) = (version.as_ref(), notes.is_empty()) {
        let mut map = read_json(&state.dir.join("notes.json"));
        map.insert(ver.clone(), Value::Array(notes.into_iter().map(Value::String).collect()));
        let _ = std::fs::write(state.dir.join("notes.json"), serde_json::to_string_pretty(&map).unwrap_or_default());
    }

    rewrite_version_json(&state.dir);

    let manifest = build_manifest(&state.dir).unwrap_or(json!({}));
    Ok(Json(json!({
        "ok": true,
        "file": file_name,
        "manifest": manifest,
    })))
}

/// DELETE /api/file/{name}：删除目录内文件（只允许删本目录的普通文件），删除后重写清单。
/// notes.json 与 version.json 是系统文件，不允许通过此接口删除，防止误删配置。
async fn delete_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let file_name = safe_name(&name).ok_or((StatusCode::BAD_REQUEST, "文件名非法".to_string()))?;
    if file_name == "notes.json" || file_name == "version.json" {
        return Err((StatusCode::FORBIDDEN, "系统文件不允许删除".to_string()));
    }
    let path = state.dir.join(&file_name);
    if !path.is_file() {
        return Err((StatusCode::NOT_FOUND, "文件不存在".to_string()));
    }
    tokio::fs::remove_file(&path)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("删除失败: {e}")))?;
    rewrite_version_json(&state.dir);
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// 管理页面（内嵌 HTML；页面地址即服务器地址，客户端填同一地址即可）
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
  .notes li { margin: 2px 0; font-size: 14px; }
  .empty { color: #aaa; font-size: 13px; }
  input[type=file], textarea { width: 100%; margin: 6px 0 10px; }
  textarea { height: 90px; resize: vertical; font-family: inherit; font-size: 13px; padding: 6px; }
  button { background: #2b7de9; color: #fff; border: none; border-radius: 5px; padding: 8px 22px; font-size: 14px; cursor: pointer; }
  button:hover { background: #1f6fd3; }
  button:disabled { background: #9db8dd; cursor: not-allowed; }
  .del { background: none; color: #d9534f; border: 1px solid #d9534f; padding: 3px 10px; font-size: 12px; border-radius: 4px; }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th, td { text-align: left; padding: 7px 8px; border-bottom: 1px solid #eef0f2; }
  th { color: #666; font-weight: normal; }
  .msg { margin: 10px 0; font-size: 13px; }
  .msg.ok { color: #2a9d4e; }
  .msg.err { color: #d9534f; }
  .field-label { font-size: 13px; color: #555; }
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
    <div id="msg" class="msg"></div>
    <form id="uploadForm">
      <div class="field-label">安装包（文件名须含版本号，如 SimuXchange_1.1.0_x64-setup.exe）</div>
      <input type="file" name="file" required>
      <div class="field-label">更新说明（每行一条，可留空）</div>
      <textarea name="notes" placeholder="新增…&#10;修复…"></textarea>
      <div class="field-label">Linux 后端 simx-server（可选，随版本更新时一起上传）</div>
      <input type="file" name="linux">
      <button id="uploadBtn" type="submit">上传发布</button>
    </form>
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
  // 升级清单
  const man = await fetch("/version.json").then(r => r.ok ? r.json() : null).catch(() => null);
  if (man) {
    const notes = (man.notes || []).length
      ? "<ul class='notes'>" + man.notes.map(n => "<li>" + n + "</li>").join("") + "</ul>"
      : "<div class='empty'>（无更新说明）</div>";
    $("manifest").innerHTML =
      "<div class='kv'><span class='k'>版本号</span><span class='v'><b>" + man.version + "</b></span></div>" +
      "<div class='kv'><span class='k'>安装包</span><span class='v'>" + man.winInstaller + "</span></div>" +
      "<div class='kv'><span class='k'>Linux 后端</span><span class='v'>" + (man.linuxServer || "未上传") + "</span></div>" +
      "<div class='kv'><span class='k'>更新说明</span><span class='v'></span></div>" + notes;
  } else {
    $("manifest").innerHTML = "<div class='empty'>目录中还没有安装包，上传后自动生成清单</div>";
  }
  // 文件列表
  const files = await fetch("/api/files").then(r => r.json()).catch(() => []);
  const rows = files.length ? files.map(f =>
    "<tr><td>" + f.name + "</td><td>" + fmtSize(f.size) + "</td><td>" + fmtTime(f.modified) +
    "</td><td><button class='del' onclick=\"delFile('" + encodeURIComponent(f.name) + "')\">删除</button></td></tr>"
  ).join("") : "<tr><td colspan='4' class='empty'>（空）</td></tr>";
  $("fileTable").innerHTML = "<tr><th>文件名</th><th>大小</th><th>修改时间</th><th></th></tr>" + rows;
}

async function delFile(name) {
  if (!confirm("确认删除 " + decodeURIComponent(name) + " ？")) return;
  const resp = await fetch("/api/file/" + name, { method: "DELETE" });
  show(resp.ok ? "已删除 " + decodeURIComponent(name) : await resp.text(), resp.ok);
  refresh();
}

function show(text, ok) {
  const m = $("msg");
  m.textContent = text;
  m.className = "msg " + (ok ? "ok" : "err");
}

$("uploadForm").addEventListener("submit", async (e) => {
  e.preventDefault();
  const btn = $("uploadBtn");
  btn.disabled = true;
  btn.textContent = "上传中…";
  const fd = new FormData($("uploadForm"));
  try {
    const resp = await fetch("/api/upload", { method: "POST", body: fd });
    const data = await resp.json().catch(() => null);
    if (resp.ok && data && data.ok) {
      show("发布成功：" + data.file + "（版本 " + (data.manifest.version || "未识别") + "）", true);
      $("uploadForm").reset();
      refresh();
    } else {
      show((data && data.error) || "上传失败", false);
    }
  } catch (err) {
    show("上传失败：" + err, false);
  } finally {
    btn.disabled = false;
    btn.textContent = "上传发布";
  }
});

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
    fn parse_version_normal() {
        assert_eq!(
            parse_version_from_filename("SimuXchange_1.1.0_x64-setup.exe"),
            Some("1.1.0".to_string())
        );
        assert_eq!(
            parse_version_from_filename("SimuXchange_1.10.0_x64-setup.exe"),
            Some("1.10.0".to_string())
        );
    }

    #[test]
    fn parse_version_bad_name() {
        // 不是安装包 / 版本号格式不对 → None
        assert_eq!(parse_version_from_filename("readme.txt"), None);
        assert_eq!(parse_version_from_filename("SimuXchange_x64-setup.exe"), None);
        assert_eq!(parse_version_from_filename("SimuXchange_abc_x64-setup.exe"), None);
        assert_eq!(parse_version_from_filename("SimuXchange_1.1_x64-setup.exe"), None); // 至少两段点分
        assert_eq!(parse_version_from_filename("MyApp_1.1.0_x64-setup.exe"), None);
    }

    #[test]
    fn version_compare() {
        assert!(version_cmp("1.10.0", "1.9.0") > 0);
        assert!(version_cmp("1.1.0", "1.1.0") == 0);
        assert!(version_cmp("1.1.0", "1.1.1") < 0);
        assert!(version_cmp("1.1.0", "1.2.0") < 0);
        assert!(version_cmp("2.0.0", "1.99.99") > 0);
    }

    #[test]
    fn safe_name_strips_paths() {
        assert_eq!(safe_name("SimuXchange_1.1.0_x64-setup.exe").as_deref(), Some("SimuXchange_1.1.0_x64-setup.exe"));
        // 路径穿越尝试只保留最后一段
        assert_eq!(safe_name("..\\..\\evil.exe").as_deref(), Some("evil.exe"));
        assert_eq!(safe_name("C:/x/y/a.exe").as_deref(), Some("a.exe"));
        assert_eq!(safe_name(""), None);
    }

    /// 扫描目录生成清单：用临时目录模拟一次发布
    #[test]
    fn build_manifest_picks_newest_and_merges_notes() {
        let dir = std::env::temp_dir().join(format!("update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SimuXchange_1.0.0_x64-setup.exe"), "old").unwrap();
        std::fs::write(dir.join("SimuXchange_1.2.0_x64-setup.exe"), "new").unwrap();
        std::fs::write(dir.join("simx-server"), "linux-bin").unwrap();
        std::fs::write(
            dir.join("notes.json"),
            r#"{"1.0.0":["旧版说明"],"1.2.0":["新版说明A","新版说明B"]}"#,
        )
        .unwrap();

        let manifest = build_manifest(&dir).unwrap();
        assert_eq!(manifest["version"], "1.2.0");
        assert_eq!(manifest["winInstaller"], "SimuXchange_1.2.0_x64-setup.exe");
        assert_eq!(manifest["linuxServer"], "simx-server");
        assert_eq!(manifest["notes"].as_array().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空目录 / 无安装包时返回 None（version.json 应 404）
    #[test]
    fn build_manifest_empty_dir() {
        let dir = std::env::temp_dir().join(format!("update-test-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("readme.txt"), "hi").unwrap();
        assert!(build_manifest(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 验证当前 axum 0.7（matchit 0.7）的路由参数语法：`:name` 匹配单段路径。
    /// 注意：`{name}` 是 axum 0.8 才支持的语法，升级 axum 0.8 时需同步改回
    #[test]
    fn route_syntax_supported() {
        let mut router = matchit::Router::new();
        router.insert("/files/:name", 1).unwrap();
        let matched = router.at("/files/notes.json");
        assert!(matched.is_ok(), ":name 语法匹配失败: {:?}", matched.err());
        assert_eq!(matched.unwrap().params.get("name"), Some("notes.json"));

        // 静态段与参数段共存（/api/files 与 /api/file/:name）
        let mut router2 = matchit::Router::new();
        router2.insert("/api/files", 1).unwrap();
        router2.insert("/api/file/:name", 2).unwrap();
        assert!(router2.at("/api/file/xxx.exe").is_ok());
        assert!(router2.at("/api/files").is_ok());
    }
}
