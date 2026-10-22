# 鹊桥（QueQiao）一键安装器——总入口
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File install.ps1
#
# 流程（每步先检测、已装即跳过，可反复运行）：
#   1. DSH（DeepSeek Harness）：检测 ~/.dsh；未装则打开官方渠道引导安装（DSH
#      需账号与安装向导，无法无人值守），装好后按回车继续。
#   2. SnowLuma：检测用户指定目录；未装则从官方 GitHub Releases 下载**原样
#      发行包**解压安装（A 档授权形态：仅官方渠道、不修改、不镜像）。
#   3. preset：把仓库 dsh/agent-presets/（qq-chat / qq-chat-v2）部署到
#      ~/.dsh/agent-presets/。
#   4. queqiao：构建 release + dist 布局。
#   5. config.json：交互式生成最小可用配置（已存在则跳过）。
#   6. MCP 注册：cordis.patch.yml 写入 queqiao-mcp 条目。
#
# 升级部署（已装过全部依赖）用 upgrade.bat / scripts/upgrade.ps1。
#
# ⚠️ SnowLuma 需人工完成两步（脚本无法代替）：登录 QQ、开启 OneBot 服务；
#    脚本在最后会提示并等待。

param(
    # 鹊桥安装目录（exe/config/state 的家）。未传时交互式询问（回车采用
    # 缺省 %LOCALAPPDATA%\QueQiao，或输入任意路径）。
    [string]$InstallDir = '',
    # SnowLuma 安装目录（检测与安装目标）。
    [string]$SnowLumaHome = "$env:USERPROFILE\SnowLuma",
    # Node 版桥路径（可选：迁移已有 config/state）。
    [string]$NodeBridge = $env:QUEQIAO_NODE_BRIDGE
)
$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
$DshHome = if ($env:DSH_HOME) { $env:DSH_HOME } else { Join-Path $env:USERPROFILE '.dsh' }
$Profile = if ($env:DSH_PROFILE) { $env:DSH_PROFILE } else { 'web' }

function Write-Step($n, $msg) { Write-Host "`n== $n/6 $msg ==" -ForegroundColor Cyan }
function Write-Ok($msg) { Write-Host "  $msg" -ForegroundColor Green }
function Write-Info($msg) { Write-Host "  $msg" -ForegroundColor Yellow }

# ── 1. DSH ────────────────────────────────────────────────────────────────────
Write-Step 1 'DSH（DeepSeek Harness）'
if (Test-Path $DshHome) {
    Write-Ok "已检测到 $DshHome"
} else {
    Write-Info '未检测到 DSH。请从 DeepSeek 官方渠道下载安装 DeepSeek Harness'
    Write-Info '（桌面端或 CLI 均可），启动一次完成初始化并配置模型。'
    Start-Process 'https://www.deepseek.com'
    Read-Host '  安装并初始化完成后，按回车继续（Ctrl+C 退出）'
    if (-not (Test-Path $DshHome)) { throw "仍未检测到 $DshHome——请确认 DSH 安装完成" }
    Write-Ok "已检测到 $DshHome"
}

# ── 2. SnowLuma（官方发行包原样下载安装）──────────────────────────────────────
Write-Step 2 'SnowLuma'
$slExe = Join-Path $SnowLumaHome 'index.mjs'
if ((Test-Path $SnowLumaHome) -and (Test-Path $slExe)) {
    Write-Ok "已检测到 $SnowLumaHome"
} else {
    Write-Info "未检测到 SnowLuma（$SnowLumaHome）。从官方 GitHub Releases 下载 win-x64 发行包…"
    $rel = Invoke-RestMethod 'https://api.github.com/repos/SnowLuma/SnowLuma/releases/latest'
    $asset = $rel.assets | Where-Object { $_.name -match 'win-x64(?<!lite)\.zip$' -or ($_.name -match 'win-x64' -and $_.name -notmatch 'lite') } | Select-Object -First 1
    if (-not $asset) { throw "官方 Releases 未找到 win-x64 资产（tag $($rel.tag_name)）——请手工安装后重跑" }
    Write-Info "下载 $($asset.name)（$([math]::Round($asset.size/1MB))MB，原样不修改）…"
    $zip = Join-Path $env:TEMP $asset.name
    Invoke-WebRequest $asset.browser_download_url -OutFile $zip
    New-Item -ItemType Directory -Force -Path $SnowLumaHome | Out-Null
    Expand-Archive $zip $SnowLumaHome -Force
    Remove-Item $zip
    Write-Ok "已安装到 $SnowLumaHome（$($rel.tag_name)）"
    Write-Info '接下来请启动 SnowLuma：登录目标 QQ → 设置里开启 OneBot 服务；'
    Write-Info '完成后 config/onebot_<QQ号>.json 会出现（queqiao 靠它自动发现 token）。'
    Read-Host '  按$回车继续（可稍后完成，config.json 的 snowluma.homeDir 已按此路径生成）'
}

# ── 3. preset 部署 ────────────────────────────────────────────────────────────
Write-Step 3 'DSH preset（qq-chat / qq-chat-v2）'
$presetDst = Join-Path $DshHome 'agent-presets'
New-Item -ItemType Directory -Force -Path $presetDst | Out-Null
foreach ($p in 'qq-chat', 'qq-chat-v2') {
    Copy-Item (Join-Path $RepoRoot "dsh\agent-presets\$p") $presetDst -Recurse -Force
    Write-Ok "$p → $presetDst\$p"
}

# ── 4. 构建 + dist 布局 ──────────────────────────────────────────────────────
Write-Step 4 '构建 queqiao'
Push-Location $RepoRoot
cargo build --release
if ($LASTEXITCODE -ne 0) { throw '构建失败' }
Pop-Location
$defaultDir = "$env:LOCALAPPDATA\QueQiao"
if (-not $InstallDir) {
    Write-Host ""
    Write-Host "  选择安装目录（exe、config.json、state/ 都住这里，与源码仓库无关）：" -ForegroundColor Yellow
    $answer = Read-Host "  直接回车装到缺省位置 [$defaultDir]，或输入自定义路径"
    $InstallDir = if ($answer.Trim()) { $answer.Trim() } else { $defaultDir }
}
$Dist = $InstallDir
New-Item -ItemType Directory -Force -Path $Dist | Out-Null
Write-Ok "安装目录：$Dist"
Copy-Item (Join-Path $RepoRoot 'target\release\queqiao.exe') $Dist -Force
Copy-Item (Join-Path $RepoRoot 'target\release\queqiao-mcp.exe') $Dist -Force
Copy-Item (Join-Path $RepoRoot 'THIRD_PARTY_NOTICES.md') $Dist -Force
if ($NodeBridge -and (Test-Path $NodeBridge)) {
    if (-not (Test-Path (Join-Path $Dist 'config.json'))) {
        Copy-Item (Join-Path $NodeBridge 'config.json') $Dist
        Write-Ok '已从 Node 版桥迁移 config.json'
    }
    if (-not (Test-Path (Join-Path $Dist 'state'))) {
        Copy-Item (Join-Path $NodeBridge 'state') $Dist -Recurse
        Write-Ok '已从 Node 版桥迁移 state/'
    }
} else {
    Write-Info '未提供 -NodeBridge：全新安装走第 5 步交互式生成 config.json。'
}
Write-Ok "dist 布局就绪：$Dist"

# ── 5. 交互式生成最小 config.json ─────────────────────────────────────────────
Write-Step 5 'config.json'
$cfgPath = Join-Path $Dist 'config.json'
if (Test-Path $cfgPath) {
    Write-Ok "已存在 $cfgPath（跳过生成）"
} else {
    $owner = Read-Host '  管理员 QQ 号（ownerQQ，AI 回应管理指令的对象）'
    $groups = Read-Host '  允许 AI 参与的群号（逗号分隔，可留空）'
    $privates = Read-Host '  允许 AI 私聊的 QQ 号（逗号分隔，可留空）'
    $cfg = [ordered]@{
        dsh = @{ baseUrl = 'http://127.0.0.1:3080'; model = '' }
        snowluma = @{ homeDir = $SnowLumaHome }
        ownerQQ = if ($owner) { [int64]$owner } else { $null }
        allow = @{
            groups = @($groups -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ } | ForEach-Object { [int64]$_ })
            private = @($privates -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ } | ForEach-Object { [int64]$_ })
        }
    }
    $cfg | ConvertTo-Json -Depth 5 | Out-File $cfgPath -Encoding utf8
    Write-Ok "已生成 $cfgPath（其余配置按默认值深度合并——按需参考仓库 README 修订）"
}

# ── 6. DSH MCP 注册 ──────────────────────────────────────────────────────────
Write-Step 6 'DSH MCP 注册'
$McpExe = Join-Path $Dist 'queqiao-mcp.exe'
$PatchFile = Join-Path $DshHome "profiles\$Profile\cordis.patch.yml"
if (-not (Test-Path (Split-Path $PatchFile))) {
    New-Item -ItemType Directory -Force -Path (Split-Path $PatchFile) | Out-Null
    New-Item -ItemType File -Path $PatchFile -Force | Out-Null
}
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
if ((Test-Path $PatchFile) -and (Get-Item $PatchFile).Length -gt 0) {
    $existing = Get-Content $PatchFile -Raw -Encoding UTF8
    $pattern = '(?s)# === (qq-bridge|queqiao) MCP BEGIN ===.*?# === (qq-bridge|queqiao) MCP END ===\r?\n?'
    if ($existing -match $pattern) {
        Set-Content $PatchFile ($existing -replace $pattern, $block) -Encoding UTF8 -NoNewline
        Write-Ok "已替换旧托管块：$PatchFile"
    } else {
        Add-Content $PatchFile "`n$block" -Encoding UTF8
        Write-Ok "已追加条目：$PatchFile"
    }
} else {
    Set-Content $PatchFile $block -Encoding UTF8 -NoNewline
    Write-Ok "已写入：$PatchFile"
}

Write-Host @"

安装完成。启动步骤：
  1. 确认 SnowLuma 已登录 QQ 且开启 OneBot 服务
  2. 启动桥：cd $Dist ; .\queqiao.exe
  3. 重启 DSH（preset 与 MCP 注册生效）
  4. 在白名单群/私聊发消息，观察 dist 窗口的决策日志与 AI 回复

"@ -ForegroundColor Green
