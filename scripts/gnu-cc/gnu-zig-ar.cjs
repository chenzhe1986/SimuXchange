// gnu-zig-ar —— zig ar 交叉归档 shim（与 gnu-zig-cc 同理：过滤 cc-rs 的
// --target= 参数后转调 zig ar；归档不涉及目标三元组，主要防御性过滤）。
const { spawnSync } = require("child_process");

const ZIG = process.env.ZIG_EXE || "zig";
const args = process.argv.slice(2).filter((a) => !/^--target=/.test(a));
const r = spawnSync(ZIG, ["ar", ...args], { stdio: "inherit" });
process.exit(r.status === null ? 1 : r.status);
