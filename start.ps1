# QUANT TERMINAL 啟動器
#
# 一次啟動後端與儀表板。
#
# 為什麼需要這個:儀表板是 file:// 網頁,基於瀏覽器安全模型,網頁無法自行啟動
# 本機程序 —— 那等於任何網站都能在你電腦上執行程式。所以由這個腳本代勞。
#
# 用法:在檔案總管對 start.cmd 按兩下,或執行:
#   powershell -ExecutionPolicy Bypass -File start.ps1

$ErrorActionPreference = "Stop"
$root    = Split-Path -Parent $MyInvocation.MyCommand.Path
$backend = Join-Path $root "rust-backend"
$exe     = Join-Path $backend "target\release\quant-terminal-backend.exe"
$dash    = Join-Path $root "crypto-bot-dashboard.html"

function Info($m){ Write-Host "[QUANT] $m" -ForegroundColor Cyan }
function Warn($m){ Write-Host "[QUANT] $m" -ForegroundColor Yellow }
function Err ($m){ Write-Host "[QUANT] $m" -ForegroundColor Red }

# ---- 1. 後端是否已在執行? --------------------------------------------
$running = $false
try {
    $h = Invoke-RestMethod -Uri "http://127.0.0.1:8787/health" -TimeoutSec 2
    $running = $true
    Info "後端已在執行中。"
} catch { }

if (-not $running) {
    # ---- 2. 需要時才編譯 ---------------------------------------------
    if (-not (Test-Path $exe)) {
        Warn "找不到執行檔,開始編譯(第一次約需 1-2 分鐘)…"
        if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
            Err "找不到 cargo。請先安裝 Rust:"
            Err "  winget install --id Rustlang.Rustup -e"
            Err "  winget install --id BrechtSanders.WinLibs.POSIX.UCRT -e"
            Err "  rustup default stable-x86_64-pc-windows-gnu"
            Read-Host "`n按 Enter 結束"
            exit 1
        }
        Push-Location $backend
        & cargo build --release
        $failed = -not $?
        Pop-Location
        if ($failed -or -not (Test-Path $exe)) {
            Err "編譯失敗。請到 rust-backend 執行 'cargo build --release' 查看完整錯誤。"
            Read-Host "`n按 Enter 結束"
            exit 1
        }
    }

    # ---- 3. 首次執行時建立 .env --------------------------------------
    $envFile = Join-Path $backend ".env"
    if (-not (Test-Path $envFile)) {
        Copy-Item (Join-Path $backend ".env.example") $envFile
        Info "已從範本建立 .env(金鑰留空,實盤鎖關閉)。"
    }

    # ---- 4. 背景啟動 --------------------------------------------------
    Info "啟動後端…"
    Start-Process -FilePath $exe -WorkingDirectory $backend -WindowStyle Hidden

    # 等待就緒,而不是盲目 sleep
    $ok = $false
    foreach ($i in 1..20) {
        Start-Sleep -Milliseconds 400
        try {
            $h = Invoke-RestMethod -Uri "http://127.0.0.1:8787/health" -TimeoutSec 2
            $ok = $true; break
        } catch { }
    }
    if (-not $ok) {
        Err "後端沒有在 8 秒內回應。請手動執行以查看錯誤:"
        Err "  cd rust-backend; .\target\release\quant-terminal-backend.exe"
        Read-Host "`n按 Enter 結束"
        exit 1
    }
    Info "後端已就緒:http://127.0.0.1:8787"
}

# ---- 5. 顯示目前的安全狀態 --------------------------------------------
try {
    $h = Invoke-RestMethod -Uri "http://127.0.0.1:8787/health" -TimeoutSec 3
    if ($h.trading_enabled -and -not $h.dry_run) {
        Err "*** 實盤下單已啟用 —— 會用真實資金下單(單筆上限 $($h.max_order_notional_usdt) USDT) ***"
    } elseif ($h.trading_enabled) {
        Warn "實盤已解鎖但為乾跑模式:委託會簽章並記錄,但不會送出。"
    } else {
        Info "實盤下單:關閉(安全模式)"
    }
} catch { }

# ---- 6. 開啟儀表板 ----------------------------------------------------
# Windows keeps the .html FILE association separate from the https protocol
# association. Brave can be your default browser and .html can still open in
# Edge, which is why the dashboard kept launching in the wrong browser.
# Prefer an explicit browser here rather than relying on the file association.
#
# Override with:  $env:QT_BROWSER = "C:\path\to\browser.exe"
$browser = $env:QT_BROWSER
if (-not $browser -or -not (Test-Path $browser)) {
    $candidates = @(
        "$env:ProgramFiles\BraveSoftware\Brave-Browser\Application\brave.exe",
        "${env:ProgramFiles(x86)}\BraveSoftware\Brave-Browser\Application\brave.exe",
        "$env:LOCALAPPDATA\BraveSoftware\Brave-Browser\Application\brave.exe"
    )
    $browser = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}

if ($browser) {
    Info "開啟儀表板(Brave)…"
    Start-Process -FilePath $browser -ArgumentList $dash
} else {
    Warn "找不到 Brave,改用系統預設程式開啟 .html。"
    Warn "若想固定用某個瀏覽器,設定環境變數 QT_BROWSER 指向它的 exe。"
    Start-Process $dash
}

Info "完成。後端會持續在背景執行,關掉瀏覽器也不影響。"
Info "停止後端:taskkill /F /IM quant-terminal-backend.exe"
