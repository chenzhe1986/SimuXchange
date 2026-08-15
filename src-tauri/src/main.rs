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
use tauri::{Emitter, State};

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
/// command，payload 为 JSON 命令，具体格式见 simx_core::api
#[tauri::command]
async fn dispatch(
    engine: State<'_, Engine>,
    payload: serde_json::Value,
) -> Result<serde_json::Value, String> {
    Ok(simx_core::api::dispatch(engine.inner(), payload).await)
}

/// 下载新版安装包并启动安装（“发现新版本”弹窗里的“下载并安装”按钮触发）。
///
/// 下载到用户“下载”目录（文件名取 URL 最后一段），完成后直接启动安装程序
/// （NSIS 安装包，会弹出安装向导）。下载是纯 IO 活，用 spawn_blocking 挪到
/// 独立线程执行，避免卡住 Tauri 的异步运行时。
#[tauri::command]
async fn download_update(url: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // 从 URL 末尾提取文件名（没有则给个默认名，避免路径拼接问题）
        let file_name = url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("simuxchange-setup.exe");
        // 下载目录：优先“用户/Downloads”，取不到（非常规环境）就退回临时目录
        let downloads = std::env::var("USERPROFILE")
            .map(|p| PathBuf::from(p).join("Downloads"))
            .unwrap_or_else(|_| std::env::temp_dir());
        let dest = downloads.join(file_name);

        let resp = ureq::get(&url)
            .call()
            .map_err(|e| format!("连接更新服务器失败: {}", e))?;
        let mut reader = resp.into_reader();
        let mut file =
            std::fs::File::create(&dest).map_err(|e| format!("创建文件失败: {}", e))?;
        std::io::copy(&mut reader, &mut file).map_err(|e| format!("写入文件失败: {}", e))?;

        // 启动安装程序（exe 直接执行即可，Windows 会弹出 NSIS 安装向导）
        std::process::Command::new(&dest)
            .spawn()
            .map_err(|e| format!("启动安装程序失败: {}", e))?;
        Ok(format!("安装包已下载到 {} 并启动安装", dest.display()))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn main() {
    let data_dir = resolve_data_dir();
    let app_config = load_app_config(&data_dir);
    let engine = Engine::new(data_dir);
    // --auto-start：进程启动后自动恢复上次运行中的网关（无需界面手动启动），
    // 供无人值守/自启动场景使用；未带参数则保持手动启动的旧行为
    let auto_start = std::env::args().any(|a| a == "--auto-start");
    let engine_auto = engine.clone();

    tauri::Builder::default()
        // manage()：把对象存进 Tauri 的全局状态，command 函数通过
        // State<T> 参数按类型取用（依赖注入）；Mutex 供运行时修改配置
        .manage(Mutex::new(app_config))
        .manage(engine.clone())
        .setup(move |app| {
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
        // 注册前端可调用的 command 列表
        .invoke_handler(tauri::generate_handler![
            get_app_config,
            set_app_config,
            dispatch,
            download_update
        ])
        .run(tauri::generate_context!())
        .expect("SimuXchange 启动失败");
}
