# 交叉编译 glibc 版 simx-server（Windows 打包机 + zig）。
#
# 用途：Linux 服务端要使用「同步持仓数据」（Oracle Instant Client）时必须用
# glibc 动态链接版——随包发布的 musl 静态版无法 dlopen glibc 客户端。
#
# 产物：dist\linux-glibc\simx-server-glibc（默认 glibc 2.17 基线，CentOS 7+ 可运行）
# 用法：.\scripts\build-glibc-server.ps1 [-GlibcVersion 2.17]
# 注意：不改版本号；不影响 pack-all.ps1 生成的 7z（发布包布局保持固定）。

param([string]$GlibcVersion = "2.17")

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$target = "x86_64-unknown-linux-gnu"
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

# 探测 zig（PATH 优先，其次默认安装位置 C:\zig\zig-*\zig.exe，与 build-all.ps1 一致）
$zigExe = $null
$cmd = Get-Command zig.exe -ErrorAction SilentlyContinue
if ($cmd) { $zigExe = $cmd.Source }
if (-not $zigExe) {
    $zigDir = Get-ChildItem "C:\zig" -Directory -Filter "zig-x86_64-windows-*" -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending | Select-Object -First 1
    if ($zigDir) {
        $candidate = Join-Path $zigDir.FullName "zig.exe"
        if (Test-Path $candidate) { $zigExe = $candidate }
    }
}
if (-not $zigExe) {
    Write-Host "未找到 zig.exe（安装后放入 PATH 或 C:\zig\zig-*\，见构建调试手册第 2 节）" -ForegroundColor Red
    exit 1
}

# 确保 Rust 的 glibc std 已安装（首次需下载；国内网络可先设置 rsproxy 镜像）
rustup target add $target
if ($LASTEXITCODE -ne 0) {
    Write-Host "安装 $target 失败" -ForegroundColor Red
    exit 1
}

$shim = Join-Path $root "scripts\gnu-cc\gnu-zig-cc.cmd"
$arShim = Join-Path $root "scripts\gnu-cc\gnu-zig-ar.cmd"
if (-not (Test-Path $shim) -or -not (Test-Path $arShim)) {
    Write-Host "缺少 zig 参数翻译 shim：$shim" -ForegroundColor Red
    exit 1
}

# zig 目标要带 glibc 最低版本；shim 负责把 cc-rs 追加的 Rust 三元组过滤掉
$env:ZIG_EXE = $zigExe
$env:ZIG_GLIBC_TARGET = ($target -replace "-unknown", "") + "." + $GlibcVersion
$env:CC_x86_64_unknown_linux_gnu = $shim
$env:AR_x86_64_unknown_linux_gnu = $arShim
$env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = $shim

Write-Host "== 交叉编译 glibc 版 simx-server（glibc $GlibcVersion 基线）=="
Push-Location $root
try {
    cargo build --release -p simx-server --target $target
    if ($LASTEXITCODE -ne 0) {
        Write-Host "编译失败" -ForegroundColor Red
        exit 1
    }
} finally {
    Pop-Location
}

$bin = Join-Path $root "target\$target\release\simx-server"
if (-not (Test-Path $bin)) {
    Write-Host "缺少产物: $bin" -ForegroundColor Red
    exit 1
}
$outDir = Join-Path $root "dist\linux-glibc"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
Copy-Item $bin (Join-Path $outDir "simx-server-glibc") -Force

Write-Host ""
Write-Host "编译完成: $bin"
Write-Host "已复制到: $(Join-Path $outDir 'simx-server-glibc')"
Write-Host "部署：把 simx-server-glibc 与 start.sh 放同一目录，用 ./start.sh glibc 启动（默认 musl 用 ./start.sh）"
