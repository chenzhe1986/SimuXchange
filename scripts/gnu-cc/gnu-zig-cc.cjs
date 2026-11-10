// gnu-zig-cc —— zig cc 交叉编译 glibc 目标的参数翻译 shim（Windows 打包机专用）。
//
// 背景：cc-rs 交叉编译时会给编译器追加 Rust 风格三元组
// `--target=x86_64-unknown-linux-gnu`，而 zig 只认 `x86_64-linux-gnu[.glibc版本]`。
// 本 shim 过滤掉 --target=，统一补上带 glibc 最低版本的目标（默认 2.17，
// 兼容 CentOS 7+；可用环境变量 ZIG_GLIBC_TARGET 覆盖，如 x86_64-linux-gnu.2.28）。
// zig 路径可用 ZIG_EXE 覆盖（默认取 PATH 里的 zig）。
const { spawnSync } = require("child_process");

const ZIG = process.env.ZIG_EXE || "zig";
const TARGET = process.env.ZIG_GLIBC_TARGET || "x86_64-linux-gnu.2.17";
const args = process.argv.slice(2).filter((a) => !/^--target=/.test(a));
const r = spawnSync(ZIG, ["cc", "-target", TARGET, ...args], {
    stdio: "inherit",
});
process.exit(r.status === null ? 1 : r.status);
