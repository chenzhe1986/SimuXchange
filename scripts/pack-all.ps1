# 一键编译打包：产出绿色版发布目录与 7z 压缩包（不再生成 NSIS 安装包）。
#
# 产物（dist\ 已加入 .gitignore，不进仓库）：
#   dist\pkg\SimuXchange\              发布目录（解压即用，便于检查）
#   dist\SimuXchange_V<版本号>.7z      最终发布包（版本号取自 tauri.conf.json）
#
# 压缩包内容布局（与历史发布包基本一致；simx-server 额外提供 glibc 版）：
#   SimuXchange\
#     simuxchange.exe
#     simx-cli.exe
#     simx.config.json
#     docs\   CLI工具使用手册.md / Lua脚本回报编写说明.md / 产品使用手册.md / 项目简介.md
#     Linux\
#       simx-cli\simx-cli
#       simx-server\
#         simx-server-musl   静态链接，默认，任何 x86_64 Linux 可用
#         simx-server-glibc  需要 Oracle 查询时用，要求 glibc >= 2.17
#         start.sh / stop.sh （./start.sh [musl|glibc] 选择，默认 musl）
#       simx-update-server\{simx-update-server,start-update.sh,stop-update.sh}
#
# 流程：npm run tauri build --no-bundle（beforeBuildCommand 即 build-all.ps1：
#      交叉编译 Linux musl 三件套 + Windows CLI + 前端，再编译桌面端）
#      → build-glibc-server.ps1 交叉编译 glibc 版 simx-server
#      → 按上面的布局汇总到 dist\pkg\SimuXchange → 7z 压缩到 dist\。
# 版本号只读取、不修改（项目规范：除非用户明确要求，不自动升版本）。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$muslTarget = "x86_64-unknown-linux-musl"
$gnuTarget = "x86_64-unknown-linux-gnu"

# 版本号与桌面端保持一致（Tauri 配置是发版主版本号来源），仅读取不递增
$version = (Get-Content (Join-Path $root "src-tauri\tauri.conf.json") -Raw | ConvertFrom-Json).version
if (-not $version) {
    Write-Host "读取版本号失败（src-tauri\tauri.conf.json）" -ForegroundColor Red
    exit 1
}

# ---- 1. tauri build：编译全部产物；--no-bundle 不再生成 NSIS 安装包 ----
Write-Host "== tauri build（桌面端 + Linux 三件套 + Windows CLI + 前端；不生成安装程序）=="
Push-Location $root
try {
    npm run tauri build -- --no-bundle
    if ($LASTEXITCODE -ne 0) {
        Write-Host "tauri build 失败，终止打包" -ForegroundColor Red
        exit 1
    }
} finally {
    Pop-Location
}

# ---- 1.5 额外编译 glibc 版 simx-server（Oracle 查询用，独立产物 + 进发布包）----
Write-Host "== 编译 glibc 版 simx-server（Oracle 查询用）=="
& (Join-Path $PSScriptRoot "build-glibc-server.ps1")
if ($LASTEXITCODE -ne 0) {
    Write-Host "glibc 版 simx-server 编译失败，终止打包" -ForegroundColor Red
    exit 1
}

# ---- 2. 按发布包布局汇总到 dist\pkg\SimuXchange ----
$appDir = Join-Path $root "dist\pkg\SimuXchange"
if (Test-Path $appDir) { Remove-Item $appDir -Recurse -Force }
foreach ($d in @(
    $appDir,
    (Join-Path $appDir "docs"),
    (Join-Path $appDir "Linux\simx-cli"),
    (Join-Path $appDir "Linux\simx-server"),
    (Join-Path $appDir "Linux\simx-update-server")
)) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}

$items = @(
    # 桌面端与 Windows CLI
    @{ src = "target\release\simuxchange.exe"; dst = $appDir },
    @{ src = "target\release\simx-cli.exe"; dst = $appDir },
    # 运行配置（与程序同目录）
    @{ src = "simx.config.json"; dst = $appDir },
    # 用户文档（只放产品相关四份，构建手册等内部文档不进发布包）
    @{ src = "docs\CLI工具使用手册.md"; dst = (Join-Path $appDir "docs") },
    @{ src = "docs\Lua脚本回报编写说明.md"; dst = (Join-Path $appDir "docs") },
    @{ src = "docs\产品使用手册.md"; dst = (Join-Path $appDir "docs") },
    @{ src = "docs\项目简介.md"; dst = (Join-Path $appDir "docs") },
    # Linux 三件套（各自独立目录 + 对应启停脚本）
    @{ src = "target\$muslTarget\release\simx-cli"; dst = (Join-Path $appDir "Linux\simx-cli") },
    # 两个 simx-server 版本：musl 默认通用，glibc 用于 Oracle 查询
    @{ src = "target\$muslTarget\release\simx-server"; dst = (Join-Path $appDir "Linux\simx-server"); name = "simx-server-musl" },
    @{ src = "target\$gnuTarget\release\simx-server"; dst = (Join-Path $appDir "Linux\simx-server"); name = "simx-server-glibc" },
    @{ src = "scripts\start.sh"; dst = (Join-Path $appDir "Linux\simx-server") },
    @{ src = "scripts\stop.sh"; dst = (Join-Path $appDir "Linux\simx-server") },
    @{ src = "target\$muslTarget\release\simx-update-server"; dst = (Join-Path $appDir "Linux\simx-update-server") },
    @{ src = "scripts\start-update.sh"; dst = (Join-Path $appDir "Linux\simx-update-server") },
    @{ src = "scripts\stop-update.sh"; dst = (Join-Path $appDir "Linux\simx-update-server") }
)

$missing = @()
foreach ($it in $items) {
    $src = Join-Path $root $it.src
    if (Test-Path $src) {
        $targetName = if ($it.name) { $it.name } else { Split-Path -Leaf $src }
        Copy-Item $src (Join-Path $it.dst $targetName) -Force
        Write-Host "已汇总: $($it.src) -> $targetName"
    } else {
        $missing += $src
        Write-Host "缺少产物: $src" -ForegroundColor Red
    }
}
if ($missing.Count -gt 0) { exit 1 }

# ---- 3. 7z 压缩：包根目录固定为 SimuXchange，与历史发布包一致 ----
$sevenZip = (Get-Command 7z.exe -ErrorAction SilentlyContinue).Source
if (-not $sevenZip) {
    Write-Host "未找到 7z 命令，请安装 7-Zip/NanaZip 并确保 7z 在 PATH 中" -ForegroundColor Red
    exit 1
}
$archive = Join-Path $root "dist\SimuXchange_V$version.7z"
if (Test-Path $archive) { Remove-Item $archive -Force }
Push-Location (Join-Path $root "dist\pkg")
try {
    & $sevenZip a -t7z -mx=9 $archive "SimuXchange" | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "7z 压缩失败" -ForegroundColor Red
        exit 1
    }
} finally {
    Pop-Location
}

$sizeMb = [math]::Round((Get-Item $archive).Length / 1MB, 2)
Write-Host ""
Write-Host "== 打包完成 =="
Write-Host "发布目录  : $appDir"
Write-Host "发布压缩包: $archive（$sizeMb MB）"
