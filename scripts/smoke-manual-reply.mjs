// 冒烟测试：验证“手动回复在途单”（成交/拒单/撤单成功）+ 平台级报文 + 分目录持久化
// 用法：node scripts/smoke-manual-reply.mjs
// 前置：simx-server 已启动（数据在 exe 同目录，建议把 exe 复制到临时目录启动，不污染现有配置）
import net from "node:net";
import fs from "node:fs";
import path from "node:path";

const WS_URL = "ws://127.0.0.1:9800/ws";

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

// ---------- 深交所 Binary 报文构造（与 smoke-orders-e2e.mjs 相同骨架） ----------
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

/** 现券委托体（与 protocol.rs NewOrderCash 字段顺序一致） */
function newOrderBody(clOrdId, qtyRaw, priceRaw) {
    return Buffer.concat([
        Buffer.from(pad("010", 3)),        // appl_id
        Buffer.from(pad("PBU001", 6)),     // submitting_pbu_id
        Buffer.from(pad("000001", 8)),     // security_id
        Buffer.from(pad("102", 4)),        // security_id_source
        u16(0),                            // owner_type
        Buffer.from(pad("", 2)),           // clearing_firm
        i64(20260814093000000),            // transact_time
        Buffer.from(pad("", 8)),           // user_info
        Buffer.from(pad(clOrdId, 10)),     // cl_ord_id
        Buffer.from(pad("B880000001", 12)), // account_id
        Buffer.from(pad("0001", 4)),       // branch_id
        Buffer.from(pad("", 4)),           // order_restrictions
        Buffer.from("1"),                  // side 买
        Buffer.from("2"),                  // ord_type 限价
        i64(qtyRaw),                       // order_qty
        i64(priceRaw),                     // price
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

/** 读取一条完整报文帧：{ mt, body }（8 字节头 + body + 4 校验和） */
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

/** 等待并返回一笔业务回报（跳过 Logon/心跳/平台信息等会话消息） */
async function readBusiness(sock, skip = new Set([1, 2, 3, 5, 6, 7, 9])) {
    for (;;) {
        const f = await readFrame(sock);
        if (!skip.has(f.mt)) return f;
    }
}

// ---------- 主流程 ----------
async function waitMs(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

let pass = 0;
function ok(cond, label) {
    if (!cond) throw new Error(`校验失败: ${label}`);
    pass++;
    console.log(`  ✓ ${label}`);
}

async function main() {
    await new Promise((resolve, reject) => {
        ws.onopen = resolve;
        ws.onerror = reject;
    });

    // 1. 建/找 sz 测试网关（ackOnly + cacheOrders + showPackets + persistPackets）
    const snap = await call("get_snapshot");
    let gw = snap.gateways.find((g) => g.config.name === "冒烟手动回复");
    if (!gw) {
        const created = await call("save_gateway", {
            gateway: {
                id: "",
                name: "冒烟手动回复",
                category: "sz",
                platforms: [
                    {
                        id: "",
                        name: "冒烟回复平台",
                        platformType: 1,
                        listenHost: "127.0.0.1",
                        port: 9411,
                        compId: "SIMX_TGW",
                        checkPassword: false,
                        password: "",
                        partitionNo: 1,
                        showPackets: true,
                        persistPackets: true,
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
        gw = snap2.gateways.find((g) => g.config.id === created.id);
    }
    const pid = gw.config.platforms[0].id;
    const port = gw.config.platforms[0].port;
    const gwId = gw.config.id;
    console.log(`测试平台: ${gw.config.platforms[0].name} (端口 ${port})`);

    // 2. 启动网关并 TCP 登录
    try { await call("stop_gateway", { id: gwId }); } catch { /* 未运行忽略 */ }
    await call("start_gateway", { id: gwId });
    await waitMs(300);
    console.log("网关已启动");
    const sock = await connectTcp(port);
    sock.write(frame(1, logonBody()));
    let f = await readFrame(sock);
    while (f.mt !== 1) f = await readFrame(sock);
    console.log("TCP 登录成功");

    // 3. 单连接限制：已有连接时，新 TCP 连接被立即拒绝（accept 后直接断开）
    const intruder = await connectTcp(port);
    const intruderClosed = await new Promise((resolve) => {
        intruder.once("close", () => resolve(true));
        intruder.once("error", () => resolve(true));
        setTimeout(() => resolve(false), 2000);
    });
    intruder.destroy();
    ok(intruderClosed, "已有连接时新 TCP 连接被立即拒绝");

    // 4. 下单 A（100 股 @ 10.01，ackOnly 保持“已报”在途）
    const CL_A = "A" + String(Date.now()).slice(-9);
    sock.write(frame(100101, newOrderBody(CL_A, 10000, 100100)));
    f = await readBusiness(sock);
    ok(f.mt === 200102, `下单 A 收到确认回报（mt=${f.mt}）`);

    // 5. 手动回复成交：40 股 @ 10.05（部分成交）
    const r1 = await call("send_report", {
        gatewayId: gwId, platformId: pid, clOrdId: CL_A, kind: "trade",
        qty: 40, price: 10.05,
    });
    console.log("  send_report(trade 40@10.05):", r1.desc);
    f = await readBusiness(sock);
    ok(f.mt === 200115, `收到成交回报（mt=${f.mt}，应=200115）`);
    ok(f.body.readUInt8(101) === 0x46, "成交回报 ExecType=F");
    ok(f.body.readBigInt64BE(111) === 4000n, `成交数量=4000（协议×100，实得 ${f.body.readBigInt64BE(111)}）`);
    ok(f.body.readBigInt64BE(103) === 100500n, `成交价格=100500（协议×10000，实得 ${f.body.readBigInt64BE(103)}）`);
    ok(f.body.readBigInt64BE(119) === 6000n, "剩余=6000（×100）");
    let orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    let o = orders.find((x) => x.clOrdId === CL_A);
    ok(o && o.status === "partial" && o.cumQty === 40 && o.leavesQty === 60,
        `订单 A 缓存为部分成交（cum=${o?.cumQty}, leaves=${o?.leavesQty}）`);

    // 6. 手动回复成交：不传参数（默认剩余量全成 + 委托价）
    await call("send_report", { gatewayId: gwId, platformId: pid, clOrdId: CL_A, kind: "trade" });
    f = await readBusiness(sock);
    ok(f.mt === 200115 && f.body.readBigInt64BE(111) === 6000n, "默认成交=剩余 60 股全成");
    ok(f.body.readBigInt64BE(103) === 100100n, "默认价格=委托价 10.01");
    orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    o = orders.find((x) => x.clOrdId === CL_A);
    ok(o && o.status === "filled", "订单 A 缓存为全部成交");

    // 7. 终态保护：已全成的订单不能再手动回复
    try {
        await call("send_report", { gatewayId: gwId, platformId: pid, clOrdId: CL_A, kind: "cancel" });
        throw new Error("终态订单竟然允许回复");
    } catch (e) {
        ok(e.message.includes("终态"), `终态保护生效（${e.message}）`);
    }

    // 8. 下单 B → 手动拒单（原因代码 3）
    const CL_B = "B" + String(Date.now()).slice(-9);
    sock.write(frame(100101, newOrderBody(CL_B, 10000, 100100)));
    f = await readBusiness(sock);
    ok(f.mt === 200102, `下单 B 收到确认回报（mt=${f.mt}）`);
    const r2 = await call("send_report", {
        gatewayId: gwId, platformId: pid, clOrdId: CL_B, kind: "reject", reason: 3,
    });
    console.log("  send_report(reject reason=3):", r2.desc);
    f = await readBusiness(sock);
    ok(f.mt === 200102, `收到拒单回报（mt=${f.mt}，应=200102）`);
    ok(f.body.readUInt8(111) === 0x38, "拒单 ExecType=8");
    ok(f.body.readUInt16BE(113) === 3, `拒单原因代码=3（实得 ${f.body.readUInt16BE(113)}）`);
    orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    o = orders.find((x) => x.clOrdId === CL_B);
    ok(o && o.status === "rejected", "订单 B 缓存为已拒绝");

    // 9. 下单 C → 手动撤单成功
    const CL_C = "C" + String(Date.now()).slice(-9);
    sock.write(frame(100101, newOrderBody(CL_C, 10000, 100100)));
    f = await readBusiness(sock);
    ok(f.mt === 200102, `下单 C 收到确认回报（mt=${f.mt}）`);
    const r3 = await call("send_report", {
        gatewayId: gwId, platformId: pid, clOrdId: CL_C, kind: "cancel",
    });
    console.log("  send_report(cancel):", r3.desc);
    f = await readBusiness(sock);
    ok(f.mt === 200102, `收到撤单成功回报（mt=${f.mt}，应=200102）`);
    ok(f.body.readUInt8(111) === 0x34, "撤单 ExecType=4");
    ok(f.body.subarray(85, 95).toString().trim() === CL_C, "OrigClOrdID=原单号（手动模式无撤单请求号）");
    orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    o = orders.find((x) => x.clOrdId === CL_C);
    ok(o && o.status === "cancelled", "订单 C 缓存为已撤");

    // 9.5 手动确认回报：下单 E → 手动回确认（ExecType='0'），订单保持可继续回复
    const CL_E = "E" + String(Date.now()).slice(-9);
    sock.write(frame(100101, newOrderBody(CL_E, 10000, 100100)));
    f = await readBusiness(sock);
    ok(f.mt === 200102, `下单 E 收到确认回报（mt=${f.mt}）`);
    const r5 = await call("send_report", {
        gatewayId: gwId, platformId: pid, clOrdId: CL_E, kind: "ack",
    });
    console.log("  send_report(ack):", r5.desc);
    f = await readBusiness(sock);
    ok(f.mt === 200102, `收到手动确认回报（mt=${f.mt}，应=200102）`);
    ok(f.body.readUInt8(111) === 0x30, "确认回报 ExecType=0（已报）");
    orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    o = orders.find((x) => x.clOrdId === CL_E);
    ok(o && o.status === "new", "订单 E 缓存仍为已报（确认不改变在途状态）");

    // 9.6 前台拒单：下单 F → 勾选前台拒单（frontReject=true）→ 回业务拒绝消息(MsgType=4)
    const CL_F = "F" + String(Date.now()).slice(-9);
    sock.write(frame(100101, newOrderBody(CL_F, 10000, 100100)));
    f = await readBusiness(sock);
    ok(f.mt === 200102, `下单 F 收到确认回报（mt=${f.mt}）`);
    const r6 = await call("send_report", {
        gatewayId: gwId, platformId: pid, clOrdId: CL_F, kind: "reject",
        reason: 20009, frontReject: true,
    });
    console.log("  send_report(frontReject=true reason=20009):", r6.desc);
    f = await readBusiness(sock);
    ok(f.mt === 4, `收到业务拒绝消息（mt=${f.mt}，应=4）`);
    // MsgType=4 布局：ApplID3+TransactTime8+SubmitPbu6+SecurityID8+SecSrc4+RefSeqNum8+RefMsgType4+RefID10 = 51 → Reason(u16)
    ok(f.body.readUInt16BE(51) === 20009, `业务拒绝原因代码=20009（实得 ${f.body.readUInt16BE(51)}）`);
    orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    o = orders.find((x) => x.clOrdId === CL_F);
    ok(o && o.status === "rejected", "订单 F 缓存为已拒绝");

    // 10. 统计校验：委托 5、成交 2（A 两笔回复合并 1 个计数）、拒单 2（B 执行报告 + F 前台拒单）、撤单 0（撤单成功不占计数）
    const snap2 = await call("get_snapshot");
    const ps = snap2.gateways.find((g) => g.config.id === gwId)
        .platforms.find((p) => p.platformId === pid).stats;
    ok(ps.orders === 5, `统计·委托=${ps.orders}（应=5）`);
    ok(ps.trades === 2, `统计·成交=${ps.trades}（应=2）`);
    ok(ps.orderRejects === 2, `统计·拒单=${ps.orderRejects}（应=2，含前台拒单）`);
    ok(ps.cancels === 0, `统计·撤单=${ps.cancels}（应=0，手动撤单不占撤单请求计数）`);

    // 11. 平台级报文：跨连接聚合 + 每行带 connId + 连接概要（分组标题数据源）
    const page = await call("get_platform_packets", { gatewayId: gwId, platformId: pid, afterSeq: 0 });
    ok(Array.isArray(page.packets) && page.packets.length > 0, `平台报文共 ${page.packets.length} 条`);
    ok(page.packets.every((p) => p.connId > 0), "每条报文都带 connId 标注");
    const seqs = page.packets.map((p) => p.seq);
    ok(seqs.every((s, i) => i === 0 || s > seqs[i - 1]), "报文按平台级 seq 有序递增");
    const again = await call("get_platform_packets", { gatewayId: gwId, platformId: pid, afterSeq: page.latestSeq });
    ok(again.packets.length === 0, "增量拉取（游标之后）为空");
    ok(Array.isArray(page.conns) && page.conns.length === 1 && page.conns[0].alive === true,
        `conns 含 1 个在线连接（#${page.conns[0]?.connId} ${page.conns[0]?.peer} ${page.conns[0]?.since} 接入）`);

    // 12. 报文文件分目录：packets/<YYYYMMDD>/<网关名>_<平台名>/pkg_<HHMMSS>_ip-<IP>_port-<端口>.log
    // （simx-server 以 scripts/.smoke-data 为数据目录时，报文文件也在其下）
    const dir = path.join(process.cwd(), "scripts", ".smoke-data", "packets");
    const days = fs.existsSync(dir) ? fs.readdirSync(dir) : [];
    ok(days.length > 0, `packets/ 下按日期分目录：${days.join(", ")}`);
    const today = days[days.length - 1];
    ok(/^\d{8}$/.test(today), `日期目录为 YYYYMMDD（${today}）`);
    const platDir = path.join(dir, today, `${gw.config.name}_${gw.config.platforms[0].name}`);
    const logs = fs.existsSync(platDir) ? fs.readdirSync(platDir) : [];
    ok(logs.length > 0, `日期/${gw.config.name}_${gw.config.platforms[0].name}/ 下有报文文件：${logs.join(", ")}`);
    const pkgRe = /^pkg_\d{6}_ip-\d+_\d+_\d+_\d+_port-\d+\.log$/;
    ok(logs.every((l) => pkgRe.test(l)), `文件名格式 pkg_<时间>_ip-<IP点转下划线>_port-<端口>.log（${logs.join(", ")}）`);
    const sample = fs.readFileSync(path.join(platDir, logs[0]), "utf8");
    ok(sample.trim().length > 0, "报文文件内容非空");
    const filesBefore = logs.length;

    // 13. 旧连接再下一单 D（断开后由新连接回复它，验证“回复走当前连接”）
    const CL_D = "D" + String(Date.now()).slice(-9);
    sock.write(frame(100101, newOrderBody(CL_D, 10000, 100100)));
    f = await readBusiness(sock);
    ok(f.mt === 200102, `下单 D 收到确认回报（mt=${f.mt}）`);

    // 14. 断开旧连接：平台报文仍完整可回看（记录器保留），连接标记为离线
    sock.destroy();
    await waitMs(500);
    const page2 = await call("get_platform_packets", { gatewayId: gwId, platformId: pid, afterSeq: 0 });
    ok(page2.packets.length >= page.packets.length, "断开后平台报文仍完整可拉（历史连接报文保留）");
    ok(page2.conns.length === 1 && page2.conns[0].alive === false,
        `旧连接在 conns 中标记为已离线（#${page2.conns[0]?.connId} ${page2.conns[0]?.peer}）`);

    // 15. 重连：新连接正常登录，conns 按连接号列出 2 个连接（旧离线 + 新在线）
    const sock2 = await connectTcp(port);
    sock2.write(frame(1, logonBody()));
    f = await readFrame(sock2);
    while (f.mt !== 1) f = await readFrame(sock2);
    console.log("重连登录成功");
    const page3 = await call("get_platform_packets", { gatewayId: gwId, platformId: pid, afterSeq: 0 });
    ok(page3.conns.length === 2, `conns 列出 2 个历史连接（${page3.conns.map((c) => `#${c.connId}${c.alive ? "在线" : "离线"}`).join("、")}）`);
    ok(page3.conns[0].connId < page3.conns[1].connId, "连接按连接号升序排列");
    ok(page3.conns[0].alive === false && page3.conns[1].alive === true, "旧连接离线、新连接在线");

    // 16. 对旧连接发的订单 D 手动回复：回报从当前（新）连接发出
    const r4 = await call("send_report", {
        gatewayId: gwId, platformId: pid, clOrdId: CL_D, kind: "trade",
        qty: 40, price: 10.05,
    });
    console.log("  send_report(旧连接订单 trade 40@10.05):", r4.desc);
    f = await readBusiness(sock2);
    ok(f.mt === 200115 && f.body.readUInt8(101) === 0x46, "新连接收到旧订单成交回报（回复走当前连接）");
    ok(f.body.readBigInt64BE(111) === 4000n, `成交数量=4000（实得 ${f.body.readBigInt64BE(111)}）`);
    ok(f.body.readBigInt64BE(103) === 100500n, `成交价格=100500（实得 ${f.body.readBigInt64BE(103)}）`);
    orders = await call("get_orders", { gatewayId: gwId, platformId: pid });
    o = orders.find((x) => x.clOrdId === CL_D);
    ok(o && o.status === "partial" && o.cumQty === 40, `订单 D 缓存为部分成交（cum=${o?.cumQty}）`);

    // 17. 新连接产生新的报文文件（每个连接一个文件）+ 统计补齐
    const filesAfter = fs.readdirSync(platDir);
    ok(filesAfter.length === filesBefore + 1,
        `重连后新增 1 个报文文件（${filesBefore} → ${filesAfter.length}）`);
    ok(pkgRe.test(filesAfter[filesAfter.length - 1]), "新文件同样符合 pkg_ 命名格式");
    const snap3 = await call("get_snapshot");
    const ps3 = snap3.gateways.find((g) => g.config.id === gwId)
        .platforms.find((p) => p.platformId === pid).stats;
    ok(ps3.orders === 6, `统计·委托=${ps3.orders}（应=6，含断开前 D 与 E/F）`);
    ok(ps3.trades === 3, `统计·成交=${ps3.trades}（应=3，含 D 的一笔）`);

    // 18. 收尾
    sock2.destroy();
    try { await call("stop_gateway", { id: gwId }); } catch { /* 忽略 */ }
    console.log(`\n手动回复冒烟测试通过 ✓（${pass} 项校验全部通过）`);
}

main()
    .catch((e) => {
        console.error("冒烟失败 ✗", e.message);
        process.exitCode = 1;
    })
    .finally(() => {
        ws.close();
        process.exit();
    });
