# 企業開發副本：實作進度

日期：2026-11-04。來源為 Cardbe v1.7.0，工作分支 `enterprise/phase-1`。

## 本輪交付

| 項目           | 狀態                                                                                     |
| -------------- | ---------------------------------------------------------------------------------------- |
| 獨立開發副本   | 保留原始碼、Git 歷史、submodule 與企業計畫；執行時測試資料另建                           |
| Linux 建置     | 透過 Cargo 更新 deep-link 至 2.6.3，取代先前測試快取修補                                 |
| Linux 資源統計 | 兩處 sysinfo refresh 排除 threads，新增真實 thread 回歸測試                              |
| Linux beta CI  | 新增手動 Ubuntu 檢查、測試、`.deb` artifact 工作流程；尚未在 GitHub 執行                 |
| 單板備份       | Flow、引用便箋與 ID 重映射；交易回滾；覆寫前快照；匯入時阻擋舊畫布存檔；保留舊格式相容性 |
| 企業原型服務   | 獨立 Axum／SQLite；組織、成員、裝置、板級權限、指派／留言、稽核與持久去重                |
| 管理介面       | 中英雙語、工作區／看板、任務、成員／裝置、板級授權及稽核；連接真實原型 API               |

啟動方式與 API：[企業原型 README](../enterprise-server/README.md)。完整目標：[企業版計畫](enterprise-plan.md)。

後續排程：[下一階段開發計畫](enterprise-next-steps.md)（2026-11-05），包含首兩週工作、在線內部版門檻與桌面同步依賴。

## 與完整企業版的差距

這次完成的是可執行的基礎原型及既有缺陷修正。計畫中的正式 OIDC、PostgreSQL、共用桌面 domain、持久離線佇列／增量同步、通知收件匣、軟刪除／垃圾桶、自動備份與正式部署仍未完成。原型沒有把個人資料上傳，也沒有把本機 MCP 改成遠端服務。

下一階段應先接上正式身分與儲存 adapter，再把已驗證的交易內授權、操作去重和撤權規則接入桌面待送佇列；用同步原型與試點 IdP 驗證後續範圍。

## 依賴與驗證記錄

桌面 Cargo lock 經 package manager 更新；除了 deep-link 2.6.3，Cargo 也重新解析部分既有套件對 `windows-sys` 的版本引用，並未升級那些套件本身。Windows 平台仍需在對應 CI 實測，不能以 Linux 測試替代。

企業服務有獨立 Cargo manifest 與 lock，沒有新增前端依賴。所有測試使用隔離資料庫。

已完成的驗證：

- 桌面 Rust：371 項通過，2 項既有測試忽略；包含實際 Wasm 元件、Linux thread 排除、Flow／便箋備份重映射及失敗回滾。
- 企業服務：17 項 Store 測試、5 項 HTTP 整合測試通過；服務、桌面與 Wasm fixture 的 clippy 均通過。
- 前端：92 個測試檔、742 項測試通過，1 項既有測試因缺少 Chromium 略過；包含匯入等待舊存檔、丟棄過期回應與指令契約檢查。
- Svelte 型別檢查：0 錯誤、0 警告。前端格式、lint 與正式 build 通過。
- Linux 桌面執行檔：使用正式 deep-link 2.6.3 與 `--locked --features tauri/custom-protocol` 建置成功。Xvfb／tauri-driver 實際建立中文看板、欄位與任務，結束程序後以另一個獨立測試程序重啟，確認原資料仍在；截圖顯示資源統計約 1.11 GB，未再出現將 threads 重複累計的異常值。這是 debug 建置的單次觀察，並非效能基準。
- 真實 WebKitGTK MiniBrowser：以繁體中文介面建立工作區、看板、任務與留言；改用成員 token 讀取；撤銷板級權限後收到 404、清除任務與留言，並保持登入。截圖經人工檢查。
- 實際 Owner 恢復 CLI：可替原組織的 Owner 發出新憑證，並通過 API 登入。
- 原始 `Cardbe` 的 717 個工作樹檔案雜湊與複製前一致。

上述 GUI 驗證使用 Xvfb，不能替代實體桌面、Wayland、托盤、通知與 Windows 測試；GitHub Linux workflow 與 `.deb` 打包尚未執行。首次在同一個外層測試程序中連續重啟時，第二次啟動因資料鎖尚未釋放而逾時；改用獨立測試程序後通過，沒有刪除鎖檔或修改測試資料。

## 這台測試機的啟動方式

已在 `/home/brian/cardbe-enterprise-test` 準備隔離工具鏈入口。分別在兩個終端執行：

```sh
/home/brian/cardbe-enterprise-test/enter.sh vp run enterprise:serve
```

```sh
/home/brian/cardbe-enterprise-test/enter.sh vp run enterprise:ui
```

然後開啟 `http://127.0.0.1:1441/enterprise-dev`。測試組織已初始化，Owner 開發憑證位於 `/home/brian/cardbe-enterprise-test/owner.json`（檔案權限 0600，未納入 Git）。若本機已有此測試服務執行，使用現有程序即可。

企業介面的本機驗證報告及安全截圖位於 `/home/brian/cardbe-enterprise-test/logs/browser/`，Linux 桌面驗證結果位於同層的 `gui/`；這些報告不含登入憑證。其他機器依[原型 README](../enterprise-server/README.md)安裝工具鏈並另建測試組織。
