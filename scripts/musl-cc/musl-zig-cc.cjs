// musl-zig-cc —— zig cc 交叉编译参数翻译 shim（Windows 打包机专用）。
//
// 背景：mlua 的构建脚本用 cc-rs 编译 vendored Lua C 源码。交叉编译时 cc-rs 会
// 给编译器追加 Rust 风格的三元组 `--target=x86_64-unknown-linux-musl`，而 zig 的
// 目标解析器不认带 unknown vendor 的写法（只认 x86_64-linux-musl），直接用会报
// "unable to parse target query ... UnknownOperatingSystem"。
// 本 shim 过滤掉 --target= 参数、换成 zig 认识的 -target，再转调 zig cc。
// zig 路径可用环境变量 ZIG_EXE 覆盖（默认取 PATH 里的 zig）。
const { spawnSync } = require("child_process");

const ZIG = process.env.ZIG_EXE || "zig";
const args = process.argv.slice(2).filter((a) => !/^--target=/.test(a));
const r = spawnSync(ZIG, ["cc", "-target", "x86_64-linux-musl", ...args], {
    stdio: "inherit",
});
process.exit(r.status === null ? 1 : r.status);
