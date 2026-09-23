// 发布版不弹黑色控制台窗口（Windows 上 GUI 程序的惯例写法）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! SimuXchange 桌面端：内嵌 simx-core 引擎的 Tauri 壳
//!
//! # 程序定位
//!
//! Tauri 的工作方式：界面是网页（Vue），跑在系统自带的浏览器内核里；
//! 后端是这个 Rust 程序。两者通过 Tauri 的 command 机制通信：
//! 前端调 invoke("dispatch", {...})，就会执行下面带 #[tauri::command]
//! 标记的同名函数；后端用 emit("engine-event", ...) 向前端推事件。
//!
//! 本文件只做三件事：确定数据目录 → 创建引擎 → 把引擎接入 Tauri
//! （注册两个 command + 把引擎日志转发给前端）。业务逻辑全在
//! simx-core 里，这里只是个薄薄的壳。

use simx_core::config::AppConfig;
use simx_core::engine::Engine;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{Emitter, Manager, State};

mod updater;

/// 解析配置/数据目录：优先取可执行文件目录（存在 simx.config.json 时），
/// 其次取当前工作目录及其上级（开发模式），最后回退到可执行文件目录。
///
/// 为什么这么绕：发布版的 exe 和配置文件放在同一目录，而开发模式
/// （npm run tauri dev）的 exe 藏在 target/debug 深处，工作目录才是
/// 项目目录——所以要把几个候选位置都找一遍，谁有配置文件就用谁。
fn resolve_data_dir() -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.clone());
        if let Some(parent) = cwd.parent() {
            candidates.push(parent.to_path_buf());
        }
    }
    for c in &candidates {
        if c.join("simx.config.json").exists() {
            return c.clone();
        }
    }
    candidates
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 读应用配置 simx.config.json；文件不存在或格式错误时用默认值
/// （默认 = 本地模式，不连远程后端）
fn load_app_config(dir: &PathBuf) -> AppConfig {
    let path = dir.join("simx.config.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => AppConfig::default(),
    }
}

/// 前端读取应用配置（决定连接本地引擎还是远程后端）。
/// Mutex 包一层：界面“后端连接设置”保存时会写内存，读取要拿到最新值
#[tauri::command]
fn get_app_config(cfg: State<'_, Mutex<AppConfig>>) -> AppConfig {
    cfg.inner().lock().map(|c| c.clone()).unwrap_or_default()
}

/// 前端保存应用配置（界面“后端连接设置”里切换本地/远程模式时调用）：
/// 写回 simx.config.json 并更新内存，下次启动仍然生效。
#[tauri::command]
fn set_app_config(
    cfg: State<'_, Mutex<AppConfig>>,
    config: AppConfig,
) -> Result<AppConfig, String> {
    // 与启动时同一套目录解析逻辑，保证读写的是同一份配置文件
    let path = resolve_data_dir().join("simx.config.json");
    let text =
        serde_json::to_string_pretty(&config).map_err(|e| format!("序列化配置失败: {}", e))?;
    std::fs::write(&path, text).map_err(|e| format!("写入 {} 失败: {}", path.display(), e))?;
    let mut guard = cfg.inner().lock().map_err(|e| e.to_string())?;
    *guard = config.clone();
    Ok(config)
}

/// 统一控制接口：前端所有操作（启停网关、存配置、拉快照…）都走这一个
/// command，payload 为 JSON 命令，具体格式见 simx_core::api。
/// 本地桌面端的前端就在本机（Tauri 进程内调用），操作日志里 IP 固定记
/// 127.0.0.1；远程模式（WebSocket）由 simx-server 记录真实对端 IP。
#[tauri::command]
async fn dispatch(
    engine: State<'_, Engine>,
    payload: serde_json::Value,
) -> Result<serde_json::Value, String> {
    Ok(simx_core::api::dispatch(engine.inner(), payload, "127.0.0.1").await)
}

/// "AI 生成脚本"的模型 API 透传（PlatformEditor 的「AI 生成脚本」按钮触发）。
///
/// 为什么要桌面壳代发：模型接口不返回 CORS 头，WebView 里的前端 fetch 会被
/// 浏览器拦截。本 command 只做一次 HTTPS 转发——url / 请求头 / 请求体都由
/// 前端拼好（模型配置在 src/llm.ts 里维护），壳不理解任何业务语义；
/// 模拟引擎（simx-core / Linux 后端）完全不参与该功能。
///
/// TLS 注意：ureq 的 native-tls 特性只编译实现、不会作为默认后端启用
/// （默认是 rustls，本项目为避开 ring 的 C/汇编编译没开），必须在这里用
/// AgentBuilder::tls_connector 显式挂上系统 TLS（Windows = schannel）。
/// 大模型推理可能较慢，整体超时放宽到 180 秒。
#[tauri::command]
async fn llm_chat(
    url: String,
    headers: std::collections::HashMap<String, String>,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // 系统原生 TLS 连接器（Windows 上是 schannel）；创建失败极少见（本地
        // 安全环境异常），报错文本直接给前端弹窗
        let tls = native_tls::TlsConnector::new()
            .map_err(|e| format!("初始化系统 TLS 失败: {}", e))?;
        let agent = ureq::AgentBuilder::new()
            .tls_connector(std::sync::Arc::new(tls))
            .timeout(std::time::Duration::from_secs(180))
            .build();
        let mut req = agent.post(&url);
        for (k, v) in &headers {
            req = req.set(k, v);
        }
        // HTTP 状态码错误（4xx/5xx）也归入 Err，把服务端的错误说明带给前端
        let resp = req.send_json(body).map_err(|e| match e {
            ureq::Error::Status(code, resp) => {
                let text = resp.into_string().unwrap_or_default();
                format!(
                    "模型接口返回 {}: {}",
                    code,
                    text.chars().take(400).collect::<String>()
                )
            }
            other => format!("调用模型接口失败: {}", other),
        })?;
        let text = resp
            .into_string()
            .map_err(|e| format!("读取模型响应失败: {}", e))?;
        serde_json::from_str(&text).map_err(|e| format!("模型响应不是合法 JSON: {}", e))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 检查更新：拉 version.json 并与当前版本比较（逻辑见 updater::check_update_remote）。
/// 返回”有新版 + 服务器清单”；网络/解析错误返回 Err 让前端展示，省略时不打扰启动。
/// 检查放 Rust 走网络而不是前端 fetch，避开 WebView 的 CORS 限制。
#[tauri::command]
fn check_update_server(
    url: String,
    current_version: String,
) -> Result<Option<updater::RemoteManifest>, String> {
    updater::check_update_remote(&url, &current_version)
}

/// 下载新版本的整目录文件到临时区（“发现新版本”弹窗的“下载更新”按钮触发）。
/// 下载过程在 updater::download_update_files：主 exe 恒最后下载，
/// 逐文件校验 size/sha256，完成后写 pending.json 激活”重启以更新”提示。
#[tauri::command]
async fn download_update_files(
    url: String,
    manifest: updater::RemoteManifest,
    state: State<'_, Mutex<updater::UpdateState>>,
) -> Result<updater::PendingUpdate, String> {
    // 下载是纯 IO 活，spawn_blocking 挪到线程池，避免卡住 Tauri 异步运行时
    let pending = tauri::async_runtime::spawn_blocking(move || {
        updater::download_update_files(&url, &manifest)
    })
    .await
    .map_err(|e| e.to_string())??;
    state
        .lock()
        .map_err(|e| e.to_string())?
        .pending = Some(pending.clone());
    Ok(pending)
}

/// 应用已下载的更新（红色"重启以更新"图标点击时触发）：
/// 启动更新器 → 主进程退出，更新器等主进程退出后覆盖文件并（按 restart）重启。
#[tauri::command]
fn apply_update(
    app: tauri::AppHandle,
    state: State<'_, Mutex<updater::UpdateState>>,
    restart: bool,
) -> Result<(), String> {
    let mut st = state.lock().map_err(|e| e.to_string())?;
    let pending = st
        .pending
        .clone()
        .ok_or_else(|| "没有待应用的更新".to_string())?;
    updater::spawn_applier(&pending, restart)?;
    st.dispatched = true;
    drop(st);
    // 更新器已独立运行，这里结束主进程（窗口关闭事件不会再触发第二次）
    app.exit(0);
    Ok(())
}

/// 前端查询是否有待应用的更新（启动时恢复红色"重启以更新"提示）
#[tauri::command]
fn get_pending_update(
    state: State<'_, Mutex<updater::UpdateState>>,
) -> Option<updater::PendingUpdate> {
    state.lock().ok().and_then(|g| g.pending.clone())
}

/// 在系统默认浏览器里打开外部网页（“关于”弹窗里的项目主页链接触发）。
///
/// 为什么不直接让前端 window.open：WebView 里打开外部域名会被 Tauri 拦截
/// （安全策略：WebView 只允许加载应用自己的页面）。改用系统命令调起浏览器：
/// Windows 用 `cmd /c start`，类 Unix 用 `xdg-open`——两者都是系统自带，
/// 不引入额外依赖。URL 传参用引号包裹，防止被解析成命令行参数
/// （URL 本身硬编码在前端常量里，属可信输入，双重保险）。
#[tauri::command]
fn open_external(url: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // cmd.exe 是控制台程序，本程序是 GUI（无控制台）：spawn 它时 Windows
        // 会为它新建一个控制台窗口再立即关闭，用户看到的就是“闪一下的黑窗”。
        // 用 CREATE_NO_WINDOW 创建标志让子进程不建窗口即可根治。
        // start 后的空引号是“窗口标题”占位参数（cmd 的 start 语法要求）。
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("打开链接失败: {}", e))?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .map_err(|e| format!("打开链接失败: {}", e))?;
    }
    Ok(())
}

fn main() {
    // 更新器入口：临时目录里"新版本 exe"带 --apply-update 参数启动时，
    // 不进 Tauri 主程序（无窗口、无单实例逻辑），办完“覆盖+重启”就退出。
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--apply-update") {
        let code = updater::run_apply_update(&args[2..]);
        std::process::exit(code);
    }

    let data_dir = resolve_data_dir();
    let app_config = load_app_config(&data_dir);
    let engine = Engine::new(data_dir);
    // --auto-start：进程启动后自动恢复上次运行中的网关（无需界面手动启动），
    // 供无人值守/自启动场景使用；未带参数则保持手动启动的旧行为
    let auto_start = std::env::args().any(|a| a == "--auto-start");
    let engine_auto = engine.clone();

    tauri::Builder::default()
        // 单实例：重复双击 exe（包括第一次启动窗口没显示出来、用户又开一个的
        // 情况）不会产生第二个 simuxchange.exe 进程——新进程立即退出，
        // 回调里把已有实例的主窗口显示并聚焦，用户看到的就是已有实例
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        // manage()：把对象存进 Tauri 的全局状态，command 函数通过
        // State<T> 参数按类型取用（依赖注入）；Mutex 供运行时修改配置
        .manage(Mutex::new(app_config))
        .manage(engine.clone())
        // 更新状态：pending 为已下载待应用的更新，dispatched 防止窗口关闭
        // 事件重复触发更新器（下载完成后写在磁盘，启动时恢复）
        .manage(Mutex::new(updater::UpdateState {
            pending: updater::load_pending(),
            dispatched: false,
        }))
        .setup(move |app| {
            // 本地桌面端“前端接入”：操作日志记一条，与远程模式 WebSocket
            // 接入的日志语义对齐（远程模式记真实对端 IP）
            engine.log_op("127.0.0.1", "前端接入（本地桌面端）");
            // 日志转发任务：订阅引擎事件，每收到一条就 emit 给前端
            // （前端在 backend.ts 里 listen("engine-event", ...) 接收）
            let handle = app.handle().clone();
            let mut rx = engine.subscribe();
            tauri::async_runtime::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            let _ = handle.emit("engine-event", &ev);
                        }
                        // 日志太快被丢弃时跳过继续，不影响功能
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
            // --auto-start：窗口加载前先恢复上次运行中的网关，
            // 界面打开时即可直接看到网关已运行（失败不影响界面）
            if auto_start {
                tauri::async_runtime::spawn(async move {
                    let failures = engine_auto.auto_start_previous().await;
                    for (id, e) in &failures {
                        eprintln!("自动启动网关 [{}] 失败: {}", id, e);
                    }
                });
            }
            Ok(())
        })
        // 窗口关闭（点 ✕ / Alt+F4 / 任务栏关闭）时：若已有下载完成的更新且更新器
        // 尚未启动，则先 spawn 更新器（restart=false）再放行关闭——主进程退出后
        // 更新器自动覆盖文件，用户下次双击即运行新版本；点红色"重启以更新"按钮
        // 走的是 apply_update 命令（restart=true，覆盖后自动重启）
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let st = window.state::<Mutex<updater::UpdateState>>();
                // 显式取出锁结果再判断，避免 if-let 里的临时借用拖过 st 的存活期
                let locked = st.lock();
                if let Ok(mut guard) = locked {
                    // 有待应用更新且还没交出更新器 → 交给更新器后放行关闭
                    if let Some(pending) = guard.pending.clone() {
                        if !guard.dispatched && updater::spawn_applier(&pending, false).is_ok() {
                            guard.dispatched = true;
                        }
                    }
                }
            }
        })
        // 注册前端可调用的 command 列表
        .invoke_handler(tauri::generate_handler![
            get_app_config,
            set_app_config,
            dispatch,
            check_update_server,
            download_update_files,
            apply_update,
            get_pending_update,
            llm_chat,
            open_external
        ])
        .run(tauri::generate_context!())
        .expect("SimuXchange 启动失败");
}

// 单元测试：默认忽略（要联网），手动验证 ureq + 系统 TLS 链路是否通畅：
//   cargo test --manifest-path src-tauri/Cargo.toml tls_native_smoke -- --ignored --nocapture
// 场景正是 llm_chat 依赖的 HTTPS 通道：TLS 后端没接上时 ureq 会报
// "no TLS backend is configured"（踩过的坑，见 llm_chat 注释）；
// 接上后向模型接口发一次无鉴权 GET，服务端应回 HTTP 400（缺 model 参数）。
#[cfg(test)]
mod tests {

    #[test]
    #[ignore]
    fn tls_native_smoke() {
        let tls = native_tls::TlsConnector::new().expect("初始化系统 TLS 失败");
        let agent = ureq::AgentBuilder::new()
            .tls_connector(std::sync::Arc::new(tls))
            .timeout(std::time::Duration::from_secs(30))
            .build();
        let result = agent
            .get("https://chatapi.weixin.qq.com/openai/v1/models")
            .call();
        match result {
            // 服务端拒绝缺 model 参数的请求：说明 TLS 握手与 HTTPS 请求已打通
            Err(ureq::Error::Status(code, _)) => assert_eq!(code, 400, "TLS 通了但响应异常"),
            other => panic!("TLS 链路未打通（后端没挂上或网络不通）：{:?}", other.map(|_| ())),
        }
    }
}
