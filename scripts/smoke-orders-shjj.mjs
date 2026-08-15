// 上海竞价(shjj)/新债券(shbond)端到端冒烟：验证订单缓存 + 按状态撤单 + 终态保护
// 用法：node scripts/smoke-orders-shjj.mjs [网关ID] [平台ID] [shjj|shbond]
// 前置：simx-server 已启动
import net from "node:net";

const WS_URL = "ws://127.0.0.1:9800/ws";
const GW_ID = process.argv[2];
const PLATFORM_ID = process.argv[3];
// 协议变体：shjj（竞价，版本 0.54）或 shbond（新债券，版本 1.90）
const PROTO = process.argv[4] ?? "shjj";
const IS_BOND = PROTO === "shbond";

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

// ---------- 上交所 0.54 协议报文构造 ----------
const pad = (s, n) => (s + " ".repeat(n)).slice(0, n);
const u16 = (v) => {
    const b = Buffer.alloc(2);
    b.writeUInt16BE(v);
    return b;
};
const u32 = (v) => {
    const b = Buffer.alloc(4);
    b.writeUInt32BE(v >>> 0);
    return b;
};
const u64 = (v) => {
    const b = Buffer.alloc(8);
    b.writeBigUInt64BE(BigInt(v));
    return b;
};
const i64 = (v) => {
    const b = Buffer.alloc(8);
    b.writeBigInt64BE(BigInt(v));
    return b;
};
const checksum = (buf) => buf.reduce((a, b) => (a + b) % 256, 0);
/** shjj 帧：MsgType(4) + MsgSeqNum(8) + BodyLen(4) + body + 校验和(4) */
const frame = (mt, body, seq = 1) => {
    const head = Buffer.concat([u32(mt), u64(seq), u32(body.length)]);
    const cks = Buffer.alloc(4);
    cks.writeUInt32BE(checksum(Buffer.concat([head, body])));
    return Buffer.concat([head, body, cks]);
};

/** Logon(40)：sender 32 + target 32 + hb u16 + ver 8 + date u32 + qsize u32 */
function logonBody() {
    return Buffer.concat([
        Buffer.from(pad("OMS0001", 32)),
        Buffer.from(pad("TDGW", 32)),
        u16(30),
        Buffer.from(pad(IS_BOND ? "1.90" : "0.54", 8)),
        u32(20260814),
        u32(8),
    ]);
}

/** 委托(58)：biz_id u32 + biz_pbu 8 + cl_ord_id 10 + sec 12 + acct 13 + owner u8 + side 1 + price i64 + qty i64 + type 1 + tif 1 + ntime u64 + credit 2 + firm 8 + branch 8 + user 32 */
function newOrderBody(clOrdId) {
    return Buffer.concat([
        u32(IS_BOND ? 1 : 100010), // 债券现券竞价 = 1 / 现货竞价 = 100010
        Buffer.from(pad("PBU00001", 8)),
        Buffer.from(pad(clOrdId, 10)),
        Buffer.from(pad("600000", 12)),
        Buffer.from(pad("A123456789", 13)),
        Buffer.from("1"), // owner_type 占 1 字节
        Buffer.from("1"), // side 买
        i64(1005000),     // price 10.05 元 × 10 万
        i64(100000),      // qty 100 股 × 1000
        Buffer.from("2"), // ord_type 限价
        Buffer.from("0"),
        u64(113000000000000),
        Buffer.from(pad("", 2)),
        Buffer.from(pad("", 8)),
        Buffer.from(pad("0001", 8)),
        Buffer.from(pad("", 32)),
    ]);
}

/** 撤单(61)：biz_id u32 + biz_pbu 8 + cl_ord_id 10 + sec 12 + acct 13 + owner u8 + side 1 + orig 10 + ntime u64 + branch 8 + user 32 */
function cancelBody(clOrdId, origClOrdId) {
    return Buffer.concat([
        u32(IS_BOND ? 1 : 100010),
        Buffer.from(pad("PBU00001", 8)),
        Buffer.from(pad(clOrdId, 10)),
        Buffer.from(pad("600000", 12)),
        Buffer.from(pad("A123456789", 13)),
        Buffer.from("1"),
        Buffer.from("1"),
        Buffer.from(pad(origClOrdId, 10)),
        u64(113000000000000),
        Buffer.from(pad("0001", 8)),
        Buffer.from(pad("", 32)),
    ]);
}

/** 执行回报同步(206)：组数 u16 + (pbu 8 + set_id u32 + begin u64) */
function syncBody() {
    return Buffer.concat([
        u16(1),
        Buffer.from(pad("OMS0001", 8)),
        u32(1),
        u64(0),
    ]);
}

// ---------- TCP 会话 ----------
function connectTcp(port) {
    return new Promise((resolve, reject) => {
        const s = net.connect(port, "127.0.0.1", () => resolve(s));
        s.on("error", reject);
    });
}

/** 读取一条完整报文：累积缓冲直到凑齐一条完整帧 */
function readFrame(sock) {
    return new Promise((resolve, reject) => {
        let buf = Buffer.alloc(0);
        const onData = (chunk) => {
            buf = Buffer.concat([buf, chunk]);
            if (buf.length < 16) return;
            const mt = buf.readUInt32BE(0);
            const len = buf.readUInt32BE(12);
            if (buf.length < 16 + len + 4) return;
            sock.removeListener("data", onData);
            if (buf.length > 16 + len + 4) sock.unshift(buf.subarray(16 + len + 4));
            resolve({ mt, body: buf.subarray(16, 16 + len) });
        };
        sock.on("data", onData);
        sock.on("error", reject);
    });
}

/** 读报文直到出现指定 MsgType（跳过来回的其他消息，如心跳） */
async function readUntil(sock, wantMt, seen = []) {
    for (;;) {
        const f = await readFrame(sock);
        seen.push(f.mt);
        if (f.mt === wantMt) return f;
    }
}

const waitMs = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
    await new Promise((resolve, reject) => {
        ws.onopen = resolve;
        ws.onerror = reject;
    });

    // 1. 定位网关（没有就自动创建一个）
    let gwId = GW_ID, pid = PLATFORM_ID;
    const snap = await call("get_snapshot");
    let gw = snap.gateways.find((g) => g.config.id === gwId);
    if (!gw && !gwId) {
        gw = snap.gateways.find(
            (g) => g.config.category === PROTO && g.config.platforms.length > 0,
        );
    }
    if (!gw) {
        console.log(`未找到 ${PROTO} 网关，自动创建…`);
        await call("save_gateway", {
            gateway: {
                id: "",
                name: `冒烟 ${PROTO} 网关`,
                category: PROTO,
                platforms: [
                    {
                        id: "",
                        name: IS_BOND ? "冒烟新债券平台" : "冒烟竞价平台",
                        platformType: IS_BOND ? 2 : 0,
                        listenHost: "127.0.0.1",
                        port: IS_BOND ? 9403 : 9402,
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
        const snap2 = await call("get_snapshot");
        gw = snap2.gateways[snap2.gateways.length - 1];
    }
    gwId = gw.config.id;
    const plat = gw.config.platforms.find((p) => p.id === pid) ?? gw.config.platforms[0];
    pid = plat.id;
    const port = plat.port;
    console.log(`shjj 平台: ${plat.name} (端口 ${port}, cacheOrders=${plat.cacheOrders})`);
    if (!plat.cacheOrders) throw new Error("该平台未开启缓存订单");

    // 2. 临时切 ackOnly + 重启
    const origMode = plat.strategy.mode;
    const gwConfig = JSON.parse(JSON.stringify(gw.config));
    gwConfig.platforms.find((p) => p.id === pid).strategy.mode = "ackOnly";
    try { await call("stop_gateway", { id: gwId }); } catch { /* 忽略 */ }
    await call("save_gateway", { gateway: gwConfig });
    await call("start_gateway", { id: gwId });
    await waitMs(300);

    // 3. 登录
    const sock = await connectTcp(port);
    sock.on("close", () => console.log("[tcp] 连接被关闭"));
    sock.on("error", (e) => console.log("[tcp] 错误:", e.message));
    sock.write(frame(40, logonBody()));
    const seen = [];
    await readUntil(sock, 40, seen); // Logon 确认
    console.log("Logon 确认 ✓（收到报文:", seen.join(","), "）");

    // 4. 未同步先发委托 → 进 pending 缓冲，但缓存已登记
    const CL = "T" + String(Date.now()).slice(-9);
    sock.write(frame(58, newOrderBody(CL), 2));
    await waitMs(200);
    let orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    let o = orders.find((x) => x.clOrdId === CL);
    console.log("委托登记:", o ? `状态=${o.status} 价格=${o.price} 数量=${o.qty}` : "未找到 ✗");
    if (!o || o.status !== "new" || o.price !== 10.05 || o.qty !== 100) {
        throw new Error("shjj 订单缓存登记不正确");
    }

    // 5. 未同步先撤单（在途）→ 撤单成功回报进缓冲（同步前不发任何执行报告），
    //    但缓存状态应立即置为已撤
    const CXL = "C" + String(Date.now()).slice(-9);
    sock.write(frame(61, cancelBody(CXL, CL), 3));
    await waitMs(200);
    orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    o = orders.find((x) => x.clOrdId === CL);
    console.log("撤单后缓存状态:", o?.status, "（应=cancelled）");
    if (o?.status !== "cancelled") throw new Error("shjj 缓存状态未更新为已撤");

    // 6. 同步前不应收到撤单成功回报（执行报告流等待 207 后统一补发）
    const early = await Promise.race([
        readFrame(sock).then(() => true),
        waitMs(300).then(() => false),
    ]);
    console.log("同步前是否收到回报:", early, "（应=false，回报在缓冲中）");
    if (early) throw new Error("shjj 同步前不应推送撤单回报");

    // 7. 完成同步 → 补发缓冲回报：撤单成功(32) ExecType='4'
    //    （原委托的确认/成交因订单已撤被发送侧终态检查丢弃）
    sock.write(frame(206, syncBody(), 4));
    const f = await readUntil(sock, 32, []);
    // ExecRpt body 布局：pbu 8 + set_id 4 + report_index 8 + biz_id 4 → exec_type 在 offset 24
    const execType = f.body.readUInt8(24);
    console.log("同步后补发 MsgType:", f.mt, "（应=32），ExecType:", String.fromCharCode(execType), "（应=4）");
    if (f.mt !== 32 || execType !== 0x34) throw new Error("shjj 同步后未补发撤单成功");

    // 8. 缓存状态仍为已撤（终态保护：补发的在途确认不回退状态）
    orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    o = orders.find((x) => x.clOrdId === CL);
    console.log("同步补发后缓存状态:", o?.status, "（应仍=cancelled，终态保护）");
    if (o?.status !== "cancelled") throw new Error("shjj 终态保护失效：状态被回退");

    // 9. 再撤已撤订单 → 撤单失败(59)
    sock.write(frame(61, cancelBody("C" + String(Date.now()).slice(-9), CL), 5));
    const f2 = await readFrame(sock);
    console.log("二次撤单回报 MsgType:", f2.mt, "（应=59 撤单失败）");
    if (f2.mt !== 59) throw new Error("shjj 终态订单撤单未失败");

    // 10. 恢复原策略
    sock.destroy();
    try { await call("stop_gateway", { id: gwId }); } catch { /* 忽略 */ }
    if (origMode !== "ackOnly") {
        const restore = JSON.parse(JSON.stringify(gw.config));
        restore.platforms.find((p) => p.id === pid).strategy.mode = origMode;
        await call("save_gateway", { gateway: restore });
        await call("start_gateway", { id: gwId });
        console.log(`网关已恢复（策略=${origMode}）`);
    }
    console.log("shjj 端到端冒烟测试通过 ✓");
}

main()
    .catch((e) => {
        console.error("shjj 端到端冒烟失败 ✗", e.message);
        process.exitCode = 1;
    })
    .finally(() => {
        ws.close();
        process.exit();
    });
