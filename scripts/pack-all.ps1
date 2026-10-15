# 一键编译打包脚本：一条命令产出全部发布产物，并按平台汇总到 dist\ 目录。
#
# 产物布局（dist\ 已加入 .gitignore，不进仓库）：
#   dist\windows\
#     simuxchange.exe    Windows 桌面客户端（界面 + 内嵌后端，双击即用）
#     simx-cli.exe       Windows 版 CLI（自动化测试用，连本机或远程后端）
#   dist\linux\
#     simx-server          Linux 后端（musl 静态链接，任意 x86_64 Linux 可跑）
#     simx-update-server   Linux 升级服务器（同样静态）
#     simx-cli             Linux 版 CLI
#
# 流程：npm run tauri build（其 beforeBuildCommand 即 build-all.ps1 会先交叉
# 编译 Linux 三件套与 Windows 版 CLI、构建前端，再产出桌面端 exe 与安装包）
# → 把各产物拷贝汇总到 dist\ → 校验并打印清单。
# 前提：npm install 已执行、rustup/cargo 可用、musl 交叉编译工具链就绪
# （见 docs/构建调试手册.md 第 2 节）。NSIS 安装包等其余 bundle 产物仍留在
# target\release\bundle\，本脚本只汇总"可直接使用"的裸 exe / 无后缀二进制。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$muslTarget = "x86_64-unknown-linux-musl"

# ---- 1. tauri build：内含 Linux 三件套交叉编译、Windows CLI 编译、前端构建与桌面端打包 ----
Write-Host "== tauri build（桌面客户端 + Linux 三件套 + 前端 + Windows CLI）=="
Push-Location $root
try {
    npm run tauri build
    if ($LASTEXITCODE -ne 0) {
        Write-Host "tauri build 失败，终止打包" -ForegroundColor Red
        exit 1
    }
} finally {
    Pop-Location
}

# ---- 2. 汇总产物到 dist\ ----
$distWin = Join-Path $root "dist\windows"
$distLinux = Join-Path $root "dist\linux"
New-Item -ItemType Directory -Force -Path $distWin, $distLinux | Out-Null

# 汇总清单：Windows 两件（桌面客户端 + CLI），Linux 三件（musl 静态）
# 桌面 exe 名取自 src-tauri 的 Cargo 包名 simuxchange（productName 只影响安装包命名）
$items = @(
    @{ src = "target\release\simuxchange.exe"; dst = $distWin },
    @{ src = "target\release\simx-cli.exe"; dst = $distWin }
)
foreach ($name in @("simx-server", "simx-update-server", "simx-cli")) {
    $items += @{ src = "target\$muslTarget\release\$name"; dst = $distLinux }
}

$missing = @()
foreach ($it in $items) {
    $src = Join-Path $root $it.src
    if (Test-Path $src) {
        Copy-Item $src (Join-Path $it.dst (Split-Path -Leaf $src)) -Force
        Write-Host "已汇总: $($it.src) -> $($it.dst)"
    } else {
        $missing += $src
        Write-Host "缺少产物: $src" -ForegroundColor Red
    }
}
if ($missing.Count -gt 0) { exit 1 }

Write-Host ""
Write-Host "== 打包完成，产物位于 dist\ =="
Write-Host "dist\windows\ : 桌面客户端 + Windows 版 CLI（直接运行）"
Write-Host "dist\linux\   : 后端三件套（musl 静态；拷到服务器后先 chmod +x 再运行）"
