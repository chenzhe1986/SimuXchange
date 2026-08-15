# 打包前的统一准备脚本（`npm run tauri build` 时自动执行，作为 beforeBuildCommand）。
#
# 做两件事：
# 1. 确保 Linux 后端产物存在（target\x86_64-unknown-linux-musl\release\simx-server），
#    缺失时自动尝试交叉编译（安装 musl target → cargo build，链接失败退回 rust-lld
#    自包含链接）。编译也失败则中止打包并给出提示——安装包要包含该文件，缺不得。
# 2. 构建前端（tauri build 的 beforeBuildCommand 执行本脚本，代替默认的 npm run build）。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$linuxBin = Join-Path $root "target\x86_64-unknown-linux-musl\release\simx-server"

Write-Host "== 准备 Linux 后端产物 =="
if (Test-Path $linuxBin) {
    Write-Host "已找到: $linuxBin"
} else {
    Write-Host "未找到 Linux 产物，尝试交叉编译..."
    rustup target add x86_64-unknown-linux-musl
    if ($LASTEXITCODE -ne 0) {
        Write-Host "安装 musl target 失败，无法编译 Linux 后端" -ForegroundColor Red
        exit 1
    }
    # 先按常规链接（用户可能已配置 musl 工具链）；失败则退回 rust-lld 自包含链接
    cargo build --release -p simx-server --target x86_64-unknown-linux-musl
    if ($LASTEXITCODE -ne 0) {
        Write-Host "常规链接失败，改用 rust-lld 自包含链接重试..."
        $env:RUSTFLAGS = "-C linker=rust-lld -C link-self-contained=yes"
        cargo build --release -p simx-server --target x86_64-unknown-linux-musl
        if ($LASTEXITCODE -ne 0) {
            Write-Host "Linux 后端编译失败，请检查 musl 交叉编译工具链" -ForegroundColor Red
            exit 1
        }
    }
    Write-Host "Linux 后端编译完成"
}

Write-Host "== 构建前端 =="
Push-Location $root
npm run build
$code = $LASTEXITCODE
Pop-Location
exit $code
