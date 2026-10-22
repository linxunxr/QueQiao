# 鹊桥（QueQiao）部署脚本：构建 → dist 布局 → DSH preset 注册
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/setup-dsh.ps1
# 前提：DSH 已安装且跑过一次（~/.dsh/profiles 存在）；SnowLuma 已登录。
#
# 做四件事：
#   1. cargo build --release
#   2. dist/ 布局：两个 exe + 从 Node 版桥复制 config.json 与 state/（首次）
#   3. 生成 DSH MCP 注册片段（~/.dsh/profiles/<PROFILE>/cordis.patch.yml 的
#      qq-bridge 托管块替换为指向 queqiao-mcp.exe 的条目）
#   4. 打印启动步骤
#
# ⚠️ 切换前先停 Node 版桥（3100 端口冲突 + 双桥并行唤醒同一会话会双重回复）。

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
$DshHome = if ($env:DSH_HOME) { $env:DSH_HOME } else { Join-Path $env:USERPROFILE '.dsh' }
$Profile = if ($env:DSH_PROFILE) { $env:DSH_PROFILE } else { 'web' }
$NodeBridge = 'D:\100work\103Tools\qq-bridge'   # Node 版桥（复制 config/state 用）

Write-Host '== 1/4 release 构建 ==' -ForegroundColor Cyan
Push-Location $RepoRoot
cargo build --release
if ($LASTEXITCODE -ne 0) { throw '构建失败' }
Pop-Location

Write-Host '== 2/4 dist 布局 ==' -ForegroundColor Cyan
$Dist = Join-Path $RepoRoot 'dist'
New-Item -ItemType Directory -Force -Path $Dist | Out-Null
Copy-Item (Join-Path $RepoRoot 'target\release\queqiao.exe') $Dist -Force
Copy-Item (Join-Path $RepoRoot 'target\release\queqiao-mcp.exe') $Dist -Force
# 首次部署：从 Node 版桥带一份配置与状态（会话映射/唤醒配置沿用，无缝切换）。
if (-not (Test-Path (Join-Path $Dist 'config.json'))) {
    Copy-Item (Join-Path $NodeBridge 'config.json') $Dist
    Write-Host '  已复制 config.json（来自 Node 版）' -ForegroundColor Yellow
}
if (-not (Test-Path (Join-Path $Dist 'state'))) {
    Copy-Item (Join-Path $NodeBridge 'state') $Dist -Recurse
    Write-Host '  已复制 state/（会话映射与持久状态沿用）' -ForegroundColor Yellow
}

Write-Host '== 3/4 DSH MCP 注册 ==' -ForegroundColor Cyan
$McpExe = Join-Path $Dist 'queqiao-mcp.exe'
$PatchFile = Join-Path $DshHome "profiles\$Profile\cordis.patch.yml"
if (-not (Test-Path (PatchFile)) -and (Test-Path (Join-Path $DshHome 'profiles'))) {
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
