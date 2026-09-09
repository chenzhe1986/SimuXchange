# 打包前的统一准备脚本（`npm run tauri build` 时自动执行，作为 beforeBuildCommand）。
#
# 做两件事：
# 1. 确保 Linux 后端产物存在（交叉编译两个二进制到 musl 静态目标）：
#      - simx-server       运行时后端，内嵌进桌面安装包，缺不得；
#      - simx-update-server 升级服务器，独立部署到发布服务器，同样走 musl 静态链接。
#    注意：引入 mlua（Lua 脚本回报模式）后，依赖里含 vendored Lua C 源码，
#    musl 交叉编译必须要有 musl 交叉 C 编译器（x86_64-linux-musl-gcc /
#    musl-gcc），探测到后自动设置 CC/AR 环境变量供 cc crate 使用；
#    找不到则中止并提示安装（见 docs/构建调试手册.md 第 2 节）。
#    链接仍走 rust-lld 自包含（musl-gcc 编出的 C 目标文件可直接参与链接）。
# 2. 构建前端（tauri build 的 beforeBuildCommand 执行本脚本，代替默认的 npm run build）。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$target = "x86_64-unknown-linux-musl"
$linuxServerBin = Join-Path $root "target\$target\release\simx-server"
$updateServerBin = Join-Path $root "target\$target\release\simx-update-server"
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

# ---- musl 交叉 C 编译器（mlua 的 vendored Lua C 源码必需） ----
# cc crate 编译 C 代码时按 CC_<target> 环境变量找编译器；找不到会退回宿主
# 编译器（Windows 上是 MSVC cl.exe，产出 COFF 而非 musl ELF，必然链接失败），
# 所以这里必须显式探测并设置。探测顺序（两级）：
# 1. musl 交叉 gcc（x86_64-linux-musl-gcc / musl-gcc）——注意 musl.cc 上发布的
#    工具链是 Linux 宿主的（ELF 二进制，Windows 上跑不了），装了也探测不到；
# 2. zig（Windows 原生运行）：`zig cc -target x86_64-linux-musl` 充当交叉
#    C 编译器（单文件安装，见构建调试手册第 2 节）。
# 用户已手动设置时（自定义工具链路径）优先尊重。
$muslGcc = $null
foreach ($c in @("x86_64-linux-musl-gcc", "musl-gcc")) {
    $cmd = Get-Command $c -ErrorAction SilentlyContinue
    if ($cmd) { $muslGcc = $cmd.Source; break }
}
if ($env:CC_x86_64_unknown_linux_musl) {
    Write-Host "C 交叉编译器: 使用已有环境变量 $($env:CC_x86_64_unknown_linux_musl)"
} elseif ($muslGcc) {
    $env:CC_x86_64_unknown_linux_musl = $muslGcc
    # 对应的 ar（musl.cc 工具链带 x86_64-linux-musl-ar；Debian musl-tools 无独立
    # ar，cc crate 会用 gcc-ar 兜底，可不设）
    $muslAr = Get-Command "x86_64-linux-musl-ar" -ErrorAction SilentlyContinue
    if ($muslAr) { $env:AR_x86_64_unknown_linux_musl = $muslAr.Source }
    Write-Host "C 交叉编译器: $muslGcc（已设置 CC/AR 环境变量，供 Lua C 源码编译）"
} else {
    # zig 分支：先找 PATH 里的 zig，找不到再看默认安装位置 C:\zig\zig-*\zig.exe
    # （手册推荐的解压位置；用户还没把 zig 加进 PATH 时打包照样能跑）
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
    if ($zigExe) {
        # zig 的目标解析器不认 cc-rs 追加的 Rust 三元组 --target=x86_64-unknown-linux-musl
        # （只认 x86_64-linux-musl），所以不直接把 "zig cc -target ..." 塞给 CC，
        # 而是走 scripts/musl-cc/ 下的参数翻译 shim（过滤 --target= 后转调 zig，
        # 需要 node——前端构建链本来就有）
        $shim = Join-Path $PSScriptRoot "musl-cc\musl-zig-cc.cmd"
        $arShim = Join-Path $PSScriptRoot "musl-cc\musl-zig-ar.cmd"
        if (-not (Test-Path $shim)) {
            Write-Host "未找到 zig cc 参数翻译 shim：$shim" -ForegroundColor Red
            exit 1
        }
        # shim 优先用 ZIG_EXE 指定的 zig 路径（不依赖 zig 已加入 PATH）
        $env:ZIG_EXE = $zigExe
        $env:CC_x86_64_unknown_linux_musl = $shim
        $env:AR_x86_64_unknown_linux_musl = $arShim
        Write-Host "C 交叉编译器: $zigExe（经 musl-zig-cc 参数翻译 shim，供 Lua C 源码编译）"
    } else {
        Write-Host @"

未找到可用的 musl 交叉 C 编译器。
"Lua 脚本回报"功能引入的 mlua 需要编译 vendored Lua C 源码，
musl 交叉编译（Linux 后端）因此必须安装 zig（单文件即用，Windows 原生运行）：
  下载 https://ziglang.org/download/ 的 Windows x86_64 版，解压到 C:\zig
  （或任意目录）并把目录加入 PATH；本脚本也会自动探测 C:\zig\zig-*\zig.exe。
  注意：musl.cc 上的工具链是 Linux 宿主版（ELF 二进制），Windows 下不可用。
详见 docs/构建调试手册.md 第 2 节环境准备。
"@ -ForegroundColor Red
        exit 1
    }
}

# 决定链接方式（关键：避免每次打包都先“报错→重试”）：
# - rust-lld 自包含链接（Rust 自带链接器，rustc 1.82+ 无需额外安装），
#   musl-gcc 编出的 C 目标文件可直接参与链接；
# - 装了 musl 交叉工具链时也可选常规 gcc 链接（首次探测优先 musl-gcc）。
# 首次探测后把结果记到标记文件，之后打包直接沿用，不再重复探测。
$mode = if (Test-Path $marker) { (Get-Content $marker).Trim() } else { "" }
if ($mode -eq "") {
    $mode = if ($env:CC_x86_64_unknown_linux_musl -and (Split-Path -Leaf $env:CC_x86_64_unknown_linux_musl) -eq "x86_64-linux-musl-gcc") { "musl-gcc" } else { "rust-lld" }
    New-Item -ItemType Directory -Force -Path (Split-Path $marker) | Out-Null
    Set-Content -Path $marker -Value $mode -NoNewline -Encoding ascii
    Write-Host "链接方式: $mode（已记录到 $marker，之后打包沿用）"
}

# 始终执行 cargo build：由 cargo 按源码编译指纹自行判断是否需要重编/重链。
# 不能“产物存在就跳过”——那样改了 simx-core/simx-server/simx-update-server 代码后，
# 打包进安装包 / 发布到服务器的仍是旧版。cargo 的指纹判断很快：代码没变时这一步接近秒过。
# 一次编译两个 Linux 二进制（simx-server + simx-update-server），共享依赖编译。
if ($mode -eq "rust-lld") {
    $env:RUSTFLAGS = "-C linker=rust-lld -C link-self-contained=yes"
}
cargo build --release -p simx-server -p simx-update-server --target $target
if ($LASTEXITCODE -ne 0) {
    if ($mode -eq "musl-gcc") {
        # 装了工具链但链接仍失败（版本不匹配等）：退回 rust-lld 并更新标记
        Write-Host "常规链接失败，改用 rust-lld 自包含链接重试..."
        Set-Content -Path $marker -Value "rust-lld" -NoNewline -Encoding ascii
        $env:RUSTFLAGS = "-C linker=rust-lld -C link-self-contained=yes"
        cargo build --release -p simx-server -p simx-update-server --target $target
        if ($LASTEXITCODE -ne 0) {
            Write-Host "Linux 后端编译失败，请检查 musl 交叉编译工具链（或确认 rustc 版本 >= 1.82 以使用 rust-lld）" -ForegroundColor Red
            exit 1
        }
    } else {
        Write-Host "rust-lld 链接失败（rustc 1.82+ 自带 rust-lld；版本过旧请先 rustup update）" -ForegroundColor Red
        exit 1
    }
}

# 产物校验：两个二进制都必须存在，缺一个即视为失败（simx-server 要进安装包，
# simx-update-server 是发布服务器用的，缺一不可提醒到）
$allOk = $true
foreach ($b in @($linuxServerBin, $updateServerBin)) {
    if (Test-Path $b) {
        Write-Host "Linux 后端就绪: $b"
    } else {
        Write-Host "缺少产物: $b" -ForegroundColor Red
        $allOk = $false
    }
}
if (-not $allOk) { exit 1 }

Write-Host "== 构建前端 =="
Push-Location $root
npm run build
$code = $LASTEXITCODE
Pop-Location
exit $code
