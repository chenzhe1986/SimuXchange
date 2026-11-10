//! 上海综合网关配置校验的集成测试（不需要真实数据库）。
//!
//! 覆盖：category 序列化、缺少 db / 库名为空时保存失败、合法配置保存成功并出现在快照里。

use simx_core::config::{
    DbConfig, GatewayCategory, GatewayConfig, PlatformConfig, StrategyConfig,
};
use simx_core::engine::Engine;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "simx_shzh_cfg_{}_{}_{}",
        tag,
        std::process::id(),
        chrono::Local::now().format("%H%M%S%f")
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("建临时目录");
    p
}

fn platform(db: Option<DbConfig>) -> PlatformConfig {
    PlatformConfig {
        id: "p1".into(),
        name: "上海综合业务平台".into(),
        platform_type: 2,
        listen_host: String::new(),
        port: 0,
        db,
        comp_id: String::new(),
        check_password: false,
        password: String::new(),
        partition_no: 0,
        partition_nos: String::new(),
        show_packets: false,
        persist_packets: false,
        cache_orders: true,
        strategy: StrategyConfig::default(),
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn gateway(platform: PlatformConfig) -> GatewayConfig {
    GatewayConfig {
        id: "g1".into(),
        name: "综合测试网关".into(),
        category: GatewayCategory::Shzh,
        platforms: vec![platform],
        ..Default::default()
    }
}

fn valid_db() -> DbConfig {
    DbConfig {
        database: "ezstep".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn shzh_requires_db_config() {
    let dir = temp_dir("missing");
    let engine = Engine::new(dir.clone());
    let err = engine
        .save_gateway(gateway(platform(None)))
        .await
        .expect_err("缺少 db 必须保存失败");
    assert!(err.contains("缺少数据库配置"), "错误信息应说明缺失：{}", err);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn shzh_rejects_empty_database_name() {
    let dir = temp_dir("emptydb");
    let engine = Engine::new(dir.clone());
    let mut db = valid_db();
    db.database = "  ".into();
    let err = engine
        .save_gateway(gateway(platform(Some(db))))
        .await
        .expect_err("库名为空必须保存失败");
    assert!(err.contains("数据库名不能为空"), "错误信息：{}", err);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn shzh_valid_config_saves_and_roundtrips() {
    let dir = temp_dir("ok");
    let engine = Engine::new(dir.clone());
    let saved = engine
        .save_gateway(gateway(platform(Some(valid_db()))))
        .await
        .expect("合法配置应保存成功");
    assert_eq!(saved.category, GatewayCategory::Shzh);
    assert_eq!(saved.platforms[0].db.as_ref().unwrap().database, "ezstep");

    let snap = engine.snapshot().await;
    let gw = snap
        .gateways
        .iter()
        .find(|g| g.config.id == "g1")
        .expect("快照里应有网关");
    assert_eq!(gw.config.category, GatewayCategory::Shzh);
    assert!(gw.config.platforms[0].db.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
