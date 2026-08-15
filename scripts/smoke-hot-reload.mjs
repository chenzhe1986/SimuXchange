// 端到端冒烟：验证“策略/回报延迟热更新（网关不停机实时生效）”+“展示收发报文默认勾选并自动持久化”
// 用法：node scripts/smoke-hot-reload.mjs [网关ID] [平台ID]
// 前置：simx-server 已启动（网关是否运行均可，脚本会自动启动）
import net from "node:net";

const WS_URL = "ws://127.0.0.1:9800/ws";
const GW_ID = process.argv[2];
const PLATFORM_ID = process.argv[3];

// ---------- WebSocket 命令通道 ----------
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
        msg.resp.ok ? resolve(msg.resp.data) : reject(new Error(msg.resp.error));
    }
};

// ---------- 深交所 Binary 报文构造（与 smoke-orders-e2e.mjs 同构） ----------
const pad = (s, n) => (s + " ".repeat(n)).slice(0, n);
const u16 = (v) => {
    const b = Buffer.alloc(2);
    b.writeUInt16BE(v);
    return b;
};
const i32 = (v) => {
    const b = Buffer.alloc(4);
    b.writeInt32BE(v);
    return b;
};
const i64 = (v) => {
    const b = Buffer.alloc(8);
    b.writeBigInt64BE(BigInt(v));
    return b;
};
const checksum = (buf) => buf.reduce((a, b) => (a + b) % 256, 0);
const frame = (mt, body) => {
    const head = Buffer.concat([i32(mt), i32(body.length)]);
    const cks = Buffer.alloc(4);
    cks.writeUInt32BE(checksum(Buffer.concat([head, body])));
    return Buffer.concat([head, body, cks]);
};

/** 现券委托体（NewOrderCash 字段顺序见 protocol.rs decode） */
function newOrderBody(clOrdId, qtyRaw, priceRaw) {
    return Buffer.concat([
        Buffer.from(pad("010", 3)),      // appl_id
        Buffer.from(pad("PBU001", 6)),   // submitting_pbu_id
        Buffer.from(pad("000001", 8)),   // security_id
        Buffer.from(pad("102", 4)),      // security_id_source
        u16(0),                          // owner_type
        Buffer.from(pad("", 2)),         // clearing_firm
        i64(20260814093000000),          // transact_time
        Buffer.from(pad("", 8)),         // user_info
        Buffer.from(pad(clOrdId, 10)),   // cl_ord_id
        Buffer.from(pad("B880000001", 12)), // account_id
        Buffer.from(pad("0001", 4)),     // branch_id
        Buffer.from(pad("", 4)),         // order_restrictions
        Buffer.from("1"),                // side 买
        Buffer.from("2"),                // ord_type 限价
        i64(qtyRaw),                     // order_qty
        i64(priceRaw),                   // price
    ]);
}

/** Logon 报文体 */
function logonBody() {
    return Buffer.concat([
        Buffer.from(pad("OMS0001", 20)),
        Buffer.from(pad("SIMX_TGW", 20)),
        i32(30),
        Buffer.from(pad("", 16)),
        Buffer.from(pad("", 32)),
    ]);
}

// ---------- TCP 会话 ----------
function connectTcp(port) {
    return new Promise((resolve, reject) => {
        const s = net.connect(port, "127.0.0.1", () => resolve(s));
        s.on("error", reject);
    });
}

/** 读取一条完整报文：返回 { mt, body }（跳过会话消息由调用方循环处理） */
function readFrame(sock) {
    return new Promise((resolve, reject) => {
        let buf = Buffer.alloc(0);
        const onData = (chunk) => {
            buf = Buffer.concat([buf, chunk]);
            if (buf.length < 8) return;
            const mt = buf.readInt32BE(0);
            const len = buf.readInt32BE(4);
            if (buf.length < 8 + len + 4) return;
            sock.removeListener("data", onData);
            if (buf.length > 8 + len + 4) sock.unshift(buf.subarray(8 + len + 4));
            resolve({ mt, body: buf.subarray(8, 8 + len) });
        };
        sock.on("data", onData);
        sock.on("error", reject);
    });
}

// 确认/拒单回报（200102）中 exec_type 的偏移：partition_no..exec_id 各字段长度之和
const EXEC_TYPE_OFF = 4 + 8 + 3 + 6 + 6 + 8 + 4 + 2 + 2 + 8 + 8 + 16 + 10 + 10 + 16;

const waitMs = (ms) => new Promise((r) => setTimeout(r, ms));

/** 登录并排空会话消息，返回就绪的 socket */
async function login(port) {
    const sock = await connectTcp(port);
    sock.on("error", () => {});
    sock.write(frame(1, logonBody()));
    let f = await readFrame(sock);
    while (f.mt !== 1) f = await readFrame(sock); // 跳过平台信息/状态
    return sock;
}

/** 发一笔委托并收集其后到达的回报（最多等 3 条，间隔 150ms 拉一次） */
async function sendOrderAndCollect(sock, cl, expectCount) {
    sock.write(frame(100101, newOrderBody(cl, 10000, 100100)));
    const got = [];
    for (let i = 0; i < expectCount; i++) {
        got.push(await readFrame(sock));
    }
    return got;
}

async function main() {
    await new Promise((resolve, reject) => {
        ws.onopen = resolve;
        ws.onerror = reject;
    });

    // 1. 找到/创建测试平台
    let gwId = GW_ID, pid = PLATFORM_ID;
    let snap = await call("get_snapshot");
    let gw = gwId ? snap.gateways.find((g) => g.config.id === gwId) : snap.gateways.find((g) => g.config.platforms.length > 0);
    if (!gw) {
        const created = await call("save_gateway", {
            gateway: {
                id: "",
                name: "热更新冒烟网关",
                category: "sz",
                platforms: [
                    {
                        id: "",
                        name: "热更新冒烟平台",
                        platformType: 1,
                        listenHost: "127.0.0.1",
                        port: 9421,
                        compId: "SIMX_TGW",
                        checkPassword: false,
                        password: "",
                        partitionNo: 1,
                        showPackets: true,
                        cacheOrders: true,
                        strategy: {
                            mode: "fullSingle",
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
        console.log("新建网关:", created.name);
        snap = await call("get_snapshot");
        gw = snap.gateways.find((g) => g.config.id === created.id);
    }
    gwId = gw.config.id;
    const plat = gw.config.platforms.find((p) => (pid ? p.id === pid : true));
    pid = plat.id;
    const port = plat.port;

    // 2. 验证“展示收发报文默认勾选且自动持久化”（需求 2：保存后持久化随展示联动）
    console.log(`测试平台: ${plat.name} (端口 ${port})`);
    const saved = await call("save_gateway", { gateway: gw.config });
    const savedPlat = saved.platforms.find((p) => p.id === pid);
    console.log(`保存后 showPackets=${savedPlat.showPackets} persistPackets=${savedPlat.persistPackets}`);
    if (savedPlat.persistPackets !== savedPlat.showPackets) {
        throw new Error("持久化未随展示开关联动（应一致）");
    }

    // 3. 启动网关（若未运行）
    const before = await call("get_snapshot");
    if (!before.gateways.find((g) => g.config.id === gwId).running) {
        await call("start_gateway", { id: gwId });
        console.log("网关已启动");
        await waitMs(300);
    } else {
        console.log("网关已在运行（直接热更新，不重启）");
    }

    // 4. TCP 登录
    console.log("TCP 登录中...");
    const sock = await login(port);
    console.log("登录成功");

    // 5. 热更新策略为“拒单”（网关不停机），发委托应收到拒单回报
    await call("update_strategy", {
        gatewayId: gwId,
        platformId: pid,
        strategy: {
            mode: "reject",
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
    });
    console.log("热更新策略 → reject");
    const CL1 = "R" + String(Date.now()).slice(-9);
    let reps = await sendOrderAndCollect(sock, CL1, 1);
    let f = reps[0];
    const execType = String.fromCharCode(f.body.readUInt8(EXEC_TYPE_OFF));
    console.log(`委托 ${CL1} 回报 MsgType=${f.mt} ExecType=${execType}（应=200102/8 拒单）`);
    if (f.mt !== 200102 || execType !== "8") throw new Error("热更新为拒单后未收到拒单回报");

    // 6. 再热更新为“全部成交（单笔）”，发委托应收到 确认 + 成交（证明实时切换）
    await call("update_strategy", {
        gatewayId: gwId,
        platformId: pid,
        strategy: {
            mode: "fullSingle",
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
    });
    console.log("热更新策略 → fullSingle");
    const CL2 = "T" + String(Date.now()).slice(-9);
    reps = await sendOrderAndCollect(sock, CL2, 2);
    const mt1 = reps[0].mt, mt2 = reps[1].mt;
    console.log(`委托 ${CL2} 回报 MsgType=${mt1}, ${mt2}（应=200102 确认 + 200115 成交）`);
    if (mt1 !== 200102 || mt2 !== 200115) throw new Error("热更新为全部成交后未收到确认+成交");

    // 7. 热更新回报延迟（确认延迟固定 300ms），发委托验证确认延迟生效
    const t0 = Date.now();
    await call("update_strategy", {
        gatewayId: gwId,
        platformId: pid,
        strategy: {
            mode: "ackOnly",
            splitCountMin: 2,
            splitCountMax: 5,
            priceTick: 0.01,
            customFills: [],
            rejectVia: "executionReport",
            rejectReason: 1,
            rejectText: "冒烟拒单",
            ackDelay: { minMs: 300, maxMs: 300 },
            tradeDelay: { minMs: 0, maxMs: 0 },
        },
    });
    console.log("热更新策略 → ackOnly + 确认延迟 300ms");
    const CL3 = "D" + String(Date.now()).slice(-9);
    sock.write(frame(100101, newOrderBody(CL3, 10000, 100100)));
    f = await readFrame(sock);
    const elapsed = Date.now() - t0;
    console.log(`委托 ${CL3} 确认回报 ${elapsed}ms 后到达（应≥300ms，延迟实时生效）`);
    if (elapsed < 250) throw new Error("确认延迟未生效（300ms 延迟实际不足 250ms）");

    // 8. 验证持久化配置已同步（重启后仍生效）
    const after = await call("get_snapshot");
    const afterPlat = after.gateways.find((g) => g.config.id === gwId).config.platforms.find((p) => p.id === pid);
    console.log(`持久化配置策略 mode=${afterPlat.strategy.mode} 确认延迟=${afterPlat.strategy.ackDelay.minMs}ms（应=ackOnly/300）`);
    if (afterPlat.strategy.mode !== "ackOnly" || afterPlat.strategy.ackDelay.minMs !== 300) {
        throw new Error("热更新未同步到持久化配置");
    }

    sock.destroy();
    console.log("热更新冒烟测试通过 ✓");
}

main()
    .catch((e) => {
        console.error("热更新冒烟失败 ✗", e.message);
        process.exitCode = 1;
    })
    .finally(() => {
        ws.close();
        process.exit();
    });
