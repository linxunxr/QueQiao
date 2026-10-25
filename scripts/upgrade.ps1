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
# 随包脚本补齐：客户端"从 Node 版迁移"按钮运行时硬依赖 migrate-from-node.ps1
# （install.ps1 新装会复制，升级链此前只换 exe——存量升级目录一直缺，
# 2026-10-24 真机：点迁移按钮即报"未找到迁移脚本"）。发布包/源码两种布局
# 仓库根都带此文件，统一补齐；ps1 无运行锁，直拷即可。
$migrateScript = Join-Path $RepoRoot 'migrate-from-node.ps1'
if (Test-Path $migrateScript) { Copy-Item $migrateScript $Dist -Force }
# 脚本自身也随升级更新：安装目录 scripts\ 下有一份副本，清单不含自己则
# 手动升级链每次执行的都是旧副本、新逻辑（含上面的补齐）永远不生效
# （v0.4.4 真机部署抓到）。zip 布局执行的是包内脚本、目标是安装目录另一
# 份文件无锁；源码布局 InstallDir 指回仓库时源=目标，try 兜底跳过即可。
$srcUpgrade = Join-Path $RepoRoot 'scripts\upgrade.ps1'
if (Test-Path $srcUpgrade) {
    New-Item -ItemType Directory -Force -Path (Join-Path $Dist 'scripts') | Out-Null
    try { Copy-Item $srcUpgrade (Join-Path $Dist 'scripts\upgrade.ps1') -Force } catch { }
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

# ── SnowLuma 迁入安装目录（A 档授权："与 Node.js 一同部署到客户端目录"）──
# 存量用户 homeDir 可能指向安装目录之外的散落位置（旧安装习惯/独立安装），
# 统一迁到 <InstallDir>\SnowLuma，启动/探测/数据都归拢在安装目录。
$slCfg = Join-Path $Dist 'config.json'
if (Test-Path $slCfg) {
    try {
        $j = Get-Content $slCfg -Raw -Encoding UTF8 | ConvertFrom-Json
        $hd = if ($j.snowluma) { [string]$j.snowluma.homeDir } else { '' }
        $target = Join-Path $Dist 'SnowLuma'
        if ($hd -and (Test-Path (Join-Path $hd 'launcher.bat')) -and `
            (-not (Resolve-Path $hd).Path.StartsWith((Resolve-Path $Dist).Path, [StringComparison]::OrdinalIgnoreCase))) {
            Write-Host "`n== SnowLuma 迁入安装目录 ==" -ForegroundColor Cyan
            Write-Host "  检测到 homeDir 指向安装目录之外：$hd"
            if (Test-Path (Join-Path $target 'launcher.bat')) {
                Write-Host "  安装目录下已有 SnowLuma——仅更新 config.homeDir 指向它（旧目录不动）" -ForegroundColor Yellow
                $j.snowluma.homeDir = $target
                $j | ConvertTo-Json -Depth 20 | Out-File $slCfg -Encoding utf8
            } else {
                $answer = Read-Host '  迁移过来？（回车 = 迁；n = 保持原位）'
                $mv = if ($null -ne $answer) { "$answer".Trim() } else { '' }
                if ($mv -notmatch '^[nN]') {
                    # 该 SnowLuma 正在运行会锁目录——先停（A 档覆盖启动；停止限本目录迁移场景自用）。
                    Get-CimInstance Win32_Process -Filter "Name='node.exe'" |
                        Where-Object { $_.CommandLine -like "*$hd*" -and $_.CommandLine -like '*index.mjs*' } |
                        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
                    Start-Sleep 2
                    Move-Item $hd $target
                    $j.snowluma.homeDir = $target
                    $j | ConvertTo-Json -Depth 20 | Out-File $slCfg -Encoding utf8
                    Write-Host "  已迁入：$target（config.homeDir 已更新）" -ForegroundColor Green
                } else {
                    Write-Host "  保持原位（$hd）" -ForegroundColor Yellow
                }
            }
        }
    } catch { Write-Host "  SnowLuma 迁移检测跳过：$($_.Exception.Message)" -ForegroundColor DarkGray }
}

Write-Host '== 3/4 DSH MCP 注册 ==' -ForegroundColor Cyan

# MCP 注册（web + desktop 两档都写——DSH 桌面版读 desktop、CLI 读 web，
# 漏写 desktop 时 AI 会话挂不到 queqiao-mcp，2026-10-25 真机总根因）。
# 幂等三步：清 queqiao/qq-bridge 托管块 → 清无标记的 Node 版 snowluma 两块
# （保留用户自配的 web-search-safe 等）→ 追加新托管块。
function Write-McpPatch {
    param($DshHome, $ProfileName, $McpExe)
    $dir = Join-Path $DshHome "profiles\$ProfileName"
    $PatchFile = Join-Path $dir 'cordis.patch.yml'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    if (-not (Test-Path $PatchFile)) { New-Item -ItemType File -Path $PatchFile -Force | Out-Null }
    $block = @"
# === queqiao MCP BEGIN ===
# 由 install.ps1 / scripts/upgrade.ps1 维护。
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
    $existing = if ((Get-Item $PatchFile).Length -gt 0) { Get-Content $PatchFile -Raw -Encoding UTF8 } else { '' }
    $existing = $existing -replace '(?s)# === (qq-bridge|queqiao) MCP BEGIN ===.*?# === (qq-bridge|queqiao) MCP END ===\r?\n?', ''
    $existing = $existing -replace '(?ms)^-\s*insert:\s*\r?\n\s*-\s*id:\s*mcp-snowluma(-host)?\s*\r?\n.*?(?=^\S|\z)', ''
    if ($existing.Trim()) {
        Set-Content $PatchFile ($existing.TrimEnd() + "`n`n" + $block) -Encoding UTF8 -NoNewline
    } else {
        Set-Content $PatchFile $block -Encoding UTF8 -NoNewline
    }
    Write-Host '  ' + "profiles\$ProfileName\cordis.patch.yml 已注册 queqiao-mcp（清 Node 旧块，保留其余条目）"
}
Write-McpPatch $DshHome 'web' $McpExe
Write-McpPatch $DshHome 'desktop' $McpExe

Write-Host '== 4/4 启动步骤 ==' -ForegroundColor Cyan
# 升级时若程序在跑：旧进程仍映射旧文件（已转 .old）——退出重开才加载新版。
# 重启恢复升级前状态：客户端在跑 → 重启客户端（是否带桥经一次性标记文件
# 传递，桥被手动停了就不拉）；只有桥在跑 → 只重启桥。
$appRunning = [bool](Get-CimInstance Win32_Process -Filter "Name='queqiao-app.exe'" -ErrorAction SilentlyContinue)
$bridgeRunning = [bool](Get-CimInstance Win32_Process -Filter "Name='queqiao.exe'" -ErrorAction SilentlyContinue)
$running = $appRunning -or $bridgeRunning
$appExe = Join-Path $Dist 'queqiao-app.exe'
$bridgeExe = Join-Path $Dist 'queqiao.exe'
if ($running) {
    Write-Host "`n  检测到鹊桥正在运行（客户端:$appRunning 桥:$bridgeRunning）——退出重开才加载新版。" -ForegroundColor Yellow
    $answer = Read-Host '  现在自动重启鹊桥？（回车 = 是；n = 稍后自己从托盘退出再开）'
    $go = if ($null -ne $answer) { "$answer".Trim() } else { '' }
    if ($go -notmatch '^[nN]') {
        Get-CimInstance Win32_Process -Filter "Name='queqiao-app.exe' OR Name='queqiao.exe'" -ErrorAction SilentlyContinue |
            ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
        Start-Sleep -Seconds 2
        if ($appRunning) {
            # 一次性标记：客户端启动读后即删——桥是否带起按升级前状态。
            $bridgeFlag = if ($bridgeRunning) { 'true' } else { 'false' }
            try {
                $stateDir = Join-Path $Dist 'state'
                New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
                Set-Content (Join-Path $stateDir 'bridge-autostart') $bridgeFlag -Encoding ASCII
            } catch { }
            $restartNote = if ($bridgeRunning) { '桥一并恢复' } else { '桥保持停止，需要时一键启动' }
            if (Test-Path $appExe) {
                try {
                    Start-Process $appExe -WorkingDirectory $Dist
                    Write-Host "  已重启新版客户端（$restartNote）：$appExe" -ForegroundColor Green
                } catch {
                    # 进程已退出而启动失败——不能让用户面前全停，明确给出手动路径。
                    Write-Host "  启动失败（$($_.Exception.Message)）" -ForegroundColor Red
                    Write-Host "  请手动启动：cd $Dist ; .\queqiao-app.exe" -ForegroundColor Yellow
                }
            } else {
                Write-Host "  $Dist 无 queqiao-app.exe——手动启动：cd $Dist ; .\queqiao.exe" -ForegroundColor Yellow
            }
        } elseif ($bridgeRunning -and (Test-Path $bridgeExe)) {
            # 只有桥在跑（无客户端）：恢复桥本身即可。
            try {
                Start-Process $bridgeExe -WorkingDirectory $Dist
                Write-Host "  已重启新版桥：$bridgeExe" -ForegroundColor Green
            } catch {
                Write-Host "  桥启动失败（$($_.Exception.Message)）——手动：cd $Dist ; .\queqiao.exe" -ForegroundColor Red
            }
        }
    } else {
        Write-Host '  跳过重启——旧窗口仍是旧代码；托盘退出（点 × 只隐藏不退）后从桌面快捷方式重开。' -ForegroundColor Yellow
    }
}
Write-Host @"
  1. 停 Node 版桥（结束 node bridge.js 或其窗口）
  2. 启动：cd $Dist ; .\queqiao.exe
     （或保持 target/release 调试：QUEQIAO_HOME 指向 $Dist 时 exe 任意放）
  3. 重启 DSH（preset 变更生效）——桌面端退出重开，或 CLI 重跑 dsh web
  4. 在白名单群/私聊发消息，观察 dist 运行窗口的决策日志与 QQ 回复
"@ -ForegroundColor Green
Write-Host '注意：QUEQIAO_HOME 环境变量可覆盖 home（默认=exe 所在目录）。' -ForegroundColor DarkGray
