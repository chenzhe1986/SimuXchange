// 端到端冒烟：验证“订单缓存 + 按状态撤单”全链路（真实 TCP 委托）
// 用法：node scripts/smoke-orders-e2e.mjs [网关ID] [平台ID]
// 前置：simx-server 已启动且目标网关已启动（smoke-orders.mjs 可自动建网关）
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

// ---------- 深交所 Binary 报文构造 ----------
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

/** 撤单请求体（OrderCancelRequest 字段顺序见 protocol.rs decode） */
function cancelBody(clOrdId, origClOrdId) {
    return Buffer.concat([
        Buffer.from(pad("010", 3)),
        Buffer.from(pad("PBU001", 6)),
        Buffer.from(pad("000001", 8)),
        Buffer.from(pad("102", 4)),
        u16(0),
        Buffer.from(pad("", 2)),
        i64(20260814093010000),
        Buffer.from(pad("", 8)),
        Buffer.from(pad(clOrdId, 10)),
        Buffer.from(pad(origClOrdId, 10)),
        Buffer.from("1"),
        Buffer.from(pad("", 16)),
        i64(0),
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

/** 读取一条完整报文：返回 { mt, body }；跳过心跳等会话消息。
 *  累积缓冲直到凑齐一条完整帧（8 字节头 + body + 4 校验和） */
function readFrame(sock) {
    return new Promise((resolve, reject) => {
        let buf = Buffer.alloc(0);
        const onData = (chunk) => {
            buf = Buffer.concat([buf, chunk]);
            if (buf.length < 8) return;
            const mt = buf.readInt32BE(0);
            const len = buf.readInt32BE(4);
            if (buf.length < 8 + len + 4) return; // 不完整，继续等
            sock.removeListener("data", onData);
            if (buf.length > 8 + len + 4) sock.unshift(buf.subarray(8 + len + 4));
            resolve({ mt, body: buf.subarray(8, 8 + len) });
        };
        sock.on("data", onData);
        sock.on("error", reject);
    });
}

// ---------- 主流程 ----------
async function waitMs(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

async function main() {
    await new Promise((resolve, reject) => {
        ws.onopen = resolve;
        ws.onerror = reject;
    });

    // 1. 找到/创建测试平台（优先复用传入的）
    let gwId = GW_ID, pid = PLATFORM_ID;
    if (!gwId || !pid) {
        const snap = await call("get_snapshot");
        const gw = snap.gateways.find((g) => g.config.platforms.length > 0);
        if (!gw) throw new Error("没有可用网关，请先手动创建一个");
        gwId = gw.config.id;
        pid = gw.config.platforms[0].id;
    }
    const snap = await call("get_snapshot");
    const gw = snap.gateways.find((g) => g.config.id === gwId);
    const plat = gw.config.platforms.find((p) => p.id === pid);
    const port = plat.port;
    const cacheOn = plat.cacheOrders;
    console.log(`测试平台: ${plat.name} (端口 ${port}, cacheOrders=${cacheOn})`);
    if (!cacheOn) throw new Error("该平台未开启缓存订单，请先开启");

    // 2. 临时把策略切成“只确认不成交”（在途订单可测撤单成功）
    const origMode = plat.strategy.mode;
    const gwConfig = JSON.parse(JSON.stringify(gw.config));
    const testPlat = gwConfig.platforms.find((p) => p.id === pid);
    testPlat.strategy.mode = "ackOnly";
    try { await call("stop_gateway", { id: gwId }); } catch { /* 未运行忽略 */ }
    await call("save_gateway", { gateway: gwConfig });
    await call("start_gateway", { id: gwId });
    await waitMs(300);
    console.log("网关已重启（策略=只确认不成交）");

    // 3. TCP 登录
    console.log("TCP 连接中...");
    const sock = await connectTcp(port);
    console.log("TCP 已连接，发送 Logon");
    sock.on("data", (d) => console.log("[raw] 收到", d.length, "字节"));
    sock.on("close", () => console.log("[tcp] 连接被关闭"));
    sock.on("error", (e) => console.log("[tcp] 错误:", e.message));
    sock.write(frame(1, logonBody()));
    let f = await readFrame(sock);
    console.log("Logon 应答 MsgType:", f.mt, "（应=1）");
    // 平台信息(9) + 平台状态(6) 先到；业务前先排空会话消息
    while (f.mt !== 1) f = await readFrame(sock);

    // 4. 发一笔委托（100 股 @ 10.01 元），ClOrdID 用时间戳保证唯一
    const CL = "T" + String(Date.now()).slice(-9);
    sock.write(frame(100101, newOrderBody(CL, 10000, 100100)));
    f = await readFrame(sock);
    console.log("委托回报 MsgType:", f.mt, "（应=200102）");

    // 5. 查订单缓存：应有 1 笔已报（ackOnly 策略下确认后仍是 new）
    let orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    let o = orders.find((x) => x.clOrdId === CL);
    console.log("缓存订单:", o ? `ClOrdID=${o.clOrdId} 状态=${o.status} 价格=${o.price} 数量=${o.qty}` : "未找到 ✗");
    if (!o || o.status !== "new" || o.price !== 10.01 || o.qty !== 100) {
        throw new Error("订单缓存登记不正确");
    }

    // 6. 撤单（原单在途 new）→ 应撤单成功
    const CXL = "C" + String(Date.now()).slice(-9);
    sock.write(frame(190007, cancelBody(CXL, CL)));
    f = await readFrame(sock);
    const execType = f.body.readUInt8(4 + 8 + 3 + 6 + 6 + 8 + 4 + 2 + 2 + 8 + 8 + 16 + 10 + 10 + 16); // exec_type 位置
    console.log("撤单回报 MsgType:", f.mt, "（应=200102），ExecType:", String.fromCharCode(execType), "（应=4 已撤）");
    if (f.mt !== 200102 || execType !== 0x34) throw new Error("在途撤单未成功");

    // 7. 缓存状态应为已撤
    orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    o = orders.find((x) => x.clOrdId === CL);
    console.log("撤单后缓存状态:", o?.status, "（应=cancelled）");
    if (o?.status !== "cancelled") throw new Error("缓存状态未更新为已撤");

    // 8. 再撤一次（已撤终态）→ 应撤单失败
    sock.write(frame(190007, cancelBody("C" + String(Date.now()).slice(-9), CL)));
    f = await readFrame(sock);
    console.log("二次撤单回报 MsgType:", f.mt, "（应=290008 撤单失败）");
    if (f.mt !== 290008) throw new Error("终态订单撤单未失败");

    // 9. 恢复原策略并收尾
    sock.destroy();
    try { await call("stop_gateway", { id: gwId }); } catch { /* 忽略 */ }
    if (origMode !== "ackOnly") {
        const restore = JSON.parse(JSON.stringify(gw.config));
        restore.platforms.find((p) => p.id === pid).strategy.mode = origMode;
        await call("save_gateway", { gateway: restore });
        await call("start_gateway", { id: gwId });
        console.log(`网关已恢复（策略=${origMode}）`);
    }
    console.log("端到端冒烟测试通过 ✓");
}

main()
    .catch((e) => {
        console.error("端到端冒烟失败 ✗", e.message);
        process.exitCode = 1;
    })
    .finally(() => {
        ws.close();
        process.exit();
    });
