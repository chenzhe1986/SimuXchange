//! 网关/平台的创建时间、修改时间、标签与备注行为测试。
//!
//! 直接操作引擎并检查快照与 gateways.json 的落盘结果，不依赖真实网络
//! （仅“运行中网关”场景需真实绑定端口），运行方式：
//! `cargo test -p simx-core --test gateway_meta`。
//!
//! 覆盖场景：
//! - 新建网关/平台：创建时间与修改时间都被赋值为当前时间
//! - 重命名网关（平台内容未变）：网关创建时间保留、修改时间刷新；平台两个时间都不变
//! - 编辑平台（内容变化）：平台创建时间保留、修改时间刷新
//! - 新增平台：新平台两个时间都取当下，旧平台修改时间不受影响
//! - 运行中热更新策略：平台与网关的修改时间一起刷新
//! - 运行中更新标签/备注：允许修改并落盘，修改时间刷新
//! - 旧配置没有时间字段（含旧版纯数组格式）：启动时回填当前时间并落盘一次

use simx_core::config::{DelayConfig, GatewayConfig, PlatformConfig, StrategyConfig};
use simx_core::engine::Engine;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

// 测试网关端口段：e2e 用 181xx/182xx/183xx，auto_start 用 184xx，
// gateway_dirs 用 185xx，本文件固定用 186xx，可安全并行。

/// 每个测试一个独立临时数据目录（串行编号避免并行冲突），结束自动清理
fn temp_data_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "simx_gateway_meta_{}_{}_{}",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_gateway(name: &str, port: u16) -> GatewayConfig {
    GatewayConfig {
        name: name.into(),
        platforms: vec![PlatformConfig {
            name: "现货集中竞价交易平台".into(),
            listen_host: "127.0.0.1".into(),
            port,
            strategy: StrategyConfig::default(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// 等 1.1 秒：时间戳精度到秒，保证后续操作落在下一秒（比较修改时间变化用）
fn tick() {
    thread::sleep(Duration::from_millis(1100));
}

/// 从引擎快照里按 id 查网关配置（找不到直接 panic）
async fn gw_of(engine: &Engine, id: &str) -> GatewayConfig {
    engine
        .snapshot()
        .await
        .gateways
        .into_iter()
        .find(|g| g.config.id == id)
        .unwrap_or_else(|| panic!("网关 {} 应存在于快照", id))
        .config
}

/// 新建网关与平台：创建时间与修改时间都被赋值为当前时间（非空且相同）
#[test]
fn new_gateway_and_platform_get_times() {
    let dir = temp_data_dir("new_times");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let gw = engine.save_gateway(test_gateway("网关A", 18601)).await.unwrap();
        assert!(!gw.created_at.is_empty(), "新建网关应带创建时间");
        assert_eq!(gw.created_at, gw.updated_at, "新建时创建/修改时间相同");
        // 格式应为 yyyy-MM-dd HH:mm:ss（长度 19，可读可排序）
        assert_eq!(gw.created_at.len(), 19, "时间格式应可读：{}", gw.created_at);
        let p = &gw.platforms[0];
        assert!(!p.created_at.is_empty(), "新建平台应带创建时间");
        assert_eq!(p.created_at, p.updated_at, "新建平台时创建/修改时间相同");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 重命名网关（平台内容未变）：
/// 网关创建时间保留、修改时间刷新；平台修改时间不受牵连
#[test]
fn rename_preserves_created_bumps_updated() {
    let dir = temp_data_dir("rename");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let gw = engine.save_gateway(test_gateway("原名", 18602)).await.unwrap();
        let p = gw.platforms[0].clone();
        tick();
        // 只改网关名，平台原样提交
        let mut new = test_gateway("新名", 18602);
        new.id = gw.id.clone();
        new.platforms[0].id = p.id.clone();
        let saved = engine.save_gateway(new).await.unwrap();

        assert_eq!(saved.created_at, gw.created_at, "重命名不应改变创建时间");
        assert!(saved.updated_at > gw.updated_at, "重命名应刷新修改时间");
        assert_eq!(saved.platforms[0].created_at, p.created_at, "平台创建时间不变");
        assert_eq!(
            saved.platforms[0].updated_at, p.updated_at,
            "平台内容没变时平台修改时间不应被牵动"
        );
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 编辑平台内容：平台创建时间保留、修改时间刷新；网关修改时间也刷新
#[test]
fn edit_platform_bumps_platform_updated() {
    let dir = temp_data_dir("edit_platform");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let gw = engine.save_gateway(test_gateway("网关A", 18603)).await.unwrap();
        let p = gw.platforms[0].clone();
        tick();
        // 改平台名，其余原样
        let mut new = gw.clone();
        new.platforms[0].name = "改过名的平台".into();
        let saved = engine.save_gateway(new).await.unwrap();

        assert_eq!(saved.platforms[0].created_at, p.created_at, "平台创建时间保留");
        assert!(
            saved.platforms[0].updated_at > p.updated_at,
            "平台内容变化应刷新平台修改时间"
        );
        assert!(saved.updated_at > gw.updated_at, "网关修改时间也应刷新");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 新增平台：新平台拿到时间戳，旧平台的修改时间不受牵连
#[test]
fn add_platform_sets_new_times_keeps_old() {
    let dir = temp_data_dir("add_platform");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let gw = engine.save_gateway(test_gateway("网关A", 18604)).await.unwrap();
        let old_p = gw.platforms[0].clone();
        tick();
        let mut new = gw.clone();
        new.platforms.push(PlatformConfig {
            name: "第二个平台".into(),
            listen_host: "127.0.0.1".into(),
            port: 18605,
            strategy: StrategyConfig::default(),
            ..Default::default()
        });
        let saved = engine.save_gateway(new).await.unwrap();

        let new_p = &saved.platforms[1];
        assert!(!new_p.created_at.is_empty(), "新平台应有创建时间");
        assert_eq!(new_p.created_at, new_p.updated_at);
        assert_eq!(
            saved.platforms[0].updated_at, old_p.updated_at,
            "旧平台未变，修改时间应保留"
        );
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 运行中热更新策略：平台与网关的修改时间一起刷新
#[test]
fn update_strategy_bumps_times() {
    let dir = temp_data_dir("hot_update");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let gw = engine.save_gateway(test_gateway("网关A", 18606)).await.unwrap();
        let p = gw.platforms[0].clone();
        engine.start_gateway(&gw.id).await.unwrap();
        tick();
        // 改一下回报延迟（非零阈值），再热更新
        let mut strategy = p.strategy.clone();
        strategy.ack_delay = DelayConfig { min_ms: 5, max_ms: 5 };
        engine
            .update_strategy(&gw.id, &p.id, strategy)
            .await
            .unwrap();

        let saved = gw_of(&engine, &gw.id).await;
        assert!(saved.platforms[0].updated_at > p.updated_at, "热更新应刷新平台修改时间");
        assert!(saved.updated_at > gw.updated_at, "热更新应刷新网关修改时间");
        engine.stop_gateway(&gw.id).await.unwrap();
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 更新标签与备注：运行中网关也允许，落盘持久化，修改时间刷新
#[test]
fn update_gateway_meta_sets_tags_notes() {
    let dir = temp_data_dir("meta");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let gw = engine.save_gateway(test_gateway("网关A", 18607)).await.unwrap();
        engine.start_gateway(&gw.id).await.unwrap();
        tick();
        // 运行中修改标签/备注（只改元数据，后端不做运行态限制）
        engine
            .update_gateway_meta(&gw.id, "生产,重点联调", "这是主力联调环境")
            .await
            .unwrap();
        let saved = gw_of(&engine, &gw.id).await;
        assert_eq!(saved.tags, "生产,重点联调");
        assert_eq!(saved.notes, "这是主力联调环境");
        assert!(saved.updated_at > gw.updated_at, "更新元数据应刷新修改时间");

        // 落盘文件里也能读到
        let content = std::fs::read_to_string(dir.join("gateways.json")).unwrap();
        assert!(content.contains("生产,重点联调"), "标签应随配置落盘");

        // 修改时间在运行中也可再次保存
        engine
            .update_gateway_meta(&gw.id, "仅测试", "")
            .await
            .unwrap();
        let saved = gw_of(&engine, &gw.id).await;
        assert_eq!(saved.tags, "仅测试");
        assert!(saved.notes.is_empty());
        engine.stop_gateway(&gw.id).await.unwrap();
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 旧配置（新格式但无时间字段）启动回填当前时间并落盘
#[test]
fn old_config_without_times_is_backfilled() {
    let dir = temp_data_dir("backfill");
    let old_json = r#"{
        "dirs": [],
        "gateways": [{
            "id": "旧网关id",
            "name": "旧版网关",
            "category": "sz",
            "platforms": [{"id": "旧平台id", "name": "旧平台", "port": 18608}],
            "wasRunning": false,
            "dirId": ""
        }]
    }"#;
    std::fs::write(dir.join("gateways.json"), old_json).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let gw = gw_of(&engine, "旧网关id").await;
        assert_eq!(gw.created_at.len(), 19, "旧网关应被回填创建时间");
        assert_eq!(gw.updated_at, gw.created_at, "回填时创建/修改时间一致");
        assert_eq!(gw.platforms[0].created_at.len(), 19, "旧平台应被回填创建时间");

        // 回填结果应已落盘（否则每次重启都在变，“创建时间”失去意义）
        let content = std::fs::read_to_string(dir.join("gateways.json")).unwrap();
        assert!(content.contains("\"createdAt\""), "回填后应落盘时间字段");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 旧版纯数组格式的配置同样兼容，并回填时间
#[test]
fn old_array_format_configs_backfilled() {
    let dir = temp_data_dir("backfill_array");
    let old_json = r#"[{
        "id": "旧网关id",
        "name": "旧版网关",
        "category": "sz",
        "platforms": [{"id": "旧平台id", "name": "旧平台", "port": 0}],
        "wasRunning": false
    }]"#;
    std::fs::write(dir.join("gateways.json"), old_json).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let snap = engine.snapshot().await;
        assert_eq!(snap.gateways.len(), 1);
        assert_eq!(snap.gateways[0].config.created_at.len(), 19);
        // 落盘文件也应升级为新格式（引擎启动时已重写）
        let content = std::fs::read_to_string(dir.join("gateways.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v.is_object(), "落盘后文件应升级为新版对象格式");
    });
    let _ = std::fs::remove_dir_all(&dir);
}