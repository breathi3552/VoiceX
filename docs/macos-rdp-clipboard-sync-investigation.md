# macOS → Windows App (RDP) 剪贴板同步问题调查记录

Last updated: 2026-07-11（下午做了一轮自动化复查，结论见文末"更新 2026-07-11：自动化复查与结论"一节）
状态：**根因已定位到闭源的 Windows App，VoiceX 侧无实用修复**。两个已确认修复（HIDSystemState、跳过恢复）已干净重新应用到工作区（**改动在工作区、未提交**，不再依赖 git stash）。真正的"粘出旧内容"根因是 Windows App 把 Mac 剪贴板变化"公告"给远端的机制会随会话老化而变慢/被节流，读缓存的远端 App（如 Zed）在公告滞后时就粘出旧内容——详见文末结论一节。下面正文（"一句话总结"到"改动去向"）是本次复查之前的原始记录，其中"优先级 0（changeCount 计两次）"假设**已被证伪**，阅读时请以文末结论为准。

> 阅读提示：本文用中文写成。里面标注"**已确认**"的结论是有直接实验证据支持的；标注"**推测/未证实**"的是基于证据做的合理推断，但没有独立验证过，接手者应该自己先核实一遍再当作既定事实使用，不要直接当作地基去做后续工作。

## 一句话总结

VoiceX 在 Mac 上运行，用户通过语音听写，识别结果需要注入到远程 Windows 桌面（通过微软官方 RDP 客户端 **Windows App**，bundle id `com.microsoft.rdc.macos`）里当前聚焦的窗口。注入方式是"写 Mac 系统剪贴板 + 模拟 Cmd+V"。这套流程里发现并修复了两个真实存在、有直接实验证据支持的 bug（见下方"已确认并修复的两个 bug"），但修完之后，问题仍然会复现，复现的模式是：**Windows 那边粘贴出来的内容，在目前观察到的样本里，是"上一轮"注入的内容，而不是这一轮的**——这个"永远慢一拍"的现象，目前还没有找到根因（见"还没解决的问题"）。

## 快速参考：对照实验记录

供接手者快速定位"已经验证过什么、不用重复踩坑"，细节见后文对应章节。

| 配置 | 结果 |
|---|---|
| 手动复制任意内容 + 手动 Cmd+V | 多次测试里一直正确同步 |
| arboard/pbcopy 程序写入 + 手动 Cmd+V | 多次测试里一直正确同步 |
| 程序写入 + enigo 默认路径模拟 Cmd+V（`CGEventSourceStateID::Private`） | 触发粘贴动作，但内容是旧的 |
| 程序写入 + 手写 CGEvent，`Private`/`CombinedSessionState`，各种 tap 位置、user data、PID 字段组合 | 全部无效（这一轮测试因为一处独立的实现 bug——漏了 `NX_DEVICELCMDKEYMASK` 这个设备相关 flag 位——而在 Windows 侧完全没识别出 Cmd 被按住，出现 IME 弹窗或裸字符"v"；这是诊断代码本身的问题，不是在验证"哪个字段有效"这个假设，后续已修正，见 Bug 1） |
| 程序写入 + 手写 CGEvent，`CGEventSourceStateID::HIDSystemState`，其余保持和 enigo 默认一致 | **正确同步**（Bug 1 的修复，见下） |
| 上面 + `VOICEX_SKIP_RESTORE=1`（跳过恢复剪贴板） | 正确同步（多轮验证） |
| 上面但不跳过恢复（正常 900ms 延迟） | 有时候正确，有时候把上一轮内容恢复回去导致同步到旧内容（Bug 2，见下，通过后台轮询诊断直接观察到恢复动作发生的精确时间点） |
| Bug 1 + Bug 2 都修复之后，正常真实听写 | 仍会复现"粘贴出上一轮内容"，且用后台轮询确认 Mac 本地剪贴板在观察窗口内没有被任何东西改变过 —— **这是当前卡住的问题** |
| 把"写入剪贴板到发送 Cmd+V 之间"的等待（`CLIPBOARD_PRE_PASTE_DELAY_MS`）从 120ms 加到 500/1000/2000ms | 没有观察到明显改善，规律依然是"上一轮"（注意：只测过这一个变量，"两次听写之间的间隔时间"从未系统性测试过，见"建议下一步"） |

## 环境

- macOS（用户本机），远程连接一台 Windows 机器
- RDP 客户端：Windows App（微软官方，`com.microsoft.rdc.macos`），闭源，非第三方
- VoiceX：Tauri + Rust 桌面应用，核心注入逻辑在 `src-tauri/src/injector/clipboard.rs`
- enigo 0.6.1（键盘/剪贴板模拟库），arboard 3.6.1（剪贴板读写库）
- 涉及的功能入口：
  - 真实听写路径：热键触发 → ASR/LLM → `src-tauri/src/session/handlers/asr.rs` 里调用注入
  - 手动重放测试路径：History 页面里某条历史记录的详情弹窗，点击"测试：重放并注入"按钮（`src/components/ReTranscribeDialog.vue`），会重跑一遍 ASR/LLM 并在延迟几秒后自动注入到当前前台应用——对应后端 `src-tauri/src/commands/retranscribe.rs` 的 `replay_history_injection` 命令。本文档提到"重放测试"均指这个按钮。
- 关键函数：`TextInjector::inject_via_pasteboard`（`clipboard.rs`），流程是：写剪贴板 → 延迟 `CLIPBOARD_PRE_PASTE_DELAY_MS`（macOS 默认 120ms）→ 发送 Cmd+V → 延迟 `CLIPBOARD_RESTORE_DELAY_MS`（macOS 默认 900ms）→ 视情况恢复剪贴板

## 现象的原始描述

用户在 Mac 上通过 VoiceX 听写，识别文本需要出现在远程 Windows 桌面当前聚焦的应用里。实际观察到的是：Windows 那边粘贴出来的不是这次识别的文本，而是"旧内容"。手动操作（在 Mac 上复制任意文本，切到 Windows App，手动按 Cmd+V）在多次测试里从未失败过，包括 VoiceX 历史记录里点击"复制"按钮（走浏览器 `navigator.clipboard.writeText`，不经过 Rust 后端）配合手动粘贴。只有"程序自动写剪贴板 + 程序模拟 Cmd+V"这条自动化路径会出问题。

## 已确认并修复的两个 bug

这两个都是通过直接的、可复现的实验证据确认的。**建议无论谁来接手都保留这两处改动**——下面给出了完整的代码片段，不依赖 git stash 也能直接重新应用（stash 里三类改动混在一起，拆分方式见文末"改动去向"）。

### Bug 1（已确认）：合成的 Cmd+V 事件源状态不对，Windows App 没有据此触发剪贴板同步

**现象**：程序模拟的 Cmd+V 能让远程 Windows 侧执行一次粘贴动作（即真的触发了粘贴），但粘贴出来的内容始终是 Windows 侧缓存的旧内容——即使 Mac 剪贴板当时已经确认是新内容。而完全相同的剪贴板内容、由人手动按 Cmd+V 触发，则总能正确同步。

**已确认的原因**：macOS 的 `CGEvent`（合成键盘事件）有一个 `CGEventSourceStateID` 字段，标识这个事件的"来源状态"，取值三选一：

- `Private`（进程私有，和真实硬件按键完全隔离）
- `CombinedSessionState`（session 级别，但不接入硬件状态）
- `HIDSystemState`（真实硬件按键共享的状态）

`enigo` 0.6.1 的 `Settings.independent_of_keyboard_state` 只能在前两者之间切换（`true`→`Private`，`false`→`CombinedSessionState`），**没有公开选项能设成 `HIDSystemState`**。

实验证据：用 `core-graphics` crate 直接构造 CGEvent（绕开 enigo），把其他一切参数都保持和 enigo 默认路径完全一致，只把事件源换成 `HIDSystemState`——Windows App 就开始正确地把这次粘贴对应到 Mac 当前剪贴板内容上。这个对照在"重放测试"按钮和真实热键听写两条路径上都验证过、结果一致。

**推测/未证实**：Windows App 判断"这次 Cmd+V 值不值得触发剪贴板同步"用的信号就是 `CGEventSourceStateID == HIDSystemState`，可能是出于安全考虑（防止本地进程伪造按键静默触发剪贴板同步/泄露）。这是基于一次 A/B 对照得出的合理推断，**不是对 Windows App 内部实现的确证**（闭源、无法直接验证）。

**代码改动**（文件 `src-tauri/src/injector/clipboard.rs`，替换原来 `send_paste_command` 函数里 macOS 分支的 enigo 调用，新增一个 `send_paste_command_macos` 函数）：

```rust
// 需要的 imports（放在文件顶部，仅 macOS）：
// use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, EventField};
// use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
// Cargo.toml 需要在 [target.'cfg(target_os = "macos")'.dependencies] 下加一行：
//   core-graphics = "0.25"

// 原有常量（clipboard.rs 已有，未改动）：
// const MACOS_COMMAND_KEYCODE: u16 = 55;
// const MACOS_V_KEYCODE: u16 = 9;

fn send_paste_command_macos(&self) -> Result<(), InjectorError> {
    // NX_DEVICELCMDKEYMASK 是 IOKit 里未公开导出、但被 enigo 内部使用的设备相关 flag
    // 位（区分左右 Command 键），数值取自 enigo 源码
    // enigo-0.6.1/src/macos/macos_impl.rs 的 add_event_flag 函数。
    const NX_DEVICELCMDKEYMASK: CGEventFlags = CGEventFlags::from_bits_retain(0x0000_0008);

    // 这两个 base flags 同样照抄自 enigo 的 Enigo::new()：
    // CGEventFlagNonCoalesced 是命名常量；第二个 0x2000_0000 是 enigo 源码注释里自己
    // 也没解释清楚用途的裸 flag（enigo 原注释：不确定是否需要，但真实按键事件观察到
    // 带这一位，所以照做）——这两者是分开的两个 flag，不要混淆。
    let base_flags = {
        let mut flags = CGEventFlags::CGEventFlagNonCoalesced;
        flags.set(CGEventFlags::from_bits_retain(0x2000_0000), true);
        flags
    };
    let command_held_flags = base_flags | CGEventFlags::CGEventFlagCommand | NX_DEVICELCMDKEYMASK;

    // 关键改动就是这一行：HIDSystemState，而不是 enigo 会用的 Private/CombinedSessionState
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| InjectorError::PasteCommandFailed("CGEventSource::new failed".to_string()))?;

    let post_key = |keycode: u16, keydown: bool, flags: CGEventFlags| -> Result<(), InjectorError> {
        let event = CGEvent::new_keyboard_event(source.clone(), keycode, keydown)
            .map_err(|_| InjectorError::PasteCommandFailed("CGEvent::new_keyboard_event failed".to_string()))?;
        // enigo 默认会给每个事件打上 EVENT_SOURCE_USER_DATA = 100（enigo::EVENT_MARKER），
        // 这个字段本身已经单独测试排除过不是关键变量，这里保持一致只是为了不引入新差异。
        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, i64::from(enigo::EVENT_MARKER));
        event.set_flags(flags);
        event.post(CGEventTapLocation::HID);
        Ok(())
    };

    // Cmd 按下 → V 按下 → V 抬起 → Cmd 抬起。V 按下/抬起时 Cmd 仍处于"按住"状态，
    // 所以都带 command_held_flags；Cmd 自己的抬起事件按惯例不带 Command flag。
    post_key(MACOS_COMMAND_KEYCODE, true, command_held_flags)?;
    post_key(MACOS_V_KEYCODE, true, command_held_flags)?;
    post_key(MACOS_V_KEYCODE, false, command_held_flags)?;
    post_key(MACOS_COMMAND_KEYCODE, false, base_flags)?;

    Ok(())
}
```

`send_paste_command`（原函数）里 macOS 分支改成直接 `return self.send_paste_command_macos();`，Windows/Linux 分支不用动。

### Bug 2（已确认）：VoiceX 自己的"恢复剪贴板"逻辑抢在 RDP 同步完成之前把内容改回去了

**现象**：即使 Bug 1 修好之后，仍然偶发"粘贴出旧内容"。

**已确认的原因**：给 `inject_via_pasteboard` 加了一个诊断用的后台线程（日志前缀 `[clipboard-watch]`），从发送 Cmd+V 之后开始，每 100ms 用一个独立的 `Clipboard` 句柄轮询一次剪贴板内容，持续约 8 秒（80 次），一旦发现变化就记录发生的精确时间和内容。用这个诊断直接观察到：**是 `restore_clipboard_if_unchanged`（VoiceX 自己的函数）**，在写入新内容后大约 900-940ms（正好对应 `CLIPBOARD_RESTORE_DELAY_MS` 这个常量的值）把注入前备份的旧内容写回了 Mac 剪贴板；日志里能确认恢复的内容和注入前的备份完全一致（`matches_our_own_restore=true`）。这个时间点上，RDP 大概率还没有把新内容同步给 Windows（这一点是从时序上推断的，没有独立于剪贴板本身之外的证据，比如没有抓包验证 RDP 具体在做什么），之后 RDP 真正去读 Mac 剪贴板时，读到的就是被恢复回去的旧内容。

**根因**：`inject_via_pasteboard` 原逻辑是：写入新文本 → 发送 Cmd+V → 固定等待 900ms → 如果剪贴板内容还等于我们写入的文本（没被别的东西改过）就恢复成注入前的备份。这个固定延迟是为本地应用设计的（本地粘贴几乎瞬时完成，900ms 绰绰有余），但对经过 RDP 桥接的远程剪贴板同步来说，这个时间有时候不够。

**代码改动**：给 `TextInjector` struct 加一个 `skip_clipboard_restore: bool` 字段：

```rust
// clipboard.rs
pub struct TextInjector {
    mode: TextInjectionMode,
    pre_paste_delay_ms: u64,
    restore_delay_ms: u64,
    skip_clipboard_restore: bool,  // 新增
}

// with_mode 签名从 with_mode(mode: TextInjectionMode) 改成：
pub fn with_mode(mode: TextInjectionMode, skip_clipboard_restore: bool) -> Self { /* ... */ }
```

`inject_via_pasteboard` 里第 4 步：

```rust
thread::sleep(Duration::from_millis(self.restore_delay_ms));
if self.skip_clipboard_restore {
    // 跳过恢复
} else {
    self.restore_clipboard_if_unchanged(&mut clipboard, backup, text);
}
```

这个字段从 `inject_serialized(mode, text, skip_clipboard_restore)`（`injector/mod.rs`）开始一路往下传，经过 `TextInjectionService::inject_background_guarded`/`inject_background`（新增同名参数），到调用方（`session/handlers/asr.rs` 真实听写路径、`commands/retranscribe.rs` 重放测试路径）在算出 `matched_override.is_some()`（这次目标应用是不是命中了用户在设置里配置的"按应用覆盖注入模式"规则——例如用户给 Windows App 配置了强制走 Pasteboard 模式，这个规则本身早已存在，见 `foreground_app::match_text_injection_override` 和 `TextInjectionAppOverride`）之后，把这个布尔值传下去——命中了就跳过恢复剪贴板这一步，没命中（普通本地应用）行为完全不变。

设计思路：过早恢复剪贴板这个竞争条件只在"目标应用被用户专门标记为需要特殊处理"（比如远程桌面客户端）时才有意义去规避，普通本地应用不需要为了这个牺牲"用完剪贴板后自动恢复"的体验。

## 还没解决的问题：粘贴出来的是"上一轮"的内容

修完上面两个 bug 之后，在有限的几组对照样本里，问题的表现是这样一个模式（下面这组是记录最完整的一次，配合了 Mac 剪贴板历史工具的截图核对）：

1. 用户听写第一轮（"我们来进行第二轮测试。"），这一轮本身粘贴是否成功没有作为干净的基线单独确认过。
2. 用户过几分钟后听写第二轮（"现在是九点二十五分，进行测试。"），**Windows 这次粘贴出来的是第一轮的内容**，不是这一轮的。
3. 与此同时，Mac 本地剪贴板历史工具清楚显示：最新一条确实是第二轮的新内容（截图核对过），且用 `[clipboard-watch]` 后台轮询确认，8 秒观察窗口内剪贴板没有发生任何变化（包括没有被 VoiceX 自己的恢复逻辑改变——因为这次目标应用命中了 override，Bug 2 的修复已经生效，恢复逻辑本身被跳过了）。

也就是说：在这组样本里，**Mac 侧的剪贴板内容是正确的、且确认没有被任何东西改变过**，问题看起来出在 RDP 把内容同步到 Windows 这一步。**但这个结论目前只基于个位数的对照样本**，还没有做过大量重复测试来确认这是不是稳定的"精确慢一轮"，还是有时候会慢两轮、或者其实和轮次无关只是碰巧看起来像。

### 已经测试过、没有观察到效果的方向

把"写入剪贴板到发送 Cmd+V 之间"的等待时间（`CLIPBOARD_PRE_PASTE_DELAY_MS`，macOS 默认 120ms）临时改成环境变量 `VOICEX_PRE_PASTE_DELAY_MS` 可调，测过 500ms / 1000ms / 2000ms 等值——**没有观察到明显改善**，规律依然是"粘贴出上一轮内容"。

需要说明的是：这里只测试了"写入剪贴板到发送 Cmd+V 之间"这一段等待，最长只测到几秒；**两次听写之间的间隔时间从来没有被系统性地变化过测试**（比如故意让两轮间隔非常短 vs 非常长，看错位程度是否随之变化）——这是下面"建议下一步"里的第一项，目前还是空白。所以"无论怎么调整都没用"这个说法，只能确认"在几秒这个量级的写入-粘贴延迟范围内无效"，不能确认"任何时序调整都无效"。

### 一个更值得优先尝试、成本很低的假设（尚未测试）

`arboard` 写入文本时的实现通常是 `clearContents()` + `setString(...)` 两步，也就是**一次 `inject_via_pasteboard` 调用可能让 macOS 系统剪贴板的 `NSPasteboard.changeCount` 增加了两次，而不是一次**。如果 Windows App 的剪贴板桥接是"每次用户动作只响应它观察到的第一次 changeCount 变化"这种逻辑，那么：`clearContents()` 产生的第一次变化被当成"本轮"处理掉了（可能对应的是空内容或未完成状态），而 `setString()` 产生的第二次变化——也就是真正的最终文本——要等到*下一次*用户动作（也就是下一轮听写）触发的检查时才被当作"新的一轮"来处理。这将完美解释"无论 Cmd+V 前等多久都没用、但错位精确是一轮"这个现象，因为问题根本不是延迟不够，而是"多计了一次变化，导致内容和轮次错位"。

验证方法：复用已经写好的 `[clipboard-watch]` 诊断线程（在 stash 里），改成监测/打印 `changeCount` 本身的具体数值变化（而不是只对比字符串内容），确认单次 `inject_via_pasteboard` 调用到底让 `changeCount` 跳了几次。如果确认是两次，下一步可以尝试把写入方式换成单次原子写入（比如直接用 `NSPasteboard` 的 `setString(_:forType:)` 而不经过 `clearContents()`，或者用 `arboard` 提供的对应 API 避免多余的一次清空），看是否能让内容和轮次对齐。这个实验预计几十分钟就能做完，成本远低于抓包或者继续盲测延迟参数。

### 外部调研发现的背景信息（**推测/未证实**，不是针对 VoiceX 这个具体场景的直接结论，仅供参考）

- Windows App 在 macOS 上的剪贴板同步问题是社区里长期存在、微软从未公开定位根因的老问题（[Microsoft Q&A: "clipboard deadlock"](https://learn.microsoft.com/en-in/answers/questions/5604525/windows-app-on-osx-clipboard-deadlock)、[Community Hub 帖子](https://techcommunity.microsoft.com/idea/azurevirtualdesktop/cant-copy-and-paste-between-microsoft-remote-desktop-and-other-macos-apps/3401733)）。
- RDP 剪贴板走的 MS-RDPECLIP 协议，官方文档描述的机制是"公告（Format List PDU）+ 按需拉取（Format Data Request/Response，delay-rendered）"——远端粘贴时请求的数据，对应的是"上一次已经公告过的格式列表"，如果本地这次变化还没来得及公告，粘贴请求自然会落到旧内容上。这个机制本身能解释"总是慢一拍"的现象，但没有解释清楚"为什么调整延迟没有观察到效果"——上面"changeCount 计了两次"这个假设，是目前唯一能同时解释这两点的猜想。
- 开源的 FreeRDP 项目在同一个"公告"环节有过多个已确认的竞争条件 bug（[#6999](https://github.com/FreeRDP/FreeRDP/issues/6999)、[#5997](https://github.com/FreeRDP/FreeRDP/issues/5997)），说明这是协议架构层面的脆弱点，不是微软客户端独有，但这些 issue 本身是关于 FreeRDP 这个不同实现的，不能直接当作 Windows App 的确证。
- 社区里公认度最高的手动修复方法：在远程 Windows 那边重启 `rdpclip.exe`——具体操作是任务管理器里结束 `rdpclip.exe` 进程后，不会自动重启，需要手动"文件 → 运行新任务 → 输入 `rdpclip.exe`"，或者在远程 Windows 那边随便做一次复制操作让它重新拉起。
- 没有找到任何"强制同步/立即发送剪贴板"的显式操作或菜单项。

### 建议下一步排查方向（按建议优先级排序）

0. **（新增，建议最先做）验证"changeCount 计了两次"假设**——见上一节，复用 `[clipboard-watch]` 诊断代码，把监测目标从"内容是否变化"换成"changeCount 数值本身"，看单次注入是否产生了两次变化。这个实验最便宜、最可能直接命中根因，建议排在最前面。
1. **确认"慢一拍"是不是精确稳定的**——多测几轮（建议至少 5-10 轮），并且专门变化"两次听写之间的间隔时间"（比如故意间隔 10 秒 vs 间隔 3 分钟），同时用一种客观的方式记录每一轮 Windows 侧实际粘贴出的内容（例如固定粘贴进远程 Windows 上的记事本，每轮清空后粘贴、截图或者手动抄录），这样能对着轮次编号逐一核对，而不是靠回忆判断"是不是对上了"。如果间隔足够长时错位消失，说明还是时序问题，只是需要的时间比之前测过的范围更长；如果无论间隔多长错位都恒定为一轮，那更支持"changeCount 计了两次"这类结构性假设。
2. **观察 Windows 侧自己的剪贴板历史（Win+V）**——注意：这个功能 Windows 默认是**关闭**的，需要提前在设置里打开；而且 Win+V 面板本身不显示每条记录的精确时间戳，没法单靠它去对齐"Mac 端写入时刻"和"Windows 端剪贴板更新时刻"，只能用来定性判断"Windows 是不是压根没收到过这次的新内容"，不能替代时间戳级别的对照。
3. **重启 `rdpclip.exe`**（具体步骤见上文外部调研部分），排除"这几次测试恰好撞上 Windows 侧监听进程卡死"这个可能，重启后重新测试对照一轮。这个假设和上面的假设不冲突，可以在做其他实验之前顺手排除一下。
4. **抓包分析 CLIPRDR 时序**（优先级最低，成本最高）——RDP 连接通常走 TLS/NLA 加密，直接抓包只能看到加密后的数据，看不到 CLIPRDR PDU 的具体内容和时序，需要额外配置解密（比如导出会话密钥）才可行，如果没有现成的解密环境，这一步实际操作起来门槛不低，建议放在其他假设都验证不通之后再考虑。

## 改动去向

本次调查中对代码做的所有改动已经用 `git stash` 收纳，**没有提交、也没有留在工作区**，仓库当前是干净的。

```
git stash list
# stash@{0}: On main: RDP clipboard paste investigation (HIDSystemState fix, skip-restore-on-override, diagnostics) - not fully solved, see docs write-up
```

查看方式：`git stash show -p stash@{0}`（只看不恢复）；恢复方式：`git stash pop`。

**注意**：这个 stash 是把下面三类改动放在同一个 stash 里的，`git stash pop` 会一次性全部应用，没有按文件/按 hunk 拆分成"确认修复"和"临时诊断"两部分（两者在 `send_paste_command`/`inject_via_pasteboard` 这两个函数里有代码上的穿插）。如果只想要两个已确认的修复、不想要诊断代码，建议直接照抄本文档"已确认并修复的两个 bug"两节里给出的完整代码片段重新实现，而不是依赖 stash 的自动应用——更可控，也不用手动拆分 diff。

Stash 里包含：

- Bug 1、Bug 2 对应的两处修复（本文档已经把完整代码贴出来了，建议保留）
- 一个临时诊断用的后台剪贴板轮询线程（日志前缀 `[clipboard-watch]`，发送 Cmd+V 之后每 100ms 检查一次剪贴板内容是否变化，持续约 8 秒；目前只对比字符串内容，还没有加上 changeCount 数值监测——上面"优先级 0"那个实验需要在这个基础上改造）
- 两个临时的环境变量诊断开关：`VOICEX_SKIP_RESTORE`（跳过恢复剪贴板，用于验证 Bug 2）、`VOICEX_PRE_PASTE_DELAY_MS`（覆盖写入到发送 Cmd+V 之间的等待时间，用于测试上面"已经测试过、没有观察到效果的方向"那组实验）——这两个是一次性诊断代码，不建议直接进生产，但恢复出来参考写法很方便
- 两处日志改进（无副作用，建议保留）：`session/handlers/asr.rs` 里，ASR 识别的原始文本、最终注入文本的预览，从 `log::debug!` 提到 `log::info!`、改成无条件打印（原来只有 LLM 改写了文本时才会打印，导致好几轮调试日志里看不到实际识别内容，只能看到字符数）

---

## 更新 2026-07-11：自动化复查与结论

这一节是在上面原始记录的基础上，用**自动化手段**重新复查后写的，结论与上文部分假设**不一致**时以本节为准。本次没有依赖 git stash，而是把两个已确认修复干净地重新实现在工作区（未提交）。

### 一句话结论

两个代码级修复（HIDSystemState 合成粘贴、跳过恢复剪贴板）是真实且正确的、该保留；但真正的"粘出旧内容"根因在**闭源的 Windows App 的剪贴板公告机制**（会话老化后 Mac→远端的公告变慢/被节流），**VoiceX 的 Mac 侧一切正确，没有可靠的代码级修复**。绕过办法是"变旧了就重连会话 / 手动 Cmd+V / 换健壮的远端 App"。

### 已确认（有直接实验证据）

1. **"优先级 0：changeCount 计了两次"假设被证伪。** 用一个独立小程序直接读 `NSPasteboard.changeCount` 实测：`arboard::set_text` = `clearContents()`(+1) + `writeObjects()`(**+0**) = 净 **+1**，不是 +2。macOS 语义是 `clearContents` 拿走所有权时 +1，紧跟其后的 `writeObjects` 不再重复计数。文档建议优先做的"原子单次写入"改法测出来也是 +1（`declareTypes`+`setString`），**行为完全一样，不会修好这个 bug，不用做**。这个实验纯本地、秒级、不需要 Windows/人工。

2. **两个修复重新应用后，在"健康会话 + 记事本"上程序注入 22/22 全部正确**，含快速连发（~1s 间隔）、~3 分钟空闲、诊断线程开/关、`skip_restore` 开/关。所以在健康状态下这条链路是稳的。

3. **真正的故障与"远端目标 App"强相关，是"粘出旧缓存"。** 记事本一直正确（它在收到粘贴时**同步**向 RDP 请求当前剪贴板，总是新鲜）；远端 **Zed 经常粘出旧内容**（它读的是"已公告的本地缓存"，公告滞后就旧）。→ **上文"记事本测好几轮都对"其实是假阴性**：记事本是个健壮目标，掩盖了 bug。用文件触发口 + 截图核对，对 Zed 一测就稳定复现（5 次全新注入全部粘出同一条旧内容）。

4. **进一步夹逼**：注入后 Mac 剪贴板始终是正确的新内容（`pbpaste` 证实，没有被远端回写覆盖）；**手动 Cmd+V 粘出新内容，合成 Cmd+V（120ms 甚至 3s 延迟）粘出旧内容**。而且观察到远端缓存里的旧值会在几十秒后自己追上最新值（两次 burst 之间）——**说明公告不是死了，而是很慢/被节流，会话用久了更慢**。

5. **尝试过、确认无效的 VoiceX 侧方向**：
   - 加大 `pre_paste_delay` 到 3s：仍旧内容（所以**不是简单的"粘贴太快"**）。
   - **打字模式（绕开剪贴板）在 RDP 下不可用**：macOS 的 unicode 注入走 `CGEventKeyboardSetUnicodeString`，RDP 不转发这种合成 unicode 按键（只转发扫描码），中文/大部分字符过不去，实测只出来个 "a"。这也解释了用户为什么给 Windows App 配了 pasteboard override。
   - 粘贴前用合成鼠标微动"唤醒"远端：仍旧内容。
   - 用 Shift+Insert 代替 Cmd+V：Mac 的 Help/Insert 键码（114）不被 RDP 转发为 Windows Insert，**完全没粘进去**。

### 推测（合理但未独立证实）

- **根因**：Windows App 的 Mac→远端剪贴板公告在会话老化后变得很慢/被节流；"读缓存"的远端 App（Zed）在公告滞后时粘出旧内容，"每次粘贴都同步拉取"的 App（记事本）则免疫。这与社区长期反映的 Windows App/RDP 剪贴板问题一致。
- 另外观察到合成 Cmd+V 偶尔被映射成 **Win+V**（弹出 Windows 剪贴板历史面板），说明 Mac-Cmd→Windows 修饰键的映射本身也不完全稳定——但这是次要现象，主症状仍是"粘出旧缓存"。

### 绕过办法（给使用者）

1. **粘贴开始变旧时，断开重连一次 RDP 会话**（新会话公告快，能正常一阵；这也解释了"时好时坏"）。
2. **手动 Cmd+V 一定对**——Mac 剪贴板始终是正确的新内容，听写落空了手动粘一下即可。
3. 尽量往"健壮"的远端 App（记事本这类）里听写；或检查 Windows App 自己的剪贴板/键盘设置、必要时在远端重启 `rdpclip.exe`。

### 自动化验证方法（下次复用，重点）

上文交接时提到"每次都要人配合、效率低"。这次用两层自动化基本解决了：

1. **本地假设验证（完全自动、不需要 Windows/人工）**：关于"macOS 剪贴板本身行为"的假设（如 changeCount 计几次），写个几十行的独立 Rust 小程序直接测量即可，秒级证伪/坐实。上面第 1 条就是这么做的。
2. **RDP 端到端复现（半自动，人只需连一次会话）**：给 dev 构建加一个**文件监视触发口**——往 `/tmp/voicex_inject_test.txt` 写文本，就用**真实注入路径**把它注入当前前台 App（跳过麦克风/ASR/LLM，从有权限的 VoiceX 进程发出，所以合成按键不会被系统丢弃）；再用截图自动核对远端实际粘出的内容。这样**每一轮都由脚本触发、脚本核对**，人不用一轮轮盯着念。这套触发口和几个诊断开关（clipwatch、pre_paste 覆盖、wake、shiftinsert）都是临时代码，验证完**已从代码里删掉**，下次可照此重建。
3. **关键教训**：一定要对着"实际会出错的目标 App"（Zed）去测，而不是顺手开个健壮目标（记事本），否则会得到假阴性、以为修好了。

### 工作区改动（未提交）

`src-tauri` 下 5 个源文件 + `Cargo.toml`/`Cargo.lock`：仅包含两个已确认修复（HIDSystemState 合成粘贴、`skip_clipboard_restore` 逐层透传）和 `asr.rs` 的日志改进，无任何临时诊断代码，`cargo check` 通过。
