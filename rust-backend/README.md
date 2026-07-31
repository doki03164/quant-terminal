# QUANT TERMINAL — Rust 後端

儀表板是網頁,而**瀏覽器沒辦法安全保管交易所密鑰** —— JS 裡的東西使用者看得到、擴充套件讀得到、XSS 也拿得到,而且交易所會用 CORS 擋掉來自瀏覽器的簽章請求。

所以密鑰放這裡,網頁只跟這個服務要餘額、或請它代為下單。

---

## 啟動教學

### 步驟 1 — 安裝 Rust

```bash
winget install --id Rustlang.Rustup -e
```

Windows 上還需要 C 編譯器(相依套件 `ring` 含 C 程式碼)。兩種選擇:

**輕量(約 400 MB,本專案夠用):**
```bash
winget install --id BrechtSanders.WinLibs.POSIX.UCRT -e
rustup default stable-x86_64-pc-windows-gnu
```

**官方推薦(約 2–7 GB,相容性最好):**
```bash
winget install --id Microsoft.VisualStudio.2022.BuildTools -e
rustup default stable-x86_64-pc-windows-msvc
```

驗證:
```bash
cargo --version
```

### 步驟 2 — 設定金鑰

```bash
cd rust-backend
cp .env.example .env
```

編輯 `.env` 填入 API 金鑰。**第一次請維持 `LIVE_TRADING=false`。**

金鑰權限設定:
- 讀取 ✅
- 合約交易 ❌(之後要自動下單才開)
- **提幣 ❌(永遠不要開)**
- IP 白名單:只綁這台機器

### 步驟 3 — 執行測試

```bash
cargo test
```

25 個測試應全數通過:RFC 4231 的 HMAC 已知答案測試(確保簽章正確)、下單安全驗證、指標正確性、以及設定檔解析。

### 步驟 4 — 啟動服務

```bash
cargo run --release
```

看到這行代表成功:
```
listening on http://127.0.0.1:8787
trading disabled (set LIVE_TRADING=true to arm)
```

驗證:
```bash
curl http://127.0.0.1:8787/health
curl http://127.0.0.1:8787/api/balance
```

### 步驟 5 — 開啟儀表板

用瀏覽器打開專案根目錄的 `crypto-bot-dashboard.html`。後端有跑的話,「總資產」會顯示交易所的真實餘額。

---

## ⚠️ 開啟自動下單(請照順序做)

**這是整個專案風險最高的部分。** 在動真錢之前,請先確認你接受以下事實:

- 策略的樣本外數據是勝率 **33%**、**最長連續虧損 16 筆**、期望值 +0.181R(2.5σ,是訊號但**不是證明**)
- 回測**尚未計入滑價與資金費率**,20× 之下這兩項很可能吃掉那個期望值
- 全自動 + 20× 槓桿 + 真實資金 = 最容易出事的組合

### 三道鎖

| 鎖 | 變數 | 預設 | 作用 |
|---|---|---|---|
| 1 | `LIVE_TRADING` | `false` | 總開關。false 時下單端點直接回 403 |
| 2 | `DRY_RUN` | `true` | 驗證並簽章,但不送出 |
| 3 | `MAX_ORDER_NOTIONAL_USDT` | `100` | 伺服器端強制的單筆名目上限 |

**儀表板上的任何按鈕都無法開啟第一道鎖。** 必須手動編輯 `.env` 並重啟服務。這個不對稱是刻意的 —— UI 的 bug、誤點、或惡意網頁都不該碰得到真實下單。

### 建議流程

**1. 先在測試網跑通**

`.env` 加上:
```
BINANCE_BASE=https://testnet.binancefuture.com
LIVE_TRADING=true
DRY_RUN=false
```
用 testnet 申請的金鑰。確認能正常開倉、止損有掛上、平倉正常。

**2. 回到主網,先乾跑**

```
LIVE_TRADING=true
DRY_RUN=true
```
讓機器人跑幾天,**讀日誌**。每一筆「將要送出」的委託都會完整記錄。確認方向、數量、止損價都合理。

**3. 小額實盤**

```
DRY_RUN=false
MAX_ORDER_NOTIONAL_USDT=50
```
50 USDT 名目在 20× 之下只佔用 2.5 USDT 保證金。**用你完全不在乎虧光的金額開始。**

**4. 觀察至少一個月再考慮放大**

特別要親身經歷一次連續虧損。連虧 16 筆時你會非常想關掉系統 —— 那才是真正的考驗。

---

## 背景執行(關掉瀏覽器也繼續跑)

儀表板關掉後 JS 引擎就停了。要讓機器人持續運作,用 daemon 模式。

### 指令

```bash
quant-terminal-backend help      # 說明
quant-terminal-backend serve     # 預設,給儀表板用的 HTTP API
quant-terminal-backend check     # 驗證 config.json 並印出實際生效的設定
quant-terminal-backend daemon    # 無頭執行策略,不需要瀏覽器
quant-terminal-backend parity BTCUSDT 4h   # 印出指標值,供與網頁對拍
```

### 設定檔

在儀表板「設定 → 4 · 設定儲存 → 匯出 config.json」,把檔案放到 `rust-backend/`。
daemon 讀的就是這份,**你在網頁上調的參數和背景執行的完全一致**。

先驗證:
```bash
cargo run --release -- check
```

### 啟動

```bash
cargo run --release -- daemon
```

Windows 背景執行:
```powershell
Start-Process -WindowStyle Hidden ".\target\release\quant-terminal-backend.exe" -ArgumentList "daemon"
elease\quant-terminal-backend.exe" -ArgumentList "daemon"
elease\quant-terminal-backend.exe -ArgumentList "daemon"
```

停止:`Ctrl-C`,或 `taskkill /F /IM quant-terminal-backend.exe`

環境變數 `DAEMON_INTERVAL_SECS`(預設 60)控制評估間隔,`CONFIG_PATH` 可指定其他設定檔。

### ⚠️ 只有 MACD 策略被移植

daemon 目前**只跑 MACD + EMA120**。雙均線與七關趨勢法仍然只在瀏覽器裡執行。

原因:七關法需要擺動點偵測與市場結構判讀,把那段邏輯用另一個語言重寫一次,
一旦出現細微差異,**daemon 交易的就不是你回測過的策略** —— 而且不會有任何明顯徵兆。
與其近似,不如先不做。設定檔裡若含這兩個策略,daemon 會印出警告並忽略它們。

### 指標對拍

已移植的指標與網頁版**逐位元一致**(BTCUSDT 4h、500 根 K 棒,7 項指標相對誤差 0.00e+0)。

自己驗證:
```bash
cargo run --release -- parity BTCUSDT 4h
```
再到瀏覽器 Console 用同一份資料比對。改動任何指標後都該重跑這個。

## 端點

| 方法 | 路徑 | 說明 |
|---|---|---|
| GET | `/health` | 服務狀態、`trading_enabled`、`dry_run`、名目上限 |
| GET | `/api/balance` | 兩家交易所的合約餘額與未實現盈虧 |
| POST | `/api/order` | 下單(市價進場 + 掛上止損 / 止盈) |
| POST | `/api/close` | 平掉指定標的的持倉並取消掛單 |

### 下單的安全驗證

每一筆委託在送出前都會被檢查,不通過就拒絕:

- **必須有止損。** 沒帶止損的委託直接拒絕 —— 20× 之下無保護的部位距離強平只有約 5%
- **止損必須在虧損側。** 做多的止損若高於進場價,那不是止損
- **止盈必須在獲利側**
- **名目金額不得超過上限**(伺服器端強制,不信任瀏覽器)
- 數量必須為正且有限(擋掉 `NaN`、`Infinity`、負數)

Binance 的進場單成交後若**止損掛單失敗**,服務會回傳明確錯誤並在日誌寫下
`POSITION IS UNPROTECTED`,要你立刻手動平倉。

---

## 安全設計

- 密鑰只從環境變數讀取,**不寫入日誌、不回傳前端**
- 只綁 `127.0.0.1`,不對外網開放
- CORS 預設只允許 localhost 與 `null`
- HTTP 逾時 10 秒
- 每一筆下單嘗試都寫入稽核日誌(無論是否真的送出)
- **沒有提幣、沒有轉帳的程式碼路徑**

## 簽章方式

- **Binance**:`HMAC-SHA256(secret, query)` → hex,附在 `&signature=`,金鑰放 `X-MBX-APIKEY`
- **Bitget**:`base64(HMAC-SHA256(secret, timestamp + METHOD + path + body))`,配合 `ACCESS-KEY` / `ACCESS-SIGN` / `ACCESS-TIMESTAMP` / `ACCESS-PASSPHRASE`

## 已知限制

- **下單路徑未經真實交易所驗證。** 單元測試涵蓋安全驗證邏輯與簽章正確性,但這個 repo 沒有對真實交易所送過任何委託。請務必先在測試網驗證
- 沒有處理部分成交、交易所維護、網路重送等邊界情況
- 沒有斷線後的持倉狀態重建

## 風險聲明

本服務僅供教育與個人使用,不構成投資建議。連接真實交易所帳戶並開啟自動下單有重大風險 —— 程式錯誤、金鑰權限設定錯誤、`.env` 外洩或本機遭入侵,都可能造成資金全額損失。使用前請自行閱讀並理解全部程式碼。
