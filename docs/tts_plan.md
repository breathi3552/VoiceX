# VoiceX 跨应用选中文本朗读（TTS）功能实施计划

> 文档状态：正式版 / 开发基线
> 日期：2026-08-12（阶段 0 完成后回填实测结论）
> 讨论过程与各方意见记录见 `tts_plan_discussion_archive.md`

## 0. 进度

| 阶段 | 状态 | 说明 |
|---|---|---|
| 阶段 0 最小端到端原型 | **已完成** | commit `193e5e5`，分支 `feat/tts-selected-text-phase0` |
| 阶段 1 测试基建与选区验证 | 未开始 | 选区门禁 `GO` 仍是宣布功能可用的前提 |
| 阶段 2 热键多动作路由工业化 | 部分提前完成 | 冲突检测已落地；配置化与持久化未做 |
| 阶段 3 系统语音链路工业化 | 部分提前完成 | 取消语义、Escape、ASR/TTS 互斥已落地；HUD、delegate、设置未做 |
| 阶段 4 云端后端 | 未开始 | 新增一个前置决策，见 §3.4 |
| 阶段 5 打磨与发布收尾 | 未开始 | |

标注 **⚠️ 实测修正** 的段落，是阶段 0 的真机验证推翻或修正了原计划假设的地方。新会话接手时优先读这些段落，以及 §5 阶段 0 的实测结论与 §7。

阶段 0 代码位置：

- `src-tauri/src/selection/` — 平台无关接口 + macOS 实现（`macos/ax.rs` 取词、`macos/clipboard.rs` Copy 降级）
- `src-tauri/src/tts/` — `TtsBackend` trait、`MacSystemBackend`、会话控制与结构化日志
- `src-tauri/src/hotkey/manager.rs` — 单监听器内的第二个动作绑定与冲突检测
- `scripts/tts/` — smoke harness 与 CGEvent 注入工具

运行 smoke：先以 `pnpm tauri dev 2>&1 | tee <log>` 启动，再 `scripts/tts/smoke_phase0.sh --log <log>`（需要辅助功能权限，见 §4.4 #1）。

## 1. 背景与目标

为 VoiceX 增加跨应用朗读能力：用户在任意应用中选中文字，按下可配置的全局快捷键，VoiceX 读取所选文字并通过所选 TTS 引擎朗读。

**产品形态**：在现有 VoiceX 中扩展，不做独立应用；代码上实现为边界清晰的独立 TTS 子系统（选区读取、TTS 后端、会话控制解耦）。macOS 首发，核心接口保持平台无关，Windows 后置。

**首版包含**：独立朗读快捷键；AX 选区读取 + 可关闭的 Copy 兼容模式；macOS 系统语音（默认、零配置）；一个云端 TTS 后端；语速/音量/引擎选择；开始、停止、取消与明确错误提示；不保存选区历史。

**首版不包含**：OCR、跨应用逐词高亮、暂停/继续、输出设备选择与 ducking、多云端供应商、Windows、扫描 PDF / Canvas / 远程桌面识别。

**非保证场景**（对外表述与现有文本注入能力对齐）：图片、扫描 PDF、Canvas、自绘控件、远程桌面、受保护内容、安全输入框。

## 2. 关键决策

| 决策点 | 结论 | 要点 |
|---|---|---|
| 产品形态 | 扩展 VoiceX，不做独立应用 | 热键、权限、按键模拟、前台识别、供应商配置、HUD/托盘等基础设施约七成现成；macOS 权限按应用授权，两个应用分别索权是体验倒退。若未来出现独立用户群、独立定价或权限冲突等条件再评估拆分，且先抽共享核心库 |
| 取词策略 | AX 优先、Copy 兜底 | AX 无副作用但覆盖率有限；Copy 覆盖率高但有剪贴板副作用，作为可关闭的兼容模式。验证体系对两条路径同时度量，留数据复核余地 |
| 开发顺序 | 原型优先 | 第一步用最短路径跑通"热键 → 取词 → 系统语音朗读"端到端原型；随后建立验证基建并工业化。原型跑通 ≠ 覆盖率达标，选区覆盖率门禁是宣布功能可用的前置条件 |
| 云端时机 | 系统语音验证后集中选型接入 | 系统语音链路先完整交付（零密钥可用）；之后一次调研多家云端、首期接入一家。统一 `TtsBackend` 接口自阶段 0 即存在（最小形态），避免抽象空转 |
| 验证方式 | 自动化优先 | 脚本驱动真实应用的端到端测试 + 脚本化门禁判定，替代人工测试矩阵；人工仅保留五项不可自动化的残余工作（§4.4） |

## 3. 技术方案

### 3.1 选区读取：分层策略

macOS 无单一 API 能覆盖所有应用的选区读取；业界成熟做法为分层 fallback。运行时顺序：

1. **AX 直接读取**：快捷键触发瞬间冻结前台应用与聚焦元素快照 → 读 `kAXFocusedUIElementAttribute` → `kAXSelectedTextAttribute`。成功时零副作用。
2. **AX 范围读取**：元素只提供选区范围时，经 `kAXSelectedTextRangeAttribute` + 参数化属性取文本。仅在类型与范围明确有效时执行，不吞 API 错误。
3. **Copy 兼容模式**（可关闭）：等待热键修饰键抬起 → 复核前台应用未变 → 备份剪贴板（多类型，失败关闭规则见 §3.4）→ 模拟 Cmd-C → 以 `NSPasteboard changeCount` 判定复制是否发生（超时 300 ms）→ 读纯文本 → 无并发修改时恢复。
4. **明确失败**：HUD 区分"没有选中文字 / 当前控件不支持 / 权限缺失 / 复制超时 / 安全输入拒绝 / 修饰键未抬起 / 前台应用已切换"，日志记录结构化失败类型（不记全文）。

**⚠️ 实测修正（阶段 0）**：

- **修饰键等待是必需步骤，不是优化**。用户按住 ⌥⌘ 时合成 Cmd-C，目标应用收到的是 ⌥⌘C——别人的快捷键。实现为最长等待 800 ms，超时返回 `modifiers_held` 而非硬发按键。
- **等待窗口后必须复核前台应用**。800 ms 足够切换窗口；不复核就可能把 Cmd-C 发给一个从未做过安全输入检查的应用，并读回错误内容。不一致返回 `foreground_changed`。
- **`org.nspasteboard.TransientType` 标记在取词方向不可实现**，原文的承诺已删除。剪贴板管理器污染来自**目标应用**写入的那一次复制，VoiceX 既不是写入方，也无法事后给它打标。若仍要减少污染，需要单独设计（例如缩短占用窗口，或在文档中如实声明该限制），不要假装已做。

**Electron/Chromium 兼容适配器**：不对 Chrome/Electron 一概写入 `AXManualAccessibility`。实现为应用特定适配器：先查询目标进程是否支持该属性；仅对已验证的 Electron 应用（逐个验证后加入适配表）启用，并记录是否改变了目标应用状态；Chrome 先测默认 AX 行为，必要时再评估按需启用方案。

```text
全局快捷键（单监听器内的 ReadSelection 动作）
  -> 冻结前台应用与焦点快照（HUD 不夺焦点）
  -> AX 直接读取 -> AX 范围读取
  -> Copy 兼容读取（修饰键等待 -> 前台复核 -> 快照 -> Cmd-C -> 读取 -> 恢复）
  -> 文本规范化（表格文本、换行、空白）
  -> 敏感度判定（Safe / Secure / Unknown，见 §3.4）
  -> TtsSession -> TtsBackend（系统语音直接朗读 | 云端合成+播放）
  -> HUD 状态
```

### 3.2 Rust 与 macOS 原生接口

技术路径可行：代码库已有全部三类互操作先例（C FFI、CGEvent、objc2），无需 Swift/ObjC 混编。具体依赖与格式细节在对应阶段落地确认：

- **C FFI（AX API）**：`hotkey/permissions.rs` 已有 `AXIsProcessTrusted` 先例；取词只需约 5 个 ApplicationServices C 函数，自声明 `extern "C"` 实现。`core-foundation` 目前是传递依赖，需加为直接依赖以管理 CFString/CFTypeRef。
- **CGEvent（模拟 Cmd-C）**：`injector/clipboard.rs` 的 Cmd-V 注入已覆盖事件 source 与 flag 处理等深层问题；取词方向（备份 → 复制 → 读取 → 条件恢复）与注入方向（写入 → 粘贴 → 恢复）假设不同，**代码独立实现，不复用注入路径**。
- **objc2（NSPasteboard changeCount）**：objc2-app-kit 已在依赖中，需加 `NSPasteboard` feature；`arboard` 不暴露 changeCount，此处直调。
- **AVSpeechSynthesizer（系统语音）**：需新增 `objc2-avf-audio` 依赖。阶段 0 仅 speak/stop + `isSpeaking` 轮询（带启动锁存与启动超时）；阶段 3 实现 delegate（objc2 `define_class!`）以准确区分完成/停止/取消/失败，按 objc2 官方模式实现并单测。
- **云端音频播放**：reqwest + rodio（需新增依赖），纯 Rust 路径。云端流式响应的实际格式（PCM / WAV / MP3 / 分块编码）决定能否渐进播放，在阶段 4 原型确认后再定播放方案。
- **线程约束**：AX 调用固定单线程串行（专用 worker 线程）。AppKit 侧按对象区分，不搞一刀切——判据是 objc2 是否将该类标为 `MainThreadOnly`：
  - `AVSpeechSynthesizer`：经 `run_on_main_thread` 在主线程访问。
  - `NSPasteboard`：**⚠️ 实测修正** objc2 未标记为 `MainThreadOnly`（对照 `NSWindow` 有该标记），`generalPasteboard()` 也是无需 `MainThreadMarker` 的安全函数；现有 `injector/clipboard.rs` 经 arboard 同样在非主线程操作。且 Copy 路径要轮询 changeCount 最长 300 ms，放主线程等于卡住 UI 那么久。因此剪贴板操作留在 selection worker 线程。
- **⚠️ `run_on_main_thread` 无法取消**：闭包一旦入队就一定会执行，调用方超时放弃等待也拦不住。凡是有副作用的主线程闭包（尤其 speak）必须自己携带取消 token 并在执行前复查，否则会出现"调用已失败/已取消，但稍后仍开始朗读"。阶段 3 换 delegate 时这条约束依旧成立。

### 3.3 架构组件

- **HotkeyManager 多动作路由**：一个系统级监听器（现有 `rdev::grab`），动作映射（`Dictate` / `ReadSelection`），设置时冲突检测，事件先解析为动作再路由到对应 Session。保留听写的轻点/长按/双击语义；TTS 首版单击语义：空闲=朗读、合成中=取消、播放中=停止；Escape 仅在 TTS 活动时生效。**任何阶段都不创建第二个全局键盘 hook**。
- **SelectionReader**（平台无关接口）：`read_selection(context) -> SelectionResult { text, source(AX/AXRange/ClipboardCopy), sensitivity(Safe/Secure/Unknown), app_info, timing, diagnostics }`。macOS 首期实现；核心逻辑不依赖 AX 类型。
- **TtsBackend**（统一控制接口，自阶段 0 存在最小形态）：

```text
TtsBackend
  list_voices()
  start(request, cancel_token)   // request 携带 sensitivity；云端后端据此拒绝非 Safe
  stop()
  status()
├── MacSystemBackend
│     AVSpeechSynthesizer 直接朗读；不产音频 bytes，不经 SpeechPlayback
└── CloudBackend（阶段 4 引入）
      CloudTtsProvider：合成，返回 stream/bytes（capabilities/list_voices/synthesize/cancel、
        最大文本长度、流式与 SSML 声明、标准化错误）
      SpeechPlayback：rodio 播放（开始/停止/完成/失败事件；与 History 录音回放完全分离）
```

  系统后端不被强迫生成音频 bytes；云端后端内部组合合成与播放。前端引擎/音色选项来自共享定义（沿用 ASR/LLM 供应商模式）。History 录音回放与 TTS 播放是两条线，UI 与音频层不混用同一套"播放"语义。
- **TtsSession** 状态机：`Idle -> ReadingSelection -> Synthesizing -> Playing -> Idle`（系统语音路径合成与播放合并为"朗读中"），所有阶段可取消；新请求取消旧请求；**状态变迁写入结构化日志**，作为自动化断言接口。
- **⚠️ 会话所有权用单一代际槽表达**（阶段 0 已落地，阶段 3 的状态机在此之上生长）：一个 `AtomicU64` 记录当前拥有会话的代际（0 = 空闲），`claim()` 取得 `CancelToken`，`finish()` 只在仍持有时才释放。原先设想的"读取中布尔量 + 后端状态"两段拼接有窗口期两边都说不忙，第二次按热键会再开一轮取词而不是停止当前这轮。要点：
  - token 必须**贯穿到 backend**，不能只停在 controller——主线程闭包也要拿着它复查（见 §3.2）。
  - 后端一切终态（完成 / 失败 / 拒绝）都必须 `finish()`，否则会话永久卡在"朗读中"。
  - 键盘 hook 读该槽必须 lock-free：event tap 回调里加锁有导致 tap 超时被系统禁用的风险。
- **ASR/TTS 资源协调**（**已提前在阶段 0 落地**，原定阶段 3）：开始听写先停 TTS；录音中触发 TTS 则拒绝（`speak_err error=recording_active`，HUD 提示待阶段 3）；TTS 不申请麦克风权限；播放失败必须释放资源。提前的理由：一旦热键可用，朗读声会被麦克风录回去并转写，属于按一次就坏的交互。录音状态由 session actor 循环统一镜像出来，避免两边各自查询造成竞态。

### 3.4 隐私与安全边界（失败关闭合同）

**选区敏感度三态**：

- `Safe`：AX 明确判定为普通文本控件（`AXTextField` / `AXTextArea` / `AXStaticText` / `AXComboBox` / `AXSearchField`）→ 正常处理。
- `Secure`：安全输入框、密码框，或系统级 `IsSecureEventInputEnabled()` 为真 → **拒绝读取**，任何路径（含 Copy）不得返回内容。
- `Unknown`：AX 无法判定聚焦控件类型（含 Copy 降级取得文本但无 AX 信息的情况）→ 允许**本地系统语音**朗读；**禁止发送云端**，HUD 提示原因并建议本地引擎。

**⚠️ 实测修正：`AXWebArea` 归入 `Unknown`，不是 `Safe`。** web area 是整个文档，选区可以跨到页面内的密码输入框，而元素级 `AXSecureTextField` 检查只看得到聚焦控件。把整页当普通文本会让它流向云端。

**由此产生的阶段 4 前置决策（新增，未解决）**：浏览器选区实测恒为 `Unknown`（Safari 走 Copy 降级，AX 只给出 web area），叠加上面"Unknown 禁止发送云端"的规则，等于**浏览器内容永远用不了云端音色**——而"把这篇网页读给我听"很可能正是最主要的使用场景。四个候选方向，阶段 4 前必须选一个并写进文档：

1. 顺着选区所在元素向上/向下遍历 AX 层级判定是否含安全控件，判定通过才升为 `Safe`（最贵，最准）。
2. 按应用或按站点的用户显式授权（把判断权交给用户）。
3. 非可编辑上下文的 web 选区视为 `Safe`（弱，但覆盖阅读场景）。
4. 接受现状：浏览器只用本地语音（零风险，但削掉主要场景）。

**剪贴板失败关闭规则**：

- Copy 降级前必须成功快照所有声明为可恢复的 item/type；发现无法快照的类型（私有 UTI、file promise、延迟提供数据、超大数据）时**默认拒绝 Copy 降级**并返回明确错误，而不是覆盖后尽力恢复。
- 恢复承诺限定于冻结的类型集：纯文本、HTML、RTF、PNG 图片、文件 URL；不承诺任意剪贴板内容 100% 恢复。
- 有并发修改（changeCount 变化非本操作所致）时不覆盖新内容。

**数据留存**：

- 默认引擎为本地系统语音；选择云端时明确提示"选中文字将发送给该供应商"。
- 默认不保存选区全文与合成音频；普通日志只记长度、来源、敏感度、目标应用、错误码。
- 诊断模式记录文本须用户显式开启并提示风险。
- 取消后不得继续下载、播放或把旧结果带入下一次会话。

## 4. 自动化验证策略

验证以自动化为主，人工只保留物理上无法自动化的残余项。

### 4.1 验证金字塔

| 层级 | 内容 | 执行方式 |
|---|---|---|
| L1 单元测试 | TtsSession 状态机（取消、重复触发、错误态）；热键动作路由与冲突检测；剪贴板快照/恢复逻辑（含失败关闭分支）；文本规范化；敏感度判定；错误分类；云端请求构造与分段 | `cargo test`，CI 可跑 |
| L2 端到端选区验证 | 三层结构，见 §4.2 | 本机一条命令，输出 JSON 报告 |
| L3 残余人工清单 | 见 §4.4，共 5 项，多为一次性 | 显式清单，逐项打勾 |

### 4.2 端到端选区验证：三层结构

macOS TCC/辅助功能授权按**进程身份**计（`AXIsProcessTrusted` 检查的是当前进程），独立 CLI 通过不能证明签名安装的 VoiceX.app 通过。因此 L2 分为三层：

- **L2a 探针 CLI**（开发诊断用，不作发布门禁）：`selection-probe` bin，执行一次完整分层读取，输出 JSON：前台 app / Bundle ID、聚焦元素 role/subrole/支持属性、各路径成败与耗时、文本长度与 hash（默认不含全文，`--include-text` 供 fixture 比对）、敏感度判定、剪贴板恢复状态、结构化错误。用于快速迭代 SelectionReader 算法。
- **L2b VoiceX 进程内诊断命令**（发布门禁数据来源）：签名安装的 VoiceX 进程暴露诊断触发方式（Tauri command / deep link / 开发者设置入口），以真实 TCC 身份执行同一 SelectionReader 并输出同构 JSON。该诊断能力保留至正式版，便于用户反馈"某应用读不到"。
- **L2c 链路级 E2E**（发布门禁数据来源）：harness 经 CGEvent 注入真实朗读快捷键，触发 VoiceX 完整链路，断言 TtsSession 结构化日志的状态序列、取消行为与 HUD 不夺焦点。

**harness 公共设施**：

- **驱动脚本**：`scripts/tts/` 下每个 P0 应用一个独立 driver：启动应用 → 载入 fixture 文本 → 程序化选中（Cmd-A / Shift+方向键 / 模拟鼠标拖选）→ **断言目标应用仍在前台**（否则该用例判 `invalid`，不计 pass/fail）→ 触发 L2b/L2c → 与 fixture 期望值比对。

**⚠️ 阶段 0 踩过的 harness 坑（阶段 1 的 driver 直接沿用结论）**：

- **按键必须用 CGEvent 注入到 HID tap，AppleScript 不行**。rdev 的 grab 在 `kCGHIDEventTap`，而 System Events 的 `key code` 投递在 session tap——HID tap 在链路更前面，**完全看不到** AppleScript 注入的键。阶段 0 因此写了 `scripts/tts/cgevent_key.py`（ctypes 直调 CoreGraphics，无第三方依赖）。
- **移动键盘焦点必须用真实鼠标事件**。System Events 的 `click at` 是 AX click，点得到元素但**不移动键盘焦点**：在 Safari 里点完页面再 Cmd-A，选中的仍是地址栏，复制出来的是 URL。需用 CGEvent 鼠标事件（`scripts/tts/cgevent_click.py`，已含拖选支持）。
- **Safari 不接受 AppleScript 导航到 `file://`**：`set URL of current tab` 会停在 `favorites://`。只能走 Launch Services（`open -a Safari FILE`）；而 `open` 会把页面塞进最前面那个窗口的 tab，所以要先 `make new document` 造一个自己的窗口，让"最前面"是我们的。
- **driver 的清理逻辑是最危险的代码，必须按唯一标记定位**。阶段 0 两次真实事故：`close document 1 saving no` 关的是最前面那个文档（可能是用户未保存的工作）；"关掉当前 tab 是 fixture 的那个窗口"会连带关掉同窗口内用户的其他 tab（实际弄丢过一个 3 tab 的窗口）。规则：每次运行生成唯一 RUN_ID 并嵌进 fixture，**只关带该标记的 document/tab**；只关 tab 不关窗口；只有自己创建且只剩空白页的窗口才可关闭；定位不到宁可残留。
- **断言要能证明读到的是预期文本**。只断言"取词成功"会被地址栏内容骗过——必须比对字符数或 hash。
- **fixtures**：中文、英文、中英混合、多行、标点/数字/URL/emoji、超长文本；负例：无选区、密码框（自建测试窗体）、焦点在 VoiceX 自身、剪贴板含不可快照类型。
- **报告**：汇总 JSON；`scripts/tts/evaluate_gate.py` 按 §4.3 输出 `GO` / `HOLD` / `INCOMPLETE` 及逐应用明细。
- **运行环境**：本机运行（需一次性授予终端与 VoiceX 权限）；CI 只跑 L1。

### 4.3 验收门槛（脚本判定）

**执行完整性规则**：

- P0 应用未安装或 driver 失效 → 门禁输出 `INCOMPLETE/HOLD`，**不得输出 GO**。
- P1/可选应用可 skip，不进入通过率分母。
- 报告必须输出：必测场景数、实际执行数、skip 数、invalid 数、成功率、每个 P0 应用的普通文本基线结果。

**`GO` 条件（数据来源为 L2b + L2c，L2a 不算数）**：

- P0 应用全部实际执行；每个 P0 至少一个普通文本基线场景通过；总场景通过率 ≥ 90%。
- 冻结类型集内剪贴板恢复率 100%（无并发修改时）；并发修改用例不覆盖新内容；不可快照类型用例正确拒绝降级。
- `Secure` 负例泄露数为 0；`Unknown` 来源发送云端计数为 0。
- Copy 路径 ≤ 300 ms 或明确超时错误；AX 路径 P95 ≤ 100 ms。
- 空选区、焦点在自身、无前台应用三负例返回各自明确错误类型。

未达门槛时先分类失败原因，不得以 OCR、静默剪贴板读取或其他防御性 fallback 掩盖（`HOLD` 路径）。

### 4.4 残余人工清单

1. **一次性系统权限授予**：终端与 VoiceX 的辅助功能 / 输入监控 / 自动化权限（macOS 强制人工）。*阶段 0 已在开发机完成。*⚠️ 注意 debug 构建的 ad-hoc 签名每次重建都可能变，TCC 授权随之失效需重新授予——这正是门禁只认 L2b/L2c（签名安装的 VoiceX 进程）而不认 L2a 探针 CLI 的原因。
2. **默认值决策表确认**（§4.5）：不提出异议即视为按默认值执行。*阶段 0 按默认值执行，无异议。*
3. **主观音质与延迟验收**：系统语音与云端各听一次（约 10 分钟，阶段 3/4 各一次）。
4. **不可脚本化应用抽查**（可选，非门禁）：微信、Word 等各做一次人工选中朗读。
5. **听写功能回归 smoke**：热键路由改造合入后人工听写一次（约 2 分钟；其余回归由 L1 路由测试覆盖）。**⚠️ 阶段 0 已改动热键路由但此项尚未执行**——改动均在 `tts_active != 0` 门控内且有单测覆盖，但真机未验证。阶段 1 开始前补做。

### 4.5 默认值决策表

以下默认值视为已确认，开发按此推进；如有异议请在阶段 1 结束前提出。

| 事项 | 默认值 |
|---|---|
| P0 应用名单（自动化） | Safari、Chrome、VS Code、TextEdit、备忘录、预览（PDF）、Terminal |
| P1 应用（人工抽查，非门禁） | 微信、Word |
| Copy 兼容模式 | 默认开启（受 §3.4 失败关闭规则约束）；HUD 标注文本来源；设置中可关闭。**⚠️ 阶段 0 实测后需复核**：Safari 只能靠这条路径，"可关闭"意味着用户一关就失去浏览器支持；阶段 1 拿到 P0 全量路径分布后重新确认本行 |
| 默认朗读快捷键 | ⌥⌘R（避开系统"朗读所选内容"的 Option-Esc；可配置，冲突检测拦截与听写键重复） |
| 播放中再按快捷键 | 停止（暂停/继续后置） |
| 首个云端后端 | 阿里云 DashScope 体系，复用现有密钥；具体模型（Qwen TTS / CosyVoice 系列等）、音色、区域、HTTP/WebSocket、输出编码由阶段 4 调研简报冻结后才写入配置与数据模型 |
| 首版语音参数 | 语速 + 音量；音调、自动语言检测后置 |
| 超长文本 | 系统语音不设限；云端按句分段、总量上限 5000 字符、超限先 HUD 提示再朗读前 5000 字符 |
| `Unknown` 敏感度来源 | 允许本地系统语音朗读；禁止发送云端 |
| 朗读历史 | 不保存 |

## 5. 实施阶段

每阶段列出任务、**自动化验收**（脚本/测试判定）与**人工参与**（对应 §4.4 编号）。按"原型优先"顺序执行：阶段 0 先跑通端到端，阶段 1–3 工业化，阶段 4 接云端。

### 阶段 0：最小端到端原型（系统语音）— ✅ 已完成

目标：最短路径跑通"热键 → 取词 → Mac 系统语音朗读 → 停止"，验证整条链路成立。

已完成任务：
- ✅ **热键**：现有 `HotkeyManager` 同一 `rdev` 监听器内的第二个动作绑定（⌥⌘R → `ReadSelection`）；**未创建第二个全局 hook**。再按一次即停止。
- ✅ `selection` 模块最小实现：AX 直接读取 + Copy 兜底（含 `Secure` 拒读；"快照失败即拒绝降级"第一天生效）。
- ✅ `TtsBackend` trait 最小定义 + `MacSystemBackend`：speak / stop；`isSpeaking` 轮询，带启动锁存 + 启动超时。
- ✅ 关键事件结构化日志（`event=` 词表即测试契约，与 `scripts/tts/` 同步变更）。

计划外提前完成（理由见对应段落）：
- ✅ 会话取消语义与 `CancelToken`（§3.3）——原计划留到阶段 3，但阶段 0 的轮询实现本身就需要它才正确。
- ✅ Escape 停止朗读（仅在 TTS 活跃时生效，否则透传）。
- ✅ ASR/TTS 最小互斥（§3.3）。
- ✅ 热键冲突检测（与听写键相同则告警并禁用，避免静默失效）；配置化仍留在阶段 2。
- ✅ 非 macOS 平台不注册该热键（避免吞掉组合键却无后端）。

自动化验收结果：`cargo test` 178 项全绿；smoke 脚本在 TextEdit 与 Safari 上连续通过。实测数据：

| 应用 | 取词路径 | 敏感度 | 耗时 | 剪贴板 |
|---|---|---|---|---|
| TextEdit | `ax` | `safe` | 9–34 ms | 未触碰 |
| Safari | `clipboard_copy` | `unknown` | 105–127 ms | 快照/恢复完整 |

人工参与：#1 已完成；#5 未做（见 §4.4）。

**⚠️ 关键实测结论：Safari 走 Copy 降级，不走 AX。**（功能是通的——文字正确、发声正常、剪贴板还原；这里说的是取词路径，不是能力缺失。）

已确认：聚焦元素 role 为 `AXWebArea`；`AXSelectedText` 未产出文本，落到 Copy 降级。

**未确认，且阶段 1 第一件事就该查清**：`AXSelectedText` 到底是返回 `unsupported`（控件不提供该属性）还是 `empty`（提供但报告无选中）。阶段 0 的实现把两者都往下导向 Copy，而区分它们的日志在 `debug` 级别、当时未开启。这个分叉决定阶段 1 的做法完全不同：

- **若为 `unsupported`**：标准 AX 属性在 web area 上不可用，`kAXSelectedTextRangeAttribute` 多半同样不可用——WebKit 用的是自己那套 `AXSelectedTextMarkerRange`。这种情况下"AX 范围读取"救不了浏览器，别按原计划直接开工。
- **若为 `empty`**：属性可用但我们读错了元素（聚焦元素不是持有选区的那个）。这是可修的缺陷，AX 路径对浏览器有救，优先级应该高于范围读取。

查清成本很低：`RUST_LOG=voicex_lib=debug` 重跑一次 Safari 用例即可（`selection/macos/mod.rs` 已有对应 debug 日志）。

无论哪种结果，都已成立的两条含义：(a) 关闭 Copy 兼容模式则浏览器不可用，§4.5"可关闭"这个默认值的代价比预期大；(b) P0 名单里 Safari 目前完全吊在 Copy 路径上。

**覆盖率提醒**：阶段 0 只测了 TextEdit 与 Safari 两个应用（计划要求如此）。P0 名单其余五个——Chrome、VS Code、备忘录、预览（PDF）、Terminal——**一个都没测过**，不要把 Safari 的结论外推到 Chrome：两者 AX 实现不同，Chrome 还牵涉 §3.1 的 `AXManualAccessibility` 适配问题。

阶段 0 遗留的已知缺口（均按计划后置，非缺陷）：无 HUD 状态、无设置 UI、无云端、无语言检测（中文由系统默认语音朗读，若系统语言为英文则听感很差）、完成检测为轮询而非 delegate（极短文本可能在两次 poll 间讲完而被误报为 `start_timeout`）。

### 阶段 1：测试基建与选区读取验证

任务：
- `selection` 模块补全：AX 范围读取、多类型剪贴板快照/恢复（含失败关闭分支）、Electron 兼容适配器、前台快照冻结、文本规范化、敏感度三态判定、结构化错误分类。
- L2a 探针 CLI；L2b VoiceX 进程内诊断命令（同一 SelectionReader、真实 TCC 身份、同构 JSON 输出）。
- `scripts/tts/` harness：P0 应用 driver（含前台断言与 invalid 判定）、fixtures（含负例）、汇总报告、门禁脚本（三态输出）。
- L1 单测：剪贴板快照/恢复边界、changeCount 判定、超时、失败关闭分支、规范化、敏感度判定、错误分类。

自动化验收：`cargo test` 全绿；基于 L2b/L2c 数据的 harness 报告过 §4.3 全部门槛，门禁脚本输出 `GO`。
人工参与：#1、#2、#4。

停止条件：`HOLD/INCOMPLETE` 时按失败分类处理选区问题，不进入阶段 4/5 的云端与发布工作（阶段 2 的热键重构不受阻塞）。

### 阶段 2：热键多动作路由工业化（部分已完成）

剩余任务：动作映射正式化（阶段 0 的硬编码绑定演进为可配置）；TTS 快捷键设置与持久化；快捷键录制流程适配；底层监听与 ASR Session 解耦。

已完成：冲突检测（含听写键运行时改动后的重新评估）与其单测。

自动化验收：路由与冲突检测单测全绿；听写手势（轻点/长按/双击/Escape）语义由状态机单测覆盖。
人工参与：#5（听写 smoke）——已欠账，见 §4.4。

### 阶段 3：系统语音链路工业化（部分已完成）

剩余任务：TtsSession 状态机（在阶段 0 的 `SessionSlot`/`CancelToken` 之上生长，结构化日志埋点）；实现 `AVSpeechSynthesizerDelegate`（objc2 `define_class!`）替换轮询，准确区分完成/停止/取消/失败；系统声音/语速/音量设置；HUD 读取/朗读/停止/错误状态。

已完成：取消语义与 token 贯穿；Escape 停止；ASR/TTS 互斥。

**HUD 是本阶段最大的用户可见缺口**：目前所有失败（无选区 / 控件不支持 / 权限缺失 / 复制超时 / 安全输入拒绝 / 剪贴板未恢复）只进结构化日志，用户看不到。`selection_ok` 已带 `clipboard_restored` 字段，HUD 接入时直接用。

自动化验收：状态机单测（取消、快速重复触发、互斥、delegate 事件映射）全绿；L2c 链路级 E2E 通过（状态序列断言、HUD 不夺焦点）；三负例行为正确。
人工参与：#3（系统音质主观验收一次）。

本阶段完成即达成"零密钥可用"的完整功能。

### 阶段 4：云端后端

前置：阶段 0–3 完成、选区门禁 `GO`。集中做云端选型与接入：自动生成供应商调研简报（复用本仓库 ASR 调研方法论；维度：中文与中英混合自然度、首包延迟、流式与 SSML、成本、取消/超时语义、密钥管理），**冻结具体模型、音色、区域、协议与输出编码**；首期接入一家验证抽象，后续按同一接口增量添加。

任务：`CloudBackend`（`CloudTtsProvider` + `SpeechPlayback`）与共享配置；按句分段与 5000 字符上限；流式格式原型确认后定 rodio 播放方案（渐进或整段）；连通性测试命令；取消、超时、标准化错误；`Unknown` 来源拦截；隐私提示文案。

自动化验收：本地 mock server 集成测试覆盖流式、取消、超时、错误映射、分段、`Unknown` 拦截；真实 API smoke 测试（复用已配置密钥，断言返回音频且时长合理）；取消后无残留下载/播放（日志断言）。
人工参与：#3（云端音质与首包延迟主观验收一次）。

### 阶段 5：打磨与发布收尾

任务：L2b 诊断能力挂入设置页开发者入口；README 与设置页文案并列"语音输入 + 选中朗读"（双语词条），并说明与系统"朗读所选内容"的差异；隐私说明；`pnpm build` 类型检查；支持/非保证场景说明定稿。

自动化验收：`pnpm build` 通过；harness 全量回归 `GO`；`cargo test` 全绿。
人工参与：无新增。

### 后置阶段（不在首版）

- 暂停/继续、句级前进后退、流式播放优化、语言自动检测、发音替换规则、更多云端后端、可选朗读历史。
- Windows：UI Automation + Copy 降级实现 `WindowsSelectionReader`，复用 TtsBackend/Session/设置 UI；harness driver 体系同构迁移。

## 6. 主要风险与应对

| 风险 | 影响 | 应对 |
|---|---|---|
| AX 覆盖率不足 | 常用应用读不到选区 | harness 先行度量；Copy 降级；按失败分类处理而非掩盖。**⚠️ 已部分兑现：Safari 实测走 Copy 降级，AX 不可用** |
| 浏览器选区恒为 `Unknown` | 云端音色对最主要场景（读网页）不可用 | 阶段 4 前必须在 §3.4 四个候选方向中选定一个 |
| harness 清理逻辑破坏用户数据 | 关掉用户未保存文档 / 标签页（阶段 0 真实发生过） | driver 只按唯一 RUN_ID 定位自己创建的资源；只关 tab 不关窗口；定位不到宁可残留 |
| 探针与正式应用 TCC 身份不一致 | 门禁误判（CLI 通过 ≠ 应用通过） | 三层验证；`GO` 仅基于 VoiceX 进程内数据（L2b/L2c） |
| 剪贴板恢复不完整 | 破坏用户剪贴板 | 失败关闭：不可快照即拒绝降级；恢复承诺限定冻结类型集；changeCount 判定；并发修改不覆盖；恢复先构造后清空（清空是破坏性步骤，任何失败都要发生在它之前）；可关闭兼容模式。~~TransientType 标记~~ 取词方向不可实现，见 §3.1 |
| UI 自动化 driver 脆弱 | 应用升级导致 harness 误报 | driver 按应用隔离；前台断言 + invalid 判定；P0 失效 → `INCOMPLETE` 而非静默跳过 |
| CI 无法授予 AX 权限 | harness 不能上 CI | 分层设计：L1 上 CI，L2 本机一条命令运行 |
| 快捷键改造影响 ASR | 听写行为回归 | 单监听器多动作路由（自阶段 0）+ 路由单测 + 一次人工 smoke（**尚未执行**） |
| 主线程闭包无法取消 | 取消后仍开始朗读 | 有副作用的闭包自带 token 并在执行前复查（§3.2） |
| TTS 与录音冲突 | 回声、资源争用 | 会话层互斥（听写优先），单测覆盖 |
| 云端首包延迟高 | 朗读体验迟钝 | 系统语音为默认基线；云端分段 + 流式（格式确认后实施） |
| 敏感文本外发 | 隐私风险 | 本地默认、云端显式提示、不记全文、`Secure` 拒读、`Unknown` 不发云端（门禁强制） |
| AVSpeech delegate 样板与内存风险 | 实现出错 | 阶段 0 用轮询锁存过渡；阶段 3 按 objc2 官方模式集中实现并单测事件映射 |
| macOS 特有代码扩散 | Windows 成本增加 | SelectionReader/TtsBackend/Session 平台无关，AX 类型不出模块 |

## 7. 直接下一步

阶段 0 已完成（commit `193e5e5`，分支 `feat/tts-selected-text-phase0`，未推送）。按优先级：

1. **补做人工清单 #5**（听写回归 smoke）。热键路由已改动但未真机验证，这是当前唯一的已知未验证回归面。
2. **进入阶段 1**：三层验证基建与门禁。优先级排序建议：
   - **先花几分钟查清 Safari 的 AX 分叉**（`RUST_LOG=voicex_lib=debug` 重跑一次，见 §5 阶段 0）：是 `unsupported` 还是 `empty`。这个答案决定要不要做 AX 范围读取——若是 `unsupported`，范围读取对 web area 很可能同样无效，直接开工是白走一趟；若是 `empty`，先修元素定位比做范围读取更划算。**不要跳过这一步直接按原计划实现范围读取。**
   - 再用同一方法把 P0 名单剩下五个应用（Chrome、VS Code、备忘录、预览、Terminal）各测一遍，拿到真实的路径分布，再决定 §4.5"Copy 兼容模式可关闭"这条默认值是否还成立。
   - 再补 L2a/L2b/L2c 与 harness；driver 直接沿用 §4.2 的踩坑结论，不要重新发明。
   - 门禁脚本三态输出，`INCOMPLETE`/`HOLD` 不得输出 `GO`。
3. **阶段 4 前**在 §3.4 的四个候选方向中选定浏览器选区敏感度方案，否则云端音色对主要场景不可用。
4. 阶段 2/3 的剩余项（配置化、delegate、HUD、设置 UI）可与阶段 1 并行，但选区门禁 `GO` 仍是宣布功能可用的前置条件。
5. 云端选型与接入放在阶段 4 集中进行（模型/协议由调研简报冻结）。
6. 每阶段以自动化验收为完成标准；人工清单仅 §4.4 五项。
