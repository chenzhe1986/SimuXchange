// 冒烟测试：验证 get_orders 命令链路（不依赖真实柜台）
// 用法：node scripts/smoke-orders.mjs
const WS_URL = "ws://127.0.0.1:9800/ws";
const ws = new WebSocket(WS_URL);
let nextId = 1;
const pending = new Map();

function call(cmd, extra = {}) {
    return new Promise((resolve, reject) => {
        const id = nextId++;
        pending.set(id, { resolve, reject });
        ws.send(JSON.stringify({ id, payload: { cmd, ...extra } }));
        setTimeout(() => {
            if (pending.has(id)) {
                pending.delete(id);
                reject(new Error(`命令超时: ${cmd}`));
            }
        }, 5000);
    });
}

ws.onmessage = (e) => {
    const msg = JSON.parse(String(e.data));
    if (msg.id && pending.has(msg.id)) {
        const { resolve, reject } = pending.get(msg.id);
        pending.delete(msg.id);
        msg.resp.ok ? resolve(msg.resp.data) : reject(new Error(`${msg.resp.error}`));
    }
};

ws.onopen = async () => {
    try {
        // 1. 看当前快照里有没有网关
        const snap = await call("get_snapshot");
        console.log("快照网关数:", snap.gateways.length);
        let gw = snap.gateways[0];
        if (!gw) {
            // 2. 没有网关就新建一个（sz 分类，cacheOrders 默认开启）
            const created = await call("save_gateway", {
                gateway: {
                    id: "",
                    name: "冒烟网关",
                    category: "sz",
                    platforms: [
                        {
                            id: "",
                            name: "冒烟平台",
                            platformType: 1,
                            listenHost: "127.0.0.1",
                            port: 9401,
                            compId: "SIMX_TGW",
                            checkPassword: false,
                            password: "",
                            partitionNo: 1,
                            showPackets: false,
                            persistPackets: false,
                            cacheOrders: true,
                            strategy: {
                                mode: "ackOnly",
                                splitCountMin: 2,
                                splitCountMax: 5,
                                priceTick: 0.01,
                                customFills: [],
                                rejectVia: "executionReport",
                                rejectReason: 1,
                                rejectText: "冒烟拒单",
                                ackDelay: { minMs: 0, maxMs: 0 },
                                tradeDelay: { minMs: 0, maxMs: 0 },
                            },
                        },
                    ],
                },
            });
            console.log("新建网关:", created);
            const snap2 = await call("get_snapshot");
            gw = snap2.gateways[snap2.gateways.length - 1];
        } else {
            console.log("复用网关:", gw.config.name, "平台:", gw.config.platforms.map((p) => p.name).join(","));
        }

        // 3. 取第一个平台，先启动网关再查订单
        const pid = gw.config.platforms[0].id;
        console.log("测试平台:", pid);
        await call("start_gateway", { id: gw.config.id });
        console.log("网关已启动");
        const orders = await call("get_orders", {
            gatewayId: gw.config.id,
            platformId: pid,
        });
        console.log("get_orders 返回订单数:", Array.isArray(orders) ? orders.length : JSON.stringify(orders));
        console.log("冒烟测试通过 ✓");
    } catch (e) {
        console.error("冒烟测试失败 ✗", e.message);
        process.exitCode = 1;
    } finally {
        ws.close();
    }
};

ws.onerror = (e) => {
    console.error("WebSocket 连接失败:", e.message ?? e);
    process.exitCode = 1;
};
