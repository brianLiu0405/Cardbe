# 企業基礎原型

這是 [企業版計畫](../docs/enterprise-plan.md)的第一階段可執行原型：用獨立服務驗證身分、權限、操作接納、撤權、持久去重與稽核。它使用自己的 SQLite，與桌面個人資料庫分開；拒絕開啟既有 Cardbe 桌面資料庫。

目前僅綁定 `127.0.0.1`，使用開發 token，適合本機測試。正式 OIDC、PostgreSQL、桌面離線佇列與同步、排程備份、通知收件匣及正式部署尚未接入。本原型不應直接作為企業正式服務或透過反向代理公開到網際網路。

## 已可操作

- 由 CLI 建立組織、Owner 與裝置；每個組織資料隔離。
- 組織 Owner／Admin／Member；工作區、看板與 Manager／Editor／Viewer 板級角色。
- 成員啟用／停權、裝置 token 發行與撤銷。內容 API 依明確板級授權檢查，管理員不能管理更高階或其他同階管理者的憑證／停權。
- 任務建立、修改、單一負責人、完成狀態、留言，以及以版本偵測同一任務的修改衝突。
- 成功的變更與稽核同交易提交；被拒絕的操作另保留稽核，回滾其業務修改。
- 固定 operation ID 的命令重送，包含服務重啟後的去重；相同 ID 不同資料會拒絕。
- 中英文瀏覽器管理介面、稽核分頁查詢；token 只留記憶體，不寫入瀏覽器儲存。

## 啟動

以下命令在專案根目錄執行。需要專案指定的 Vite+／Node 工具鏈與 Rust；前端依賴使用 `vp install --frozen-lockfile` 安裝。此獨立服務不依賴 GTK／WebKit，桌面程式仍依照[開發文件](../docs/development.md)安裝系統依賴。

第一次初始化測試組織：

```sh
vp run enterprise:bootstrap --organization "測試公司" --owner "管理員" --device "本機"
```

CLI 只在這次輸出顯示 token，請保管輸出；不要將它放入 Git、URL、截圖或公開日誌。再次執行 bootstrap 會新增另一個組織，不是重設現有 Owner。

在兩個終端機分別執行：

```sh
vp run enterprise:serve
```

```sh
vp run enterprise:ui
```

開啟 [http://127.0.0.1:1441/enterprise-dev](http://127.0.0.1:1441/enterprise-dev)，貼上初始化 token。API 位於 `127.0.0.1:9040`；Vite 將同源 `/enterprise-api` 轉送至服務，並改寫 Host，沒有開放跨來源 CORS。`1441` 或 `9040` 已被使用時應先調整配置，不要停止其他人的程序。

預設資料庫為 `enterprise-server/data/enterprise.sqlite3`，已加入 Git 忽略。也可直接使用 Cargo 指定另一個測試資料庫；bootstrap 與 serve 必須使用同一路徑：

```sh
cargo run --manifest-path enterprise-server/Cargo.toml --locked -- bootstrap --data enterprise-server/data/demo.sqlite3 --organization "測試公司" --owner "管理員" --device "本機"
cargo run --manifest-path enterprise-server/Cargo.toml --locked -- serve --data enterprise-server/data/demo.sqlite3
```

服務的 `--port` 與 `--origin` 可供測試調整，但僅接受 loopback；使用不同連接埠時需同步調整 Vite proxy。瀏覽器頁面依賴開發 proxy，目前不支援直接以靜態網站或打包後的桌面頁面連線。

## 建議測試流程

1. 使用 Owner token 登入，建立工作區與看板，再建立測試成員，妥善保管其一次性 token。
2. 將該成員加入看板，先給 Viewer；在另一個瀏覽器工作階段用成員 token 登入，確認只能讀取。
3. Owner 將成員改為 Editor，成員重新載入看板，建立中文任務、修改完成狀態、增加留言。
4. Owner 撤銷裝置或停權成員，成員下一個 API 請求應失敗並清除畫面中的受限內容。
5. 查看稽核；以自動測試驗證遺失回覆／服務重啟後的命令重送只有一次業務效果。

無法確認命令結果時，UI 保留相同 ID 與內容供重試，不會自動產生另一張卡片。這個待確認請求只在目前頁面記憶體，重新整理會遺失，尚不是持久離線佇列。工作區／成員／裝置等管理操作目前沒有重送去重；發生網路不確定結果時，應先重新查詢，避免重複建立。

## API 摘要

Owner 應在 token 到期前建立新裝置憑證，再撤銷舊憑證；UI 不能撤銷最後一位啟用 Owner 的最後有效裝置。若所有 Owner token 已過期，可由有權存取該主機資料庫的操作者使用本機恢復命令，輸入初始化輸出中的組織及 Owner ID：

```sh
cargo run --manifest-path enterprise-server/Cargo.toml --locked -- recover-owner --data enterprise-server/data/enterprise.sqlite3 --organization-id <ORGANIZATION_ID> --owner-id <OWNER_ID> --device "Recovery"
```

恢復命令不經 HTTP，只能替同組織的啟用 Owner 新發 token，並留下稽核。它不將 Member／Admin 升級為 Owner。能直接操作資料庫的主機管理者本來就處於本原型的信任範圍。

除 `/health` 外均需 `Authorization: Bearer <token>`。所有請求檢查 Host；有 Origin 時必須位於明確 allowlist，無法藉由 payload 指定 actor。

| 方法與路徑                                         | 用途                                          |
| -------------------------------------------------- | --------------------------------------------- |
| `GET /health`、`GET /v1/me`                        | 原型狀態、目前組織／使用者／裝置              |
| `GET/POST /v1/workspaces`                          | 工作區列表、建立                              |
| `GET/POST /v1/boards`                              | 有權讀取的看板、建立看板                      |
| `GET/POST /v1/members`                             | 管理員查詢／建立成員與初始裝置                |
| `PATCH /v1/members/{id}`                           | 啟用或停權；停權永久撤銷既有裝置 token        |
| `GET /v1/devices`、`POST /v1/members/{id}/devices` | 裝置列表、新發 token                          |
| `DELETE /v1/devices/{id}`                          | 撤銷裝置                                      |
| `PUT /v1/boards/{id}/access`                       | 板級授權；`role` 必填，顯式 `null` 表示撤銷   |
| `GET /v1/boards/{id}/snapshot`                     | 有權讀取的任務、留言與看板版本                |
| `POST /v1/boards/{id}/commands`                    | 型別操作、版本前提、持久去重                  |
| `GET /v1/audit?after=0&limit=50`                   | 組織管理員查詢安全摘要；以 `next_cursor` 繼續 |

命令範例，operation ID 應由呼叫端建立一次，重試沿用原值：

```json
{
  "operation_id": "81ef9056-e8dc-4a1f-8529-0b74eed1b9fa",
  "command": {
    "type": "task.create",
    "title": "企業原型測試",
    "description": "確認權限與去重"
  }
}
```

`task.update` 需要 `task_id` 與 `base_version`。`assignee_id` 未提供代表不變，`null` 代表清除；指派對象須在同組織啟用且有看板存取權。`comment.create` 需要 `task_id` 與 `body`。命令的完整定義在 [model.rs](src/model.rs)。

## 信任與限制

- Token 是開發憑證，使用隨機來源產生，SQLite 僅保存 SHA-256 digest，14 天到期。停權後重新啟用不會恢復舊 token；由有權管理者新發裝置 token。
- 手動憑證簽發者是受信任的開發操作者：管理員能看到替一般成員簽發的 token，因此能代理該成員。本原型的板級 ACL 不代表內容能對這些簽發者保密；正式版需要 OIDC 與成員自助裝置註冊來分離這個能力。
- SQLite `BEGIN IMMEDIATE` 交易內驗證身分、當下權限、業務限制與版本；重送既有 receipt 前也重驗權限。多個請求序列化處理，適合正確性原型，未經容量壓力測試。
- 一個 user 在此原型只屬於一個組織，工作區名稱對該組織啟用成員可見；看板內容按板級 ACL 隔離。尚未實作工作區成員、Owner 交接、多組織身分綁定或管理員緊急接管。
- 命令、稽核與任務存於服務資料庫，尚未抽取及共用完整桌面 domain。原型只有簡化任務模型，不會同步到原本的個人看板。
- 操作 receipts 與稽核目前持續保存，沒有自動保留／清除排程。無法識別 token 的失敗請求不歸屬任何組織稽核；尚無速率限制或外部安全日誌接收端。
- 伺服器資料與稽核未做應用層加密，也不抵抗主機管理者修改。沒有安全抹除、端對端加密或合規認證承諾。
- HTTP 僅供 loopback 原型使用，正式服務需要 OIDC、TLS、PostgreSQL、部署／備份方案與進一步安全審查。桌面企業快取與離線撤權限制仍依規劃另行實作。

## 驗證

```sh
cargo fmt --manifest-path enterprise-server/Cargo.toml --check
cargo clippy --manifest-path enterprise-server/Cargo.toml --locked --all-targets -- -D warnings
vp run enterprise:test
vp test run src/lib/enterprise/client.test.ts
```

Rust 測試建立隔離資料庫，包含跨組織／板級隔離、Viewer 寫入拒絕、管理員權限、撤權與版本競爭、持久重送去重、HTTP 邊界及 token 過期。資料庫被刻意改壞或不可寫時，不會用記憶體假成功取代持久化結果。

Linux 真實介面測試另有 [enterprise-smoke.py](../scripts/enterprise-smoke.py)。先啟動本機 API、Vite，以及 Xvfb／D-Bus 環境下的 WebKitWebDriver，再執行：

```sh
python3 scripts/enterprise-smoke.py --driver-url http://127.0.0.1:9515 --base-url http://127.0.0.1:1441 --credentials /private/owner.json --output /tmp/cardbe-enterprise-smoke
```

`--credentials` 是隔離測試組織的 bootstrap／recover-owner JSON 輸出。腳本會建立新的中文測試資料、驗證撤權、等待繪製穩定後截圖，並輸出不含 token 的摘要；不要對正式資料庫執行。
