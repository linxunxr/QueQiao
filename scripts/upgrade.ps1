# 鹊桥（QueQiao）升级部署脚本：构建 → dist 布局 → DSH MCP 注册
# 用法：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/upgrade.ps1 `
#       -NodeBridge <已有 Node 版桥仓库路径>      # 可选：迁移 config/state
#   （或设环境变量 QUEQIAO_NODE_BRIDGE 代替 -NodeBridge）
# 前提：DSH 已安装且跑过一次（~/.dsh/profiles 存在）；SnowLuma 已登录。
#
# 做四件事（NodeBridge 参数见 param 块）：
#   1. cargo build --release
#   2. dist/ 布局：两个 exe + 从 Node 版桥复制 config.json 与 state/（首次、可选）
#   3. 生成 DSH MCP 注册片段（~/.dsh/profiles/<PROFILE>/cordis.patch.yml 的
#      既有托管块替换为指向 queqiao-mcp.exe 的条目）
#   4. 打印启动步骤
#
# ⚠️ 切换前先停旧桥（3100 端口冲突 + 双桥并行唤醒同一会话会双重回复）。

param(
    # 鹊桥安装目录（与 install.ps1 的 -InstallDir 一致）。未传时交互式询问
    # （回车采用缺省 %LOCALAPPDATA%\QueQiao）；已有安装目录存在 exe 时直接建议采用。
    [string]$InstallDir = '',
    # Node 版桥仓库路径（首次部署复制 config.json/state/ 用；缺省取环境变量
    # QUEQIAO_NODE_BRIDGE——没有则跳过复制，由使用者自备 config.json 后再跑）。
    [string]$NodeBridge = $env:QUEQIAO_NODE_BRIDGE
)
$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
$DshHome = if ($env:DSH_HOME) { $env:DSH_HOME } else { Join-Path $env:USERPROFILE '.dsh' }
$Profile = if ($env:DSH_PROFILE) { $env:DSH_PROFILE } else { 'web' }

# 已有安装探测：升级的本职是找到现役安装，不该让用户重输路径。四级链：
# 注册表登记（新版 install.ps1 写入）→ 运行中进程路径 → 桌面快捷方式 →
# 常见位置；候选须含 queqiao.exe 才认；都无才交互询问。
function Find-QueqiaoInstall {
    $candidates = @()
    try {
        $reg = Get-ItemProperty 'HKCU:\Software\QueQiao' -ErrorAction Stop
        if ($reg.InstallDir) { $candidates += $reg.InstallDir }
    } catch { }
    $proc = Get-CimInstance Win32_Process -Filter "Name='queqiao.exe' OR Name='queqiao-app.exe'" -ErrorAction SilentlyContinue |
        Select-Object -ExpandProperty ExecutablePath -First 1
    if ($proc) { $candidates += (Split-Path -Parent $proc) }
    $lnk = Join-Path ([Environment]::GetFolderPath('Desktop')) '鹊桥 QueQiao.lnk'
    if (Test-Path $lnk) {
        try {
            $target = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk).TargetPath
            if ($target) { $candidates += (Split-Path -Parent $target) }
        } catch { }
    }
    $candidates += "$env:LOCALAPPDATA\QueQiao"
    foreach ($dir in $candidates) {
        if ($dir -and (Test-Path (Join-Path $dir 'queqiao.exe'))) { return $dir }
    }
    return $null
}

# 布局检测（与 install.ps1 同款）：发布包（zip 根自带构建好的 exe）直接
# 复制、无需 Rust 工具链；源码布局（git clone）才本地构建。
$shipped = @('queqiao.exe', 'queqiao-mcp.exe', 'queqiao-app.exe') |
    Where-Object { Test-Path (Join-Path $RepoRoot $_) }
Write-Host '== 1/4 release 构建 ==' -ForegroundColor Cyan
if ($shipped.Count -ge 2) {
    Write-Host "  发布包自带可执行文件（$(@($shipped).Count) 个）——跳过构建，无需 Rust 工具链" -ForegroundColor Green
} else {
    Push-Location $RepoRoot
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw '构建失败' }
    Pop-Location
}

Write-Host '== 2/4 dist 布局 ==' -ForegroundColor Cyan
$defaultDir = "$env:LOCALAPPDATA\QueQiao"
if (-not $InstallDir) {
    $found = Find-QueqiaoInstall
    if ($found) {
        $InstallDir = $found
        Write-Host "  已检测到现有安装：$InstallDir" -ForegroundColor Green
        Write-Host '  （直接升级；如需换目录，重跑并指定 -InstallDir）' -ForegroundColor DarkGray
    } else {
        $answer = Read-Host "  安装目录（回车 = [$defaultDir]，或输入已有安装路径）"
        # PS5.1：Read-Host 在 stdin 关闭/非交互时返回 $null——防 .Trim() 崩溃。
        $trimmed = if ($null -ne $answer) { "$answer".Trim() } else { '' }
        $InstallDir = if ($trimmed) { $trimmed } else { $defaultDir }
    }
}
$Dist = $InstallDir
New-Item -ItemType Directory -Force -Path $Dist | Out-Null

# exe 部署：目标正被运行（客户端/桥/mcp 进程在跑）时 Copy-Item 撞写入锁。
# Windows 运行中 exe 可改名不可覆盖——先试直拷，IOException 则旧文件转
# .old 再拷新版（运行中的旧进程不受影响，退出后重开即新版；.old 仍被
# 占用时清理失败不报错，下次升级再清——与桥自动更新 apply 脚本同技巧）。
function Deploy-Exe($src, $distDir) {
    $name = Split-Path -Leaf $src
    $dst = Join-Path $distDir $name
    if (Test-Path $dst) {
        Remove-Item "$dst.old" -Force -ErrorAction SilentlyContinue
        try {
            Copy-Item $src $dst -Force -ErrorAction Stop
        } catch [System.IO.IOException] {
            Move-Item $dst "$dst.old" -Force
            Copy-Item $src $dst -Force
            Write-Host "  $name 正在运行——旧版转 .old，新版已就位（重开该程序生效）" -ForegroundColor Yellow
        }
    } else {
        Copy-Item $src $dst -Force
    }
}
if ($shipped.Count -ge 2) {
    foreach ($exe in $shipped) { Deploy-Exe (Join-Path $RepoRoot $exe) $Dist }
    $notice = Join-Path $RepoRoot 'THIRD_PARTY_NOTICES.md'
    if (Test-Path $notice) { Copy-Item $notice $Dist -Force }
} else {
    foreach ($exe in @('queqiao.exe', 'queqiao-mcp.exe', 'queqiao-app.exe')) {
        Deploy-Exe (Join-Path $RepoRoot "target\release\$exe") $Dist
    }
}
# 首次部署：从 Node 版桥带一份配置与状态（会话映射/唤醒配置沿用，无缝切换）。
if ($NodeBridge -and (Test-Path $NodeBridge)) {
    if (-not (Test-Path (Join-Path $Dist 'config.json'))) {
        Copy-Item (Join-Path $NodeBridge 'config.json') $Dist
        Write-Host '  已复制 config.json（来自 Node 版桥）' -ForegroundColor Yellow
    }
    if (-not (Test-Path (Join-Path $Dist 'state'))) {
        Copy-Item (Join-Path $NodeBridge 'state') $Dist -Recurse
        Write-Host '  已复制 state/（会话映射与持久状态沿用）' -ForegroundColor Yellow
    }
} else {
    Write-Host '  未提供 -NodeBridge（或 QUEQIAO_NODE_BRIDGE）：跳过配置迁移，' -ForegroundColor Yellow
    Write-Host '  请自备 config.json（参考仓库 config 形状）后放入 dist/ 再启动。' -ForegroundColor Yellow
}

Write-Host '== 3/4 DSH MCP 注册 ==' -ForegroundColor Cyan
$McpExe = Join-Path $Dist 'queqiao-mcp.exe'
$PatchFile = Join-Path $DshHome "profiles\$Profile\cordis.patch.yml"
if (-not (Test-Path $PatchFile) -and (Test-Path (Join-Path $DshHome 'profiles'))) {
    New-Item -ItemType File -Path $PatchFile -Force | Out-Null
}
$block = @"
# === queqiao MCP BEGIN ===
# 由 scripts/setup-dsh.ps1 维护；替换 Node 版 qq-bridge 的托管块。
- insert:
    - id: mcp-snowluma
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        serverName: snowluma
        transport: stdio
        command: '$McpExe'
        args: []
        toolCallTimeoutMs: 725000
# === queqiao MCP END ===
"@
if (Test-Path $PatchFile) {
    $existing = Get-Content $PatchFile -Raw -Encoding UTF8
    $pattern = '(?s)# === qq-bridge MCP BEGIN ===.*?# === qq-bridge MCP END ===\r?\n?'
    if ($existing -match $pattern) {
        $updated = $existing -replace $pattern, $block
        Set-Content $PatchFile $updated -Encoding UTF8 -NoNewline
        Write-Host "  已替换 Node 版托管块：$PatchFile"
    } else {
        Add-Content $PatchFile "`n$block" -Encoding UTF8
        Write-Host "  已追加 queqiao 条目：$PatchFile"
    }
} else {
    Write-Host "  未找到 $PatchFile（DSH 未初始化？跳过注册——手工把块写入该文件）" -ForegroundColor Yellow
}

Write-Host '== 4/4 启动步骤 ==' -ForegroundColor Cyan
Write-Host @"
  1. 停 Node 版桥（结束 node bridge.js 或其窗口）
  2. 启动：cd $Dist ; .\queqiao.exe
     （或保持 target/release 调试：QUEQIAO_HOME 指向 $Dist 时 exe 任意放）
  3. 重启 DSH（preset 变更生效）——桌面端退出重开，或 CLI 重跑 dsh web
  4. 在白名单群/私聊发消息，观察 dist 运行窗口的决策日志与 QQ 回复
"@ -ForegroundColor Green
Write-Host '注意：QUEQIAO_HOME 环境变量可覆盖 home（默认=exe 所在目录）。' -ForegroundColor DarkGray
