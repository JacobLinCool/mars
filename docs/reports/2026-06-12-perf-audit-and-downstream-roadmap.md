# MARS 效能審計與 Issues #35/#37/#38/#39/#40/#41 實作改善報告

> 日期：2026-06-12
> 方法：多代理審計（90 個 subagents：5 張子系統地圖 → 5 維度效能掃描 → 每項發現 3 視角對抗驗證）＋ 主迴圈第一手程式碼覆核。
> 覆蓋缺口（誠實揭露）：`engine-render` 與 `control-plane` 兩個維度的自動驗證波次因 session limit 中斷，已由人工覆核補足 engine/render loop；IPC/doctor 控制平面僅輕度覆蓋。

---

## 一、執行摘要

1. **真正的 realtime 熱路徑只有一條，而它問題最大**：`plugin_do_io_operation`（`crates/mars-hal/src/plugin.rs:1099-1210`）跑在 coreaudiod 的 realtime IO thread 上，目前每次 callback 做 **3 次 blocking mutex、2 次 heap allocation、1 次 DashMap shard lock、3 次完整 header decode**。這正是 issue #38 的範圍，且修復它是整個 downstream roadmap 的先決條件。
2. **Ring header 存在跨行程 lost-update race（正確性問題，非僅效能）**：producer 與 consumer 都對 52-byte header 做完整 read-modify-write 回寫（`shm_backend.rs:290/:345`），process-local Mutex 無法跨行程互斥，且全程無 atomic/Acquire-Release。在引入 app-owned producer（#35）之前必須改成 ring protocol v2。
3. **daemon 的 `marsd-render` 不是真正的 RT thread**：normal priority、相對式 `thread::sleep` 步進（會累積 drift）、整個 workspace 沒有任何 QoS / time-constraint policy 設定。它是 soft-deadline 迴圈，靠 8× ring 容量緩衝。多項「render thread RT 違規」發現經對抗驗證後降級為 hygiene 等級。
4. **六個 issues 構成一條連貫的 downstream 產品線**（虛擬麥克風 SDK）：#38（資料平面 RT 化）→ #39（跨使用者權限）→ #35（producer 模式）→ #40（SDK writer API）→ #41（驗收），#37（installer）為獨立平行軌。
5. 建議的暴露策略：**三平面架構**——控制平面（IPC/SDK typed API）、資料平面（ring v2 + LiveWriter）、部署平面（versioned runtime package + 機器可讀狀態）。

---

## 二、效能審計結果

### 2.1 第一優先：HAL realtime callback（coreaudiod 內，真 RT thread）

#### 〔high〕hal-rt-1 — 每次 IO callback 取得 blocking `DRIVER_STATE` mutex
- 位置：`crates/mars-hal/src/plugin.rs:1188-1196`
- 每次 callback 為了累加 xrun counters 與 timestamp 而取得全域 `parking_lot::Mutex`。同一把鎖被非 RT 路徑長時間持有：`applied_state_json`（在鎖內做 serde_json 序列化，隨裝置數放大）、`set_desired_state_json`/`build_change_plan`（鎖內大量 `format!`）、`sync_object_registry`（跨 `shm_unlink` syscall 持鎖，`plugin.rs:2048-2069`）。parking_lot 無 priority inheritance。
- **修法**：runtime stats 改為 static `AtomicU64` 群（`fetch_add(Relaxed)`），`runtime_stats_json` 直接讀 atomics；徹底移除這把鎖。
- 驗證者校正：單獨修這項不足以消除 config apply 期間的 dropout——callback 還有 `object_registry` 鎖（見 hal-rt-2）。需成套處理。

#### 〔high〕hal-rt-2 — `object_registry` mutex 每 cycle 取 3 次，且與做 syscall 的 sync 路徑互斥
- 位置：`plugin.rs:1110`（頂部 lookup）、`:1201`（尾部 `sample_time_frames` 更新）、`:1044`（`plugin_get_zero_time_stamp`，同樣由 RT context 呼叫）
- 非 RT 的 `sync_object_registry`（`plugin.rs:2053-2131`）整段持有同一把鎖，期間呼叫 `global_registry().remove()` → `shm_unlink` syscall。裝置重新配置時 RT thread 會卡在 kernel call 後面。`find_device_by_object` 還是 O(n) 線性掃描（`plugin.rs:80-82`）。
- **修法**：以 `arc_swap::ArcSwap<HashMap<AudioObjectID, Arc<RtDeviceInfo>>>` 發布不可變 RT snapshot，由 sync 路徑重建；`sample_time_frames`/`volume_scalar` 放入 `RtDeviceInfo` 內的 atomics。注意：`sample_time_frames` 與 `zero_ts_seed` 在 start_io 是一起重設的，不可拆成獨立 atomics 而破壞一致性——以單一 `ArcSwap` 快照或 seqlock 包裝。

#### 〔high〕hal-rt-3 — 每次 callback 兩次 heap allocation
- 位置：`plugin.rs:1114`（`dev.uid.clone()`）、`:1125`（`stream_name()` 內 `format!`，`shm_backend.rs:499-504`）
- macOS malloc 可能取 zone lock／發 mach VM syscall，RT thread 上延遲無上界。
- **修法**：在 `plugin_start_io`（`plugin.rs:1004`，目前 `create_or_open` 回傳值被丟棄）把 `SharedRingHandle` 本身快取進 per-device RT snapshot；callback 只做 Arc deref + try_lock。驗證者指出：只快取 name String 不夠（仍需 clone），必須快取 handle。

#### 〔high〕hal-rt-4 — 跨行程 ring header lost-update race（正確性層級）
- 位置：`shm_backend.rs:259/:290`（write 端整塊 RMW）、`:312/:345`（read 端整塊 RMW；零 frame read 也會回寫）
- producer（daemon render loop）與 consumer（coreaudiod HAL）在不同行程；`SharedRingHandle` 的 Mutex 是 process-local，跨行程毫無互斥。兩邊都把**整個** 52-byte header 寫回：consumer 會用過期快照覆寫 `write_idx`，producer 在 overrun 時直接改寫 consumer 的 `read_idx`。且所有 index 存取都是 plain memcpy，無 Acquire/Release——consumer 可能在 frame data 可見之前就觀察到新的 `write_idx`。
- 對抗驗證共識：機制屬實；發生頻率為「間歇性」（兩邊 µs 級 RMW 窗口需實際重疊，串流中約數秒一次），每次最多錯 256 frames（約 5ms 音訊重播或丟失）＋ xrun counter 漂移。
- **修法（ring protocol v2）**：
  - 關鍵陷阱：現行 offsets `write_idx=20, read_idx=28` 只有 4-byte 對齊，**直接 cast 成 `&AtomicU64` 是 UB、在 arm64 會 fault**。必須重排 header（如 HEADER_SIZE=64，所有 u64 落在 8-byte 邊界，producer 欄位與 consumer 欄位分屬不同 cache line）、bump `RING_VERSION`、處理舊 shm object 的偵測重建。
  - 欄位所有權：producer 只寫 `write_idx`（資料寫完後 Release store）＋ `overrun_count`；consumer 只寫 `read_idx`、`underrun_count`，以 Acquire load `write_idx`。**絕不寫對方欄位**；overrun 改為 producer 丟自己的 frame（或對 `read_idx` 做 CAS），不再改寫 consumer index。
  - 此項與 #38、#35 直接相關：外部 app producer 加入後，沒有 v2 protocol 的 ring race 面只會更大。

#### 〔medium〕hal-rt-6 / ring-shm-8 — 每次 callback 經由 String hash + DashMap shard RwLock 解析 ring
- 位置：`plugin.rs:1136` → `shm_backend.rs:429-431`
- 對固定 binding 的重複查找；reconfiguration 期間 shard write lock 會擋住 RT thread。修法併入 hal-rt-3 的 handle 快取。

#### 〔medium〕hal-rt-7 / ring-shm-7 — 逐 frame staged copy ＋ 每 callback 3 次完整 header decode
- 位置：`shm_backend.rs:265-288/:320-338`（逐 frame：encode 進 `frame_scratch` → 再 memcpy 進 mmap，外加每 frame 的 u64 modulo、checked_mul/checked_add、bounds check）；`plugin.rs:1138/:1158`（前後各讀一次 header 算 stat delta，且該 delta 跨行程下會誤計對方的 xrun）
- 實測佐證（repo 自有 benchmark budgets）：`write_interleaved` 約 15.3 ns/frame 線性增長（256 frames ≈ 3.9µs），bulk 2 段 memcpy 可降至 ~0.1-0.4µs。
- **修法**：以 `(write_idx % capacity)` 一次算出環繞點，拆成至多 2 段連續 `copy_from_slice`（f32 在所有支援平台都是 LE，可用 `bytemuck::cast_slice`）；`write/read_interleaved` 改回傳 `(frames, overrun_delta, underrun_delta)`，刪除 callback 的 before/after `header()` 讀取（同時修正跨行程 xrun 誤計）。注意保留語義：overrun 逐 frame 計數、underrun 每次短讀計一次；frames > capacity 退化情況只拷貝最後 capacity frames。

#### 〔low〕hal-rt-8 — `frame_scratch` 在 RT thread 上首次 lazy 配置
- 位置：`shm_backend.rs:232/:261-262/:316-317`。一次性、極小（channels×4 bytes）；採 bulk-copy 方案後此欄位可直接刪除。

### 2.2 第二優先：daemon 外部 IO 與 sink 路徑

#### 〔high〕sink-capture-2 — recovery thread 在鎖內 drop cpal Stream，render thread 同鎖阻塞
- 位置：`crates/mars-coreaudio/src/lib.rs:1361/:1455`（render 每 cycle 取 `endpoint.runtime.lock()`）vs `:1722/:1745` → `:1830`（recovery 持鎖執行 `runtime.stream = None`，觸發 `AudioOutputUnitStop`/`Dispose`，可阻塞 1-2 個 device buffer period）
- 驗證者校正：兩個 thread 都是 normal priority，所以是「鎖內無界 syscall 造成 deadline miss」而非嚴格的 priority inversion；connect 路徑基本無害（stream 在取鎖前已建好）。
- **修法**：`let old = runtime.stream.take();` → 釋放鎖 → 鎖外 drop。一行級修復、錯誤/重連場景收益高。render 側欄位（device_channels/max_samples/phase）改為 atomics 或 `ArcSwap<small Copy struct>`，讓 `read_input_into`/`write_output_from` 完全不碰 mutex。

#### 〔high〕sink-capture-4 — `Mutex<VecDeque<f32>>` 逐 sample push/pop，臨界區 O(buffer)，try_lock 碰撞即整個 buffer 丟失
- 位置：`lib.rs:2099-2109`（push_samples，HAL callback 側）、`:2124-2136`（pop_samples）、`:1377-1391/:1465-1474`（render 側）
- 實測：逐 sample 迴圈持鎖時間約為 bulk copy 的 **35 倍**；HAL callback try_lock 失敗時整個 device buffer（5-11ms 音訊）被丟棄或填零。
- **修法**：換成 lock-free SPSC ring（`rtrb`/`ringbuf`，或重用 in-process 的 mars ring v2 邏輯）；保守版至少用 `VecDeque::as_slices` + bulk extend、溢位一次 `drain(0..excess)`。

#### 〔high〕sink-capture-3 — `external_runtime.snapshot()` 每 cycle 深拷貝診斷資料
- 位置：`mars-daemon/src/lib.rs:430-431` → `mars-coreaudio/src/lib.rs:1506-1557`
- 健康穩態約 2-3 個小配置＋數次無競爭鎖（驗證後降級）；但 error ring 滿載時每 cycle 數十個 String clone——**惡化點恰好在系統已經出狀況時**。
- **修法**：拆成 counters-only fast path（render loop 只讀既有 atomics + per-endpoint `AtomicU8` phase），完整字串 snapshot 移到 IPC/status 請求路徑或 1Hz housekeeping。

#### 〔high〕sink-capture-1 — `submit_rendered_sinks` 每 cycle 每 sink heap-clone 整批音訊
- 位置：`crates/mars-daemon/src/sink_runtime.rs:202`（`samples: data.clone()` ≈ 2KB/256f stereo ＋ `binding.id.clone()`）
- 驗證者校正：repo 實測整條 submit 路徑 ~200ns-1.1µs/cycle（5.33ms 預算的 ~0.02%），且 render thread 非 RT——實際影響有限，但**違反專案自己的 render-path zero-alloc invariant**（`mars-engine/tests/rt_alloc.rs`）。
- **修法**：固定大小 buffer pool（`ArrayQueue<Vec<f32>>` freelist，容量 = queue_capacity）＋ sink_id 改為預解析的 `usize` index。

#### 〔medium〕sink-capture-5 — endpoint queue 以 `VecDeque::new()`（零容量）建立，增長 realloc 落在 CoreAudio RT callback 內
- 位置：`mars-coreaudio/src/lib.rs:1257/:1289`；input 側增長發生在 `push_samples`（`:2091`，RT callback 內、持 queue mutex），~11 次倍增 realloc、最大單次 memcpy ~16KB。output 側增長在 render thread（非 RT）。
- **修法**：一行修復——建構時 `VecDeque::with_capacity(buffer_frames * EXTERNAL_QUEUE_PERIODS * channels + 1)`。

### 2.3 主迴圈人工覆核發現（自動驗證波次中斷，由本人直接讀碼確認）

#### render loop 計時採相對式 sleep，會累積 drift
- 位置：`mars-daemon/src/lib.rs:447-457`——每 cycle 以 `started.elapsed()` 計算剩餘時間後 `thread::sleep(remain)`。macOS sleep 喚醒延遲（典型 +0.5-2ms）不會被下一輪補償，迴圈實際週期 > 名目週期，長期相對 HAL 消費端漂移，靠 8× ring 容量吸收。
- **修法**：絕對 deadline 步進（`next_deadline += period; sleep_until(next_deadline)`），落後時跳過而非追趕。

#### 整個 workspace 無任何 thread QoS / realtime scheduling 設定
- `grep -r 'qos|thread_policy|time_constraint|pthread_set'` 零結果。`marsd-render` 是 default-priority std::thread。README 標註「Realtime Audio Thread」名不符實。
- **修法（兩階段）**：先設 `pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE)`；若要真 RT 再評估 `thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY)`——但採用後 2.2 節所有「降級」的發現會重新升級為硬違規（normal-priority 鎖共享將成真正的 priority inversion），需先完成 2.2 的修復。

#### engine 本體（`mars-engine/src/lib.rs:1337-1434`）狀態良好
- ArcSwap snapshot、`edge_scratch` 重用、每 cycle 單次 state lock、有 `rt_alloc.rs`/`perf_gate.rs`/`soak.rs` 守門。無需立即動作。

### 2.4 對抗驗證駁回的發現（避免浪費工程時間）

| 發現 | 駁回理由 |
|---|---|
| `SystemTime::now()` 在鎖內（plugin.rs:1192） | macOS 上是 commpage 讀取（~數十 ns、無 syscall）；併入 hal-rt-1 順手移出即可 |
| header cache-line false sharing | 在整塊 header RMW 修掉（hal-rt-4）之前，欄位佈局毫無意義；修掉之後才值得做 cache-line 分離 |
| `RingRegistry::remove` 鎖內 `shm_unlink` 阻塞 RT | Mutex 是 process-local；daemon 在 remove 前已 join render thread；HAL 側用 try_lock。殘餘問題已併入 hal-rt-2 |
| I16/U16 轉換 scratch 在 RT callback lazy 配置 | cpal 0.17.1 macOS backend 硬編碼 F32，該路徑現為 dead code；僅作為升版防禦保留意識 |
| WAV/CAF writer 4-byte 逐次轉換 | sink worker thread 上、非熱路徑，且有 BufWriter 緩衝 |

---

## 三、Issues 確認與實作設計

### #38 perf(hal): 硬化虛擬輸入 IO 路徑 —— 【先做，一切的地基】

issue 描述與審計結果完全吻合（見 2.1）。實作即 2.1 全套：

1. **PR-A：lock-free runtime stats**（hal-rt-1）——`AtomicU64` 取代 `DRIVER_STATE` 內 runtime 欄位。
2. **PR-B：RT device snapshot**（hal-rt-2/3/6）——`ArcSwap<HashMap<AudioObjectID, Arc<RtDeviceInfo>>>`；`RtDeviceInfo { uid, channels, is_input, volume_scalar: AtomicU32, sample_time: AtomicU64, ring: ArcSwapOption<SharedRingHandle> }`；ring handle 在 `plugin_start_io`（plugin.rs:1004）填入。callback 變成：load snapshot → Arc deref → try_lock → transfer，零鎖（blocking）、零配置、零 hash。
3. **PR-C：ring protocol v2**（hal-rt-4/7/8，詳見 2.1 hal-rt-4 修法）。
4. **PR-D：invariant 測試與文件**——比照 `mars-engine/tests/rt_alloc.rs`，為 `mars-hal` 新增 RT 路徑 no-alloc/no-blocking-lock 測試（以 assert-no-alloc 類 allocator hook 或 counting allocator 實作）；在 `docs/` 寫下 HAL realtime invariants（issue 驗收條件最後兩項）。

### #39 fix(shm): 跨 app 與 coreaudiod 使用者的 ring 權限 —— 【資料平面安全模型】

確認屬實：`open_shm_fd`（`shm_backend.rs:541`）固定 `0o600`。daemon/app 以登入使用者執行，HAL plug-in 在 coreaudiod（`_coreaudiod` 使用者）內——跨 uid 即 EACCES。單行程 unit test 測不出來（issue 的 risk 段正確）。

**設計：權限 + capability token 雙層**

1. 權限位：ring 建立模式改為可配置，預設 `0o666`（macOS POSIX shm 無 ACL 可用；BlackHole 等同類產品等效做法）。**不做 silent fallback**——HAL 開啟失敗時必須讓 `mars status`/doctor 看得到（對應 issue 驗收最後一條）。
2. 名稱即 capability：world-readable shm 的緩解——ring 名稱加入不可猜測 token：`mars.vin.<uid>.<token16hex>`。token 由 daemon 在 apply 時生成，經兩條既有的受信通道分發：
   - 對 HAL：寫進 DesiredState JSON（CoreAudio property channel）。
   - 對 SDK/app：經 Unix socket IPC 回傳（socket 本身 per-user 0600）。
   如此即使 mode 0o666，旁路行程拿不到名稱就開不了。token 在 profile re-apply 時輪替。
3. 文件化安全模型：namespace 所有權（`mars.*` 前綴）、誰可讀寫（producer=app 或 daemon、consumer=HAL）、清理責任（daemon `remove_namespace` + HAL device teardown）、孤兒 ring 的 doctor 偵測。
4. 整合測試：兩段式 acceptance——(a) 自動化：以 `sudo -u` 跨使用者開啟同一 ring 驗證（CI 可跑 root 時）；(b) 手動 runbook：真實 coreaudiod 載入 driver 後 `mars doctor` 驗證 HAL 端 ring attach 狀態（與 #41 共用 harness）。

### #35 feat(daemon/profile): app-owned virtual input producers —— 【控制平面核心】

確認問題真實：今日 downstream app 直接寫 `mars.vin.<uid>` 必與 `marsd` render loop（`mars-daemon/src/lib.rs:382-407` 把所有 vin 當 sink ring 寫入）相撞。

**設計**：

1. **Schema（additive、向後相容，仍為 version 2）**：`mars-types` 的 virtual input 結構加上
   ```yaml
   virtual:
     inputs:
       - id: primary-mic
         name: "Virtual Mic"
         channels: 1
         producer: external_app   # 預設 daemon，省略即現行為
   ```
   `ProducerKind { Daemon, ExternalApp }`，serde default = `Daemon`。
2. **Render runtime 跳過**：`render_runtime_config_from_state` 組 `vin_sinks` 時過濾掉 `ExternalApp` 端點——render loop 完全不碰該 ring（daemon 連 create_or_open 都不做，建立權交給 HAL `start_io` 與 SDK writer，避免 spec 衝突）。graph validator 同步規則：external_app vin 不可作為 route 目的地（它沒有 daemon 端 producer），validation error 要可讀。
3. **Producer 健康狀態**：daemon 不在 RT 路徑打 heartbeat，而是 status/doctor 請求時讀 ring header 觀測：
   - `absent`：無 producer 曾 attach（`write_idx == 0` 且無 attach 記錄）
   - `active`：`write_idx` 距上次觀測有前進
   - `stale`：write_idx 停滯超過閾值且 HAL 仍在消費
   - `underrunning`：`underrun_count` 持續上升
   ring header v2 預留 `producer_attach_count`/`producer_generation` 欄位（SDK writer attach 時 bump），讓 absent/stale 區分不依賴 daemon 行程記憶。
4. **原子性**：apply/rollback 不變——producer mode 只是 profile 欄位，沿用既有 transaction（plan → snapshot → apply → rollback on failure）。HAL 在無 producer 時回傳 silence（現行 `read_interleaved` 零填充行為已滿足）。
5. **`mars status --json`**：`virtual_inputs[].producer = { kind, state, last_progress_ns, underrun_count, attach_count }`。

### #40 feat(sdk): downstream virtual input API —— 【SDK 分層】

確認：現行 `mars-sdk`（`crates/mars-sdk/src/lib.rs`，405 行）只有控制平面 wrapper（ping/validate/plan/apply/clear/status/devices/processes/logs/doctor），無資料平面。

**設計：控制與資料平面分離的兩層 API**

1. **控制層（`MarsClient` 擴充）**：
   ```rust
   pub struct AppVirtualInput {
       pub app_id: String,        // reverse-DNS，作為 lease 擁有者
       pub id: String,
       pub name: String,
       pub uid: String,
       pub sample_rate: u32,      // 先支援 48_000
       pub channels: u16,         // 先支援 1，schema 留 u16
       pub producer: ProducerKind,
   }
   impl MarsClient {
       pub async fn ensure_virtual_input(&self, spec: AppVirtualInput) -> Result<VirtualMic, _>;
       pub async fn remove_virtual_input(&self, app_id: &str, id: &str) -> Result<(), _>;
       pub async fn virtual_input_status(&self, app_id: &str, id: &str) -> Result<ProducerStatus, _>;
   }
   ```
2. **關鍵架構決策：app-scoped declarative overlay（lease 模型）**。`ensure_virtual_input` 不要求 app 操作整份 profile YAML（耦合過深，issue 動機所在），daemon 新增 IPC request `EnsureVirtualInput`：
   - daemon 維護「有效設定 = 使用者 profile ⊕ 各 app 的 overlay fragments」；fragment 以 `app_id` 為命名空間、持久化於 daemon state dir、跨 daemon 重啟存活。
   - 衝突（uid/name 撞名）由既有 validator 擋下並回機器可讀錯誤。
   - 沿用 apply transaction：每次 ensure/remove 是一次小型 declarative apply，原子且可 rollback——**維持 MARS 的聲明式本質**，而非開後門變成命令式 API。
3. **資料層（`LiveWriter`）**：
   ```rust
   let mic = client.ensure_virtual_input(spec).await?;
   let mut writer = mic.open_live_writer().await?;   // IPC 取回 ring 名稱+token → shm_open → mmap
   writer.write_f32_interleaved_live(&frames)?;       // live 語義：滿了丟最舊（順 ring v2 producer 規則）
   writer.clear_unread()?;                            // read_idx 追平 write_idx（mode 切換用）
   writer.flush_silence()?;                           // 寫入一段零樣本，讓 consumer 平滑歸零
   drop(writer);                                       // detach：bump producer_generation
   ```
   - writer 內部即 ring v2 的 producer half：`write_f32_interleaved_live` 為 RT-safe（無鎖、無配置、bulk 2 段 copy），可直接在 app 的音訊 callback 內呼叫。
   - app 永遠拿不到、也不需要知道 `mars.vin.<uid>.<token>` 命名——由 SDK 從 IPC 響應封裝。
   - 多通道路徑：`RingSpec.channels` 本就支援；SDK 先驗證 48k/mono，放寬僅是解除 assert。
4. **錯誤 taxonomy**（`MarsClientError` 擴充）：`DaemonUnreachable / RuntimeIncompatible { daemon, sdk } / DeviceConflict / ProducerBusy（已有 active writer）/ RingAttachFailed（對應 #39 的不 silent-fallback 原則）`。
5. SDK docs 加 minimal virtual-mic example（issue 驗收條件），既有 apply/status/doctor API 不變。

### #37 feat(installer): 可嵌入的 install/update/uninstall/status —— 【部署平面，獨立軌】

確認：現行 `scripts/install.sh` 從原始碼建置並安裝 `/usr/local/bin/{mars,marsd}`、`/Library/Audio/Plug-Ins/HAL/mars.driver`、LaunchAgent——對 downstream 產品不可用。

**設計**：

1. **Versioned runtime package**：定義 `mars-runtime-<ver>.tar.gz` 佈局：
   ```
   manifest.json   # { version, min_macos, protocol_version, files: [{path, sha256, codesign_id}] }
   bin/mars  bin/marsd
   launchd/com.mars.marsd.plist
   driver/mars.driver/
   ```
2. **`mars runtime` 子命令家族**（進 `mars-cli`，核心邏輯抽成 `mars-sdk::runtime` 模組讓 Tauri/native app 直接 link）：
   - `mars runtime status --json` → `{ state: missing | installed_not_running | healthy | stale | incompatible, installed_version, driver_version, daemon_version, protocol_version }`
   - `mars runtime install|update --package <path>`：先驗 codesign/notarization staple、版本相容性，再做特權複製（複製 + `launchctl bootstrap` + coreaudiod reload）。特權段拆成單一冪等 script，讓宿主 app 以自家特權流程（SMJobBless/AuthorizationExecuteWithPrivileges/osascript admin）呼叫。
   - `mars runtime uninstall`：反向冪等清理（含孤兒 shm ring 清掃）。
3. **doctor 不掛死**：CoreAudio 列舉包 timeout（issue 明確要求 dry-run/doctor 不 hang）；`doctor_report_internal`（`mars-daemon/src/lib.rs:1145`）的裝置列舉移到帶 deadline 的工作執行緒。
4. **文件**：Tauri/native 嵌入指南——權限提示時機、安裝後健康檢查輪詢（用 `runtime status --json` 的 state machine）、升級時的 daemon drain 順序。

### #41 test(hal): 真實 HAL 驗收測試 —— 【整條鏈的證明】

**設計**：`scripts/acceptance/`＋`mars test hal`：

1. 自動段（需本機已裝 signed driver；CI 標記 manual）：
   - `marsd` apply 單一 `producer: external_app` virtual input
   - 測試 producer（用 #40 的 `LiveWriter`）寫入已知 440Hz 正弦
   - CoreAudio client（`AudioUnit`/cpal capture）從該虛擬輸入錄 2 秒 → FFT 驗證頻率/振幅 → **證明資料真的通過 HAL，而非 daemon 內部 routing**（issue 驗收關鍵條）
   - kill producer → 驗證讀到 silence（underrun 行為）
   - cleanup → doctor 驗證無孤兒 ring
2. 失敗時自動收集 `mars status --json`、`mars doctor`、`log show --predicate 'subsystem == "com.mars"'`。
3. 手動 runbook（docs）：QuickTime 錄音 + Zoom/Meet 裝置選擇檢查表。
4. 此測試同時是 #38（RT 行為）、#39（跨使用者 attach）、#35（producer 健康）、#40（SDK）的端到端驗收——放在 roadmap 最後但**harness 骨架應與 PR-C 同步開工**，作為 ring v2 的回歸防線。

---

## 四、Downstream 暴露策略（綜合）

### 4.1 三平面架構

```
┌─ 部署平面 ── mars runtime install/status（#37）─ 機器可讀 state machine
├─ 控制平面 ── MarsClient typed API（#35+#40 控制層）─ IPC/JSONL，app-scoped overlay lease
└─ 資料平面 ── LiveWriter / ring v2（#38+#39+#40 資料層）─ RT-safe、零鎖、capability-token
```

downstream app 的完整生命週期只接觸三個入口：
`RuntimeManager::status()/install()` → `MarsClient::ensure_virtual_input()` → `VirtualMic::open_live_writer()`。
YAML、ring 命名、daemon 狀態、HAL 細節全部不外洩。

### 4.2 版本相容契約

- **單一相容性矩陣、三個版本號**：`protocol_version`（IPC envelope，現為 2）、`RING_VERSION`（shm header，v2 起）、runtime package `version`。
- SDK 連線時握手檢查 protocol_version；writer attach 時檢查 RING_VERSION；installer 檢查 package vs installed。三處都回 `Incompatible { expected, found }` 結構化錯誤，**永不靜默降級**（與 #39 的不 silent-fallback 原則一致）。
- 相容性承諾文件化：additive schema 欄位不 bump version；ring layout 改變必 bump RING_VERSION；SDK semver 跟隨。

### 4.3 狀態與錯誤 taxonomy（跨平面統一）

| 平面 | 狀態 enum | 錯誤回報 |
|---|---|---|
| 部署 | `missing / installed_not_running / healthy / stale / incompatible` | `runtime status --json` |
| 控制 | apply transaction `applied / rolled_back { reason }` | typed `DaemonResponse::Error { code, message, details }` |
| 資料 | producer `absent / active / stale / underrunning` | `mars status --json` + SDK `ProducerStatus` |

### 4.4 落地順序與里程碑

```
M1（地基，~2-3 PRs）   #38：PR-A atomics → PR-B RT snapshot → PR-C ring v2 → PR-D invariant 測試
M2（安全模型，1 PR）    #39：權限 + token 命名 + 跨使用者整合測試（依賴 ring v2 的 header 欄位）
M3（producer 模式，1-2 PRs）#35：schema + render skip + producer 健康狀態（依賴 M1 的 v2 header、M2 的 token）
M4（SDK，1-2 PRs）      #40：EnsureVirtualInput IPC + overlay lease + LiveWriter（依賴 M3）
M5（驗收，1 PR + docs） #41：acceptance harness + 手動 runbook（端到端驗證 M1-M4）
平行軌（隨時可動工）     #37：runtime package + mars runtime 子命令 + doctor timeout
```

獨立效能快贏（不在依賴鏈上，可隨時插單）：
- sink-capture-2 stream drop 移出鎖（一行級）
- sink-capture-5 VecDeque 預配置（一行級）
- sink-capture-3 snapshot 拆 counters-only fast path
- sink-capture-4 SPSC ring 取代 Mutex<VecDeque>（可直接重用 ring v2 的 in-process 版本，建議排在 M1 之後）
- render loop 絕對 deadline 步進 + QoS 設定（QoS 設定後，sink-capture-1 buffer pool 升級為必要項）

### 4.5 跨領域風險與未解問題

1. **shm 跨使用者權限的實證缺口**：`0o666` + token 是紙上設計；coreaudiod 的 sandbox profile 是否允許 `shm_open` 任意名稱需在 M2 第一天實機驗證（issue #39 驗收第一條）。若 sandbox 擋下，備案是 HAL 端建立 ring（coreaudiod 為 owner）+ app 端開啟，或經 XPC/mach port 傳遞 fd。
2. **Ring v2 遷移**：舊版 driver（v1 header）與新 daemon 並存的窗口——header magic/version 檢查已有（`shm_backend.rs:216-225`），但需明確的「偵測 v1 → unlink 重建」路徑與 installer 的 driver/daemon 同步升級保證（#37 的版本相容檢查）。
3. **Sample-rate conversion 未涵蓋**：#40 限定 48kHz；若 HAL 裝置被 client 切到 44.1kHz，external producer 的 48k 資料會錯速。短期：driver 鎖定 supported rate = 48k only；長期需 SRC 節點（任何 issue 都未涵蓋，建議開新 issue）。
4. **多 producer 互斥**：`ProducerBusy` 依賴 daemon 行程內 lease 記錄；app crash 後的 lease 回收（檢測 writer 行程死亡 → producer_generation 不再前進 → 釋放 lease）需要明確的 timeout 策略。
5. **Device UID 跨重裝穩定性**：downstream app 重裝 runtime 後，Zoom/Meet 記住的裝置選擇是否存活取決於 UID 穩定性——UID 由 app 提供（#40 的 `uid` 欄位）解決了一半，但 driver 重裝期間裝置消失再出現的 client 行為應納入 #41 手動 runbook。
6. **TCC/麥克風權限**：使用 MARS 虛擬麥克風的 downstream app 仍需自己的 mic 權限敘述？（讀虛擬輸入裝置同樣觸發 TCC mic prompt）——應在 #37 的嵌入文件中明確說明。

---

## 附錄：審計統計

- Workflow agents：90（5 map / 5 find / ~70 verify / 6 design+synthesis 因 session limit 中止，設計由主迴圈人工完成）
- Subagent tokens：~8.0M；工具呼叫：2,025；牆鐘：~3.5 小時
- 效能發現：24 項原始 → 14 項確認（3 視角對抗驗證，≥2/3 通過）→ 10 項駁回（含 4 項因驗證票數不足而保守剔除，其中 ring-shm-1/2 的內容由 confirmed 的 hal-rt-4 完整涵蓋且經主迴圈第一手讀碼證實）
