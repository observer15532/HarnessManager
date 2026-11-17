# End-to-end smoke test.
#
# Runs the launcher against a throwaway APPDATA and DSH_HOME on a spare port, so
# the user's real config, sessions and running dsh are untouched.
#
#   powershell -File dev\smoke.ps1
#   powershell -File dev\smoke.ps1 -Port 3099 -Exe ..\dist\HarnessManager.exe

param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\dist\HarnessManager.exe'),
    [int]$Port = 3099
)

$ErrorActionPreference = 'Stop'

# The single-instance mutex is global per session, so a running launcher would
# make the test instance exit immediately.
$running = @(Get-Process HarnessManager -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
    throw "已有一个 HarnessManager 在运行（pid $($running[0].Id)）。先退出它，再跑这个测试。"
}

$Exe = (Resolve-Path $Exe).Path
$root = Join-Path $env:TEMP 'HarnessManager-smoke'
$appdata = Join-Path $root 'appdata'
$dshHome = Join-Path $root 'home'

$packageDir = Join-Path $env:APPDATA 'npm\node_modules\@deepseek-ai\dsh'
if (-not (Test-Path (Join-Path $packageDir 'lib\bin.js'))) {
    throw "找不到已安装的 dsh：$packageDir"
}

if (Test-Path $root) { Remove-Item $root -Recurse -Force }
New-Item -ItemType Directory -Path (Join-Path $appdata 'HarnessManager') -Force | Out-Null
New-Item -ItemType Directory -Path $dshHome -Force | Out-Null

$config = @"
port = $Port
channel = "next"
dsh_home = "$($dshHome -replace '\\','/')"
dsh_dir = "$($packageDir -replace '\\','/')"
start_on_launch = true
current = "web"

[[profiles]]
name = "web"
cwd = "$($PWD.Path -replace '\\','/')"
"@
$configPath = Join-Path $appdata 'HarnessManager\config.toml'
[System.IO.File]::WriteAllText($configPath, $config, (New-Object System.Text.UTF8Encoding($false)))

$env:APPDATA = $appdata

function Test-Port([int]$candidate) {
    try {
        $client = New-Object System.Net.Sockets.TcpClient
        $client.Connect('127.0.0.1', $candidate)
        $client.Close()
        return $true
    } catch { return $false }
}

$proc = Start-Process -FilePath $Exe -PassThru
$ready = $false
for ($i = 0; $i -lt 45; $i++) {
    Start-Sleep -Milliseconds 1000
    if ($proc.HasExited) { break }
    if (Test-Port $Port) { $ready = $true; break }
}

$failures = @()
if ($proc.HasExited) { $failures += "launcher 提前退出" }
if (-not $ready) { $failures += "端口 $Port 未监听" }

$children = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($proc.Id)" |
    Where-Object { $_.Name -eq 'node.exe' })
if ($children.Count -eq 0) { $failures += "没有 dsh 子进程" }

$logs = @(Get-ChildItem (Join-Path $appdata 'HarnessManager\logs') -Filter 'dsh-*.log' -ErrorAction SilentlyContinue)
if ($logs.Count -eq 0) {
    $failures += "没有 dsh 日志"
} else {
    $urlLine = Select-String -Path $logs[0].FullName -Pattern 'dsh web: http' -SimpleMatch |
        Select-Object -First 1
    if (-not $urlLine) { $failures += "日志里没有 dsh web: URL 行" }
    if ($urlLine -and $urlLine.Line -notmatch '<redacted>') { $failures += "日志里的 token 没有打码" }
}

# Crash safety: killing only the launcher must take the tree with it.
Stop-Process -Id $proc.Id -Force
Start-Sleep -Seconds 5
if (Test-Port $Port) { $failures += "launcher 终止后端口仍被占用" }
$orphans = @(Get-CimInstance Win32_Process -Filter "Name='node.exe'" |
    Where-Object { $_.CommandLine -like "*--port $Port*" })
if ($orphans.Count -gt 0) { $failures += "留下 $($orphans.Count) 个孤儿 node" }

if ($failures.Count -eq 0) {
    "PASS  (launcher -> dsh on :$Port, url parsed, tree cleaned up)"
    exit 0
}
"FAIL"
$failures | ForEach-Object { "  - $_" }
foreach ($log in $logs) { "--- $($log.Name) ---"; Get-Content $log.FullName -Tail 10 }
exit 1

