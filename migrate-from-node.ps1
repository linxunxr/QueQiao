# 鹊桥（QueQiao）从 Node 版桥（qq-bridge）迁移数据——双击或命令行运行。
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File migrate-from-node.ps1 `
#           -NodeBridge D:\path\to\qq-bridge [-InstallDir D:\path\to\QueQiao]
#
# 迁移内容（只迁用户数据，不迁运行时垃圾）：
#   config.json        主配置（allow 白名单/ownerQQ/snowluma.homeDir 等全部带上）
#   state/ 下列文件：
#     sessions.json      QQ 会话 → DSH sessionId 映射（无缝续用现有 DSH 会话）
#     social-v2.json     每会话状态（唤醒配置/消息缓冲/频率时间戳/记忆条目）
#     diary.jsonl        AI 日记
#     stickers.json      表情包收藏
#     slang.json         黑话词典
#     token-usage.jsonl  花费记账
#     current-role.json  当前人格卡
#     mode.json          运行模式
#   roles/               人格卡（<名称>.md 全部）
# 不迁：*.log、*.lock、console-token（令牌换新）、agents/（会话工作目录）、
#       slang-agent/、*-shots/、feedback.json（反馈已处理记录）。
#
# 安全：目标已存在的同名文件默认跳过（-Force 覆盖）；迁移前打印清单逐项确认。

param(
    # Node 版桥仓库根（含 config.json 与 state/）。
    [Parameter(Mandatory = $true)][string]$NodeBridge,
    # 鹊桥安装目录（缺省 %LOCALAPPDATA%\QueQiao）。
    [string]$InstallDir = "$env:LOCALAPPDATA\QueQiao",
    # 覆盖目标已存在文件。
    [switch]$Force
)
$ErrorActionPreference = 'Stop'
function Write-Ok($m) { Write-Host "  $m" -ForegroundColor Green }
function Write-Info($m) { Write-Host "  $m" -ForegroundColor Yellow }

if (-not (Test-Path (Join-Path $NodeBridge 'config.json'))) {
    throw "未找到 $NodeBridge\config.json——请确认 -NodeBridge 指向 Node 版桥仓库根"
}
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $InstallDir 'state') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $InstallDir 'roles') | Out-Null

$copied = 0; $skipped = 0
function Copy-One($src, $dst) {
    if (-not (Test-Path $src)) { return }
    if ((Test-Path $dst) -and -not $Force) {
        $script:skipped++
        Write-Info "跳过（已存在）：$(Split-Path $dst -Leaf)"
    } else {
        Copy-Item $src $dst -Force
        $script:copied++
        Write-Ok "$(Split-Path $src -Leaf) -> $dst"
    }
}

Write-Host "== 迁移 Node 版桥数据 ==" -ForegroundColor Cyan
Write-Info "源：$NodeBridge"
Write-Info "目标：$InstallDir"

# config.json（整份带走——鹊桥深度合并默认，多出的键无副作用）
Copy-One (Join-Path $NodeBridge 'config.json') (Join-Path $InstallDir 'config.json')

# state/ 用户数据白名单
$stateFiles = @(
    'sessions.json', 'social-v2.json', 'diary.jsonl', 'stickers.json',
    'slang.json', 'token-usage.jsonl', 'current-role.json', 'mode.json'
)
foreach ($f in $stateFiles) {
    Copy-One (Join-Path $NodeBridge "state\$f") (Join-Path $InstallDir "state\$f")
}

# roles/ 人格卡
Get-ChildItem (Join-Path $NodeBridge 'roles') -Filter *.md -ErrorAction SilentlyContinue | ForEach-Object {
    Copy-One $_.FullName (Join-Path $InstallDir "roles\$($_.Name)")
}

Write-Host ""
Write-Host "迁移完成：$copied 项复制，$skipped 项跳过。" -ForegroundColor Cyan
if ($skipped -gt 0) { Write-Info '跳过项如需覆盖请加 -Force 重跑。' }
Write-Host "启动鹊桥（queqiao.exe / queqiao-app.exe）即沿用全部会话与人格。" -ForegroundColor Green
