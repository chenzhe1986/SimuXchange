//! 网关目录（文件夹分组）行为测试。
//!
//! 直接操作引擎并检查 gateways.json 的落盘结果，不依赖真实网络
//! （除“运行中网关”场景需真实绑定端口外），运行方式：
//! `cargo test -p simx-core --test gateway_dirs`。
//!
//! 覆盖场景：
//! - 目录新建/重命名/同父级重名拒绝
//! - 目录最多两级的嵌套限制（子目录内不能再建目录）
//! - 移动网关到目录/移回根层级（含运行中网关允许移动）
//! - 删除目录勾选“同时删除网关”：任一网关运行中则整体失败；全停止才删
//! - 删除目录未勾选：目录下网关（含子目录里的）自动移回根层级
//! - 旧版纯数组格式的 gateways.json 兼容加载，保存后升级为新版对象格式

use simx_core::config::{DirConfig, GatewayConfig, PlatformConfig, StrategyConfig};
use simx_core::engine::Engine;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

// 测试网关端口段：e2e 测试用 181xx/182xx/183xx，auto_start 用 184xx，
// 本文件固定用 185xx，且各测试端口互不相同，可安全并行。

/// 每个测试一个独立临时数据目录（串行编号避免并行冲突），结束自动清理
fn temp_data_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "simx_gateway_dirs_{}_{}_{}",
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

/// 从快照里按名称找目录（测试辅助）
async fn find_dir(engine: &Engine, name: &str) -> Option<DirConfig> {
    engine.snapshot().await.dirs.into_iter().find(|d| d.name == name)
}

/// 目录新建/重命名/重名/两层层级限制
#[test]
fn dir_crud_and_nesting_limit() {
    let dir = temp_data_dir("crud");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());

        // 新建根级目录：id 为空由后端生成，返回的 DirConfig 应带 id
        let a = engine
            .save_dir(DirConfig {
                name: "研发环境".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!a.id.is_empty(), "新建目录应自动生成 id");
        assert_eq!(find_dir(&engine, "研发环境").await.unwrap().id, a.id);

        // 同父级（根层级）重名应拒绝
        let dup = engine
            .save_dir(DirConfig {
                name: "研发环境".into(),
                ..Default::default()
            })
            .await;
        assert!(dup.is_err(), "同父级下不应允许重名目录");

        // 不同父级下允许同名：根级目录下建同名子目录
        let sub = engine
            .save_dir(DirConfig {
                name: "研发环境".into(),
                parent_id: a.id.clone(),
                ..Default::default()
            })
            .await
            .unwrap();

        // 根级目录下可以再建子目录（第二层）
        let sub2 = engine
            .save_dir(DirConfig {
                name: "生产环境".into(),
                parent_id: a.id.clone(),
                ..Default::default()
            })
            .await
            .unwrap();

        // 第三层应被拒绝：子目录内不能再建目录
        let three = engine
            .save_dir(DirConfig {
                name: "超深层级".into(),
                parent_id: sub.id.clone(),
                ..Default::default()
            })
            .await;
        assert!(three.is_err(), "子目录内不应允许再建目录（两层层级上限）");

        // 父目录不存在应拒绝
        let orphan = engine
            .save_dir(DirConfig {
                name: "孤儿目录".into(),
                parent_id: "不存在的id".into(),
                ..Default::default()
            })
            .await;
        assert!(orphan.is_err(), "父目录不存在时新建子目录应失败");

        // 空名字应拒绝
        let blank = engine
            .save_dir(DirConfig {
                name: "   ".into(),
                ..Default::default()
            })
            .await;
        assert!(blank.is_err(), "目录名不能为空");

        // 重命名：带上已有 id 改名，parent_id 保持不变
        let renamed = engine
            .save_dir(DirConfig {
                id: sub2.id.clone(),
                name: "生产环境-改".into(),
                parent_id: sub2.parent_id.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(renamed.parent_id, a.id, "重命名不应改变目录层级");
        let snap = engine.snapshot().await;
        assert!(snap.dirs.iter().any(|d| d.name == "生产环境-改"));
        assert!(!snap.dirs.iter().any(|d| d.name == "生产环境"));
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 移动网关到目录/移回根层级；运行中的网关也允许移动
#[test]
fn move_gateway_between_dirs() {
    let dir = temp_data_dir("move");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let root = engine
            .save_dir(DirConfig {
                name: "分组一".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let sub = engine
            .save_dir(DirConfig {
                name: "子分组".into(),
                parent_id: root.id.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        let gw1 = engine.save_gateway(test_gateway("网关甲", 18501)).await.unwrap();
        let gw2 = engine.save_gateway(test_gateway("网关乙", 18502)).await.unwrap();

        // 移动到根级目录 / 子目录，快照里的 dir_id 应跟着变
        engine.move_gateway(&gw1.id, &root.id).await.unwrap();
        engine.move_gateway(&gw2.id, &sub.id).await.unwrap();
        let snap = engine.snapshot().await;
        let d1 = snap
            .gateways
            .iter()
            .find(|g| g.config.id == gw1.id)
            .unwrap();
        let d2 = snap
            .gateways
            .iter()
            .find(|g| g.config.id == gw2.id)
            .unwrap();
        assert_eq!(d1.config.dir_id, root.id, "网关甲应归入根级目录");
        assert_eq!(d2.config.dir_id, sub.id, "网关乙应归入子目录");

        // 目标目录不存在应拒绝
        assert!(engine.move_gateway(&gw1.id, "不存在的id").await.is_err());

        // 运行中的网关也允许移动（只改归属分类，不触碰运行时）
        engine.start_gateway(&gw1.id).await.unwrap();
        engine.move_gateway(&gw1.id, &sub.id).await.unwrap();
        let snap = engine.snapshot().await;
        assert!(
            snap.gateways.iter().find(|g| g.config.id == gw1.id).unwrap().running,
            "移动归属不应影响网关运行状态"
        );
        engine.stop_gateway(&gw1.id).await.unwrap();

        // 移回根层级：dir_id 传空串
        engine.move_gateway(&gw2.id, "").await.unwrap();
        let snap = engine.snapshot().await;
        assert_eq!(
            snap.gateways
                .iter()
                .find(|g| g.config.id == gw2.id)
                .unwrap()
                .config
                .dir_id,
            "",
            "移回根层级后 dir_id 应为空"
        );
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 删除目录勾选“同时删除网关”：任一网关运行中则整体失败；全停止才删
#[test]
fn delete_dir_with_running_gateway_fails() {
    let dir = temp_data_dir("del_running");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let root = engine
            .save_dir(DirConfig {
                name: "待删目录".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let sub = engine
            .save_dir(DirConfig {
                name: "待删子目录".into(),
                parent_id: root.id.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        let gw1 = engine.save_gateway(test_gateway("停止网关", 18503)).await.unwrap();
        let gw2 = engine.save_gateway(test_gateway("运行网关", 18504)).await.unwrap();
        engine.move_gateway(&gw1.id, &root.id).await.unwrap();
        engine.move_gateway(&gw2.id, &sub.id).await.unwrap();
        engine.start_gateway(&gw2.id).await.unwrap();

        // 勾选“同时删除网关”但子目录里有运行中网关：整体失败
        let err = engine.delete_dir(&root.id, true).await.unwrap_err();
        assert!(
            err.contains("运行网关") && err.contains("正在运行"),
            "错误提示应指明运行中的网关：{}",
            err
        );
        // 失败后不能留半删状态：目录、网关都应原样保留
        let snap = engine.snapshot().await;
        assert_eq!(snap.dirs.len(), 2, "删除失败不应删掉任何目录");
        assert_eq!(snap.gateways.len(), 2, "删除失败不应删掉任何网关");

        // 停止运行中的网关后再删：目录、子目录、网关全部删除
        engine.stop_gateway(&gw2.id).await.unwrap();
        engine.delete_dir(&root.id, true).await.unwrap();
        let snap = engine.snapshot().await;
        assert!(snap.dirs.is_empty(), "删除目录应连同子目录一起删除");
        assert!(snap.gateways.is_empty(), "勾选后应连同目录下网关一起删除");

        // 落盘文件里也不应有残留
        let content = std::fs::read_to_string(dir.join("gateways.json")).unwrap();
        assert!(!content.contains("待删目录"), "落盘文件不应残留目录");
        assert!(!content.contains("运行网关"), "落盘文件不应残留网关");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 删除目录未勾选“同时删除网关”：目录下网关（含子目录里的）自动移回根层级
#[test]
fn delete_dir_without_delete_moves_gateways_to_root() {
    let dir = temp_data_dir("del_keep");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let root = engine
            .save_dir(DirConfig {
                name: "分组".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let sub = engine
            .save_dir(DirConfig {
                name: "子分组".into(),
                parent_id: root.id.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        let gw1 = engine.save_gateway(test_gateway("网关甲", 18505)).await.unwrap();
        let gw2 = engine.save_gateway(test_gateway("网关乙", 18506)).await.unwrap();
        engine.move_gateway(&gw1.id, &root.id).await.unwrap();
        engine.move_gateway(&gw2.id, &sub.id).await.unwrap();

        // 未勾选“同时删除网关”：目录和子目录删掉，网关全部保留并移回根层级
        engine.delete_dir(&root.id, false).await.unwrap();
        let snap = engine.snapshot().await;
        assert!(snap.dirs.is_empty(), "目录应被删除");
        assert_eq!(snap.gateways.len(), 2, "未勾选时网关应保留");
        for g in &snap.gateways {
            assert_eq!(g.config.dir_id, "", "保留下来的网关应移回根层级");
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// 旧版纯数组格式的 gateways.json 兼容加载；落盘后自动升级为新版对象格式
#[test]
fn old_array_format_gateways_json_still_loads() {
    let dir = temp_data_dir("old_format");
    // 手工写入旧版格式（纯网关数组，无目录概念，也没有 dir_id 字段）
    let old_json = format!(
        r#"[{{
            "id": "旧网关id",
            "name": "旧版网关",
            "category": "sz",
            "platforms": [],
            "wasRunning": false
        }}]"#
    );
    std::fs::write(dir.join("gateways.json"), old_json).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let engine = Engine::new(dir.clone());
        let snap = engine.snapshot().await;
        assert!(snap.dirs.is_empty(), "旧格式文件没有目录");
        assert_eq!(snap.gateways.len(), 1, "旧格式网关应正常加载");
        assert_eq!(snap.gateways[0].config.name, "旧版网关");
        assert_eq!(snap.gateways[0].config.dir_id, "", "旧格式无 dir_id 字段应按根层级处理");

        // 之后任何落盘操作（如新建目录）都会把文件升级为新版对象格式，
        // 且旧网关数据原样保留
        engine
            .save_dir(DirConfig {
                name: "新目录".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let content = std::fs::read_to_string(dir.join("gateways.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v.is_object(), "落盘后文件应升级为新版对象格式");
        assert_eq!(v["dirs"].as_array().unwrap().len(), 1);
        assert_eq!(v["gateways"].as_array().unwrap().len(), 1);
    });
    let _ = std::fs::remove_dir_all(&dir);
}
