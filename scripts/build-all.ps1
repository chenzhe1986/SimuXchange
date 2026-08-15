# 打包前的统一准备脚本（`npm run tauri build` 时自动执行，作为 beforeBuildCommand）。
#
# 做两件事：
# 1. 确保 Linux 后端产物存在（target\x86_64-unknown-linux-musl\release\simx-server），
#    缺失时自动尝试交叉编译（安装 musl target → cargo build，链接失败退回 rust-lld
#    自包含链接）。编译也失败则中止打包并给出提示——安装包要包含该文件，缺不得。
# 2. 构建前端（tauri build 的 beforeBuildCommand 执行本脚本，代替默认的 npm run build）。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$target = "x86_64-unknown-linux-musl"
$linuxBin = Join-Path $root "target\$target\release\simx-server"
# 链接方式标记文件（放 target 目录，不进仓库）：记录上次使用的链接方式，
# 保证 RUSTFLAGS 稳定——cargo 把 RUSTFLAGS 计入编译指纹，稳定了代码没变时
# 才不会重复链接
$marker = Join-Path $root "target\$target\.linker-mode"

Write-Host "== 准备 Linux 后端产物 =="

# 确保 musl target 已安装（rustup 幂等，已装时秒过）
rustup target add $target
if ($LASTEXITCODE -ne 0) {
    Write-Host "安装 musl target 失败，无法编译 Linux 后端" -ForegroundColor Red
    exit 1
}

# 决定链接方式（关键：避免每次打包都先“报错→重试”）：
# - 机器上装有 musl 交叉工具链（x86_64-linux-musl-gcc / musl-gcc）→ 常规链接；
# - 没有 → rust-lld 自包含链接（Rust 自带链接器，rustc 1.82+ 无需额外安装）。
# 首次探测后把结果记到标记文件，之后打包直接沿用，不再重复探测。
$mode = if (Test-Path $marker) { (Get-Content $marker).Trim() } else { "" }
if ($mode -eq "") {
    $hasMuslGcc = $false
    foreach ($c in @("x86_64-linux-musl-gcc", "musl-gcc")) {
        if (Get-Command $c -ErrorAction SilentlyContinue) { $hasMuslGcc = $true; break }
    }
    $mode = if ($hasMuslGcc) { "musl-gcc" } else { "rust-lld" }
    New-Item -ItemType Directory -Force -Path (Split-Path $marker) | Out-Null
    Set-Content -Path $marker -Value $mode -NoNewline -Encoding ascii
    Write-Host "链接方式: $mode（已记录到 $marker，之后打包沿用）"
}

# 始终执行 cargo build：由 cargo 按源码编译指纹自行判断是否需要重编/重链。
# 不能“产物存在就跳过”——那样改了 simx-core/simx-server 代码后，打包进安装包
# 的仍是旧版 Linux 后端。cargo 的指纹判断很快：代码没变时这一步接近秒过。
if ($mode -eq "rust-lld") {
    $env:RUSTFLAGS = "-C linker=rust-lld -C link-self-contained=yes"
}
cargo build --release -p simx-server --target $target
if ($LASTEXITCODE -ne 0) {
    if ($mode -eq "musl-gcc") {
        # 装了工具链但链接仍失败（版本不匹配等）：退回 rust-lld 并更新标记
        Write-Host "常规链接失败，改用 rust-lld 自包含链接重试..."
        Set-Content -Path $marker -Value "rust-lld" -NoNewline -Encoding ascii
        $env:RUSTFLAGS = "-C linker=rust-lld -C link-self-contained=yes"
        cargo build --release -p simx-server --target $target
        if ($LASTEXITCODE -ne 0) {
            Write-Host "Linux 后端编译失败，请检查 musl 交叉编译工具链（或确认 rustc 版本 >= 1.82 以使用 rust-lld）" -ForegroundColor Red
            exit 1
        }
    } else {
        Write-Host "rust-lld 链接失败（rustc 1.82+ 自带 rust-lld；版本过旧请先 rustup update）" -ForegroundColor Red
        exit 1
    }
}
Write-Host "Linux 后端就绪: $linuxBin"

Write-Host "== 构建前端 =="
Push-Location $root
npm run build
$code = $LASTEXITCODE
Pop-Location
exit $code
