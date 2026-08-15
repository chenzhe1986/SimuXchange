// 最小 TCP 调试：连上后发 Logon，打印收到的所有字节
import net from "node:net";

const pad = (s, n) => (s + " ".repeat(n)).slice(0, n);
const i32 = (v) => {
    const b = Buffer.alloc(4);
    b.writeInt32BE(v);
    return b;
};
const checksum = (buf) => buf.reduce((a, b) => (a + b) % 256, 0);
const frame = (mt, body) => {
    const head = Buffer.concat([i32(mt), i32(body.length)]);
    const cks = Buffer.alloc(4);
    cks.writeUInt32BE(checksum(Buffer.concat([head, body])));
    return Buffer.concat([head, body, cks]);
};

function logonBody() {
    return Buffer.concat([
        Buffer.from(pad("OMS0001", 20)),
        Buffer.from(pad("SIMX_TGW", 20)),
        i32(30),
        Buffer.from(pad("", 16)),
        Buffer.from(pad("", 32)),
    ]);
}

const s = net.connect(10001, "127.0.0.1", () => {
    console.log("已连接，发送 Logon");
    s.write(frame(1, logonBody()));
});
s.on("data", (d) => {
    console.log("收到", d.length, "字节:", d.toString("hex").slice(0, 200));
    if (d.length >= 8) {
        console.log("MsgType:", d.readInt32BE(0), "BodyLen:", d.readInt32BE(4));
    }
});
s.on("error", (e) => console.error("错误:", e.message));
s.on("close", () => console.log("连接关闭"));
setTimeout(() => {
    console.log("3 秒后退出");
    process.exit(0);
}, 3000);
