//! --auto-start 行为测试：启动时只恢复“上次运行中”的网关。
//!
//! 直接操作引擎并检查 gateways.json 里 was_running 字段的落盘结果，
//! 不依赖真实网络，因此比 e2e 测试轻量。运行方式：`cargo test -p simx-core --test auto_start`。
//!
//! 覆盖场景：
//! - 带 --auto-start：只启动 was_running=true 的网关，其余保持停止
//! - 停止网关会同步清除标记，重启后不再恢复
//! - 不带 --auto-start：不调用自动恢复，所有网关都不启动
//! - 删除网关后标记随配置消失；配置损坏时按空处理
//!
//! “进程重启”用独立 tokio runtime 模拟：第一个 runtime 的 block_on 返回后
//! 直接 drop，其所有监听任务随之取消、端口释放，等价于进程退出。

use simx_core::config::{GatewayConfig, PlatformConfig, StrategyConfig};
use simx_core::engine::Engine;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

// 测试网关端口段：e2e 测试已用 18101-18103/18201-18204/18301-18304，
// 本文件固定用 184xx，且各测试端口互不相同，可安全并行。

/// 每个测试一个独立临时数据目录（串行编号避免并行冲突），结束自动清理
fn temp_data_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "simx_auto_start_{}_{}_{}",
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

/// 读取 gateways.json 中指定网关的 was_running 字段（文件不存在视为 false）
fn was_running(dir: &PathBuf, id: &str) -> bool {
    let path = dir.join("gateways.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<Vec<GatewayConfig>>(&s)
            .unwrap_or_default()
            .iter()
            .find(|g| g.id == id)
            .map(|g| g.was_running)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// 快照中网关是否在运行
async fn is_running(engine: &Engine, id: &str) -> bool {
    engine
        .snapshot()
        .await
        .gateways
        .iter()
        .find(|g| g.config.id == id)
        .map(|g| g.running)
        .unwrap_or(false)
}

/// 带 --auto-start 重启：只启动上次运行中的网关，其余保持停止
#[test]
fn auto_start_restores_only_last_running_gateways() {
    let dir = temp_data_dir("restore");
    // 第一阶段：模拟“上次运行期”。runtime drop 后监听任务全部取消，
    // 等价于进程退出，端口随之释放。
    let (gw1_id, gw2_id) = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let engine = Engine::new(dir.clone());
            let gw1 = engine
                .save_gateway(test_gateway("网关A", 18401))
                .await
                .unwrap();
            let gw2 = engine
                .save_gateway(test_gateway("网关B", 18402))
                .await
                .unwrap();
            // 上次运行期：A 运行、B 停止（停止会清除运行标记）
            engine.start_gateway(&gw1.id).await.unwrap();
            engine.start_gateway(&gw2.id).await.unwrap();
            engine.stop_gateway(&gw2.id).await.unwrap();
            assert!(was_running(&dir, &gw1.id), "启动的 A 应标记为上次运行中");
            assert!(!was_running(&dir, &gw2.id), "停止的 B 不应带运行标记");
            (gw1.id, gw2.id)
        })
    };

    // 第二阶段：模拟带 --auto-start 重启进程
    let rt2 = tokio::runtime::Runtime::new().unwrap();
    rt2.block_on(async {
        let engine2 = Engine::new(dir.clone());
        let failures = engine2.auto_start_previous().await;
        assert!(failures.is_empty(), "自动启动失败: {:?}", failures);
        assert!(is_running(&engine2, &gw1_id).await, "上次运行中的 A 应被恢复");
        assert!(
            !is_running(&engine2, &gw2_id).await,
            "上次已停止的 B 不应被恢复"
        );
    });

    let _ = std::fs::remove_dir_all(&dir);
}

/// 不带 --auto-start：引擎只加载配置，所有网关都不自动启动
#[test]
fn no_auto_start_leaves_all_gateways_stopped() {
    let dir = temp_data_dir("manual");
    // 第一阶段：模拟上次运行期（进程异常退出，运行标记保留在配置里）
    let gw1_id = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let engine = Engine::new(dir.clone());
            let gw1 = engine
                .save_gateway(test_gateway("网关A", 18403))
                .await
                .unwrap();
            engine.start_gateway(&gw1.id).await.unwrap();
            assert!(was_running(&dir, &gw1.id), "启动的 A 应带运行标记");
            gw1.id
        })
    };

    // 第二阶段：重启后不调用 auto_start_previous（等价于不带 --auto-start 参数）
    let rt2 = tokio::runtime::Runtime::new().unwrap();
    rt2.block_on(async {
        let engine2 = Engine::new(dir.clone());
        let gws = engine2.snapshot().await.gateways;
        assert!(
            gws.iter().all(|g| !g.running),
            "不带 --auto-start 时网关不应自动启动"
        );
        // 手动启动不受影响（端口此时应空闲可绑定）
        engine2.start_gateway(&gw1_id).await.unwrap();
    });

    let _ = std::fs::remove_dir_all(&dir);
}

/// 删除网关后运行标记随配置一起消失，自动恢复不会启动任何残留网关
#[test]
fn deleted_gateway_mark_is_cleaned_with_config() {
    let dir = temp_data_dir("deleted");
    let gw1_id = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let engine = Engine::new(dir.clone());
            let gw1 = engine
                .save_gateway(test_gateway("网关A", 18404))
                .await
                .unwrap();
            engine.start_gateway(&gw1.id).await.unwrap();
            assert!(was_running(&dir, &gw1.id));
            engine.stop_gateway(&gw1.id).await.unwrap();
            assert!(!was_running(&dir, &gw1.id), "停止后运行标记应清除");
            gw1.id
        })
    };

    let rt2 = tokio::runtime::Runtime::new().unwrap();
    rt2.block_on(async {
        let engine2 = Engine::new(dir.clone());
        // 删除网关（未运行）：配置没了，运行标记也随配置消失
        engine2.delete_gateway(&gw1_id).await.unwrap();
        assert!(!was_running(&dir, &gw1_id), "删除后网关不应残留任何标记");

        // 自动恢复不启动任何网关，也不报错
        let failures = engine2.auto_start_previous().await;
        assert!(failures.is_empty(), "已删除网关不应出现在失败列表");
        assert!(engine2.snapshot().await.gateways.is_empty());
    });

    let _ = std::fs::remove_dir_all(&dir);
}

/// gateways.json 不存在或损坏时按空处理：不自动启动任何网关
#[test]
fn missing_or_corrupt_config_means_nothing_to_start() {
    let dir = temp_data_dir("corrupt");
    let gw1_id = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let engine = Engine::new(dir.clone());
            let gw1 = engine
                .save_gateway(test_gateway("网关A", 18405))
                .await
                .unwrap();
            engine.start_gateway(&gw1.id).await.unwrap();
            assert!(was_running(&dir, &gw1.id));
            gw1.id
        })
    };

    // 配置文件写坏（首启场景等价于不存在）
    std::fs::write(dir.join("gateways.json"), "not-json").unwrap();
    let rt2 = tokio::runtime::Runtime::new().unwrap();
    rt2.block_on(async {
        let engine2 = Engine::new(dir.clone());
        let failures = engine2.auto_start_previous().await;
        assert!(failures.is_empty());
        assert!(!is_running(&engine2, &gw1_id).await);

        // 损坏场景下引擎按“无配置”处理；重新保存网关后可正常启动，
        // 配置被重写为合法 JSON 并重新打上运行标记
        let gw = engine2
            .save_gateway(test_gateway("网关A", 18405))
            .await
            .unwrap();
        engine2.start_gateway(&gw.id).await.unwrap();
        assert!(was_running(&dir, &gw.id), "重新启动后应重新落盘运行标记");
    });

    let _ = std::fs::remove_dir_all(&dir);
}
