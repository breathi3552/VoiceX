# ASR 升级执行结果（2026-08-08）

承接 [asr-upgrade-plan-2026-08.md](asr-upgrade-plan-2026-08.md)。本文记录**实际动手后的结论**，其中若干项推翻了计划文档里基于公开资料的推断。

---

# 一、OpenAI 升级：已完成并联调验证

## 1.1 先说最重要的：这不是升级，是修复

计划文档里我标注为"大概率已失效，需实测确认"。**实测确认：确实已失效。**

```
POST https://api.openai.com/v1/realtime/transcription_sessions
  → HTTP 404  "Invalid URL (POST /v1/realtime/transcription_sessions)"
```

而数据库里的实际配置是 `openaiAsrMode = realtime`——也就是说 OpenAI 这个 Provider 在实时模式下**已经是完全不可用状态**，从 2026-05-12 beta 下线那天起。

## 1.2 keywords 的效果比预期大得多

用同一段 TTS 音频（"我们把 Kysely 的 query builder 换成 Drizzle，然后用 Bun 跑 Vitest"）做 A/B：

| 调用 | 输出 |
|---|---|
| `gpt-transcribe`，无 keywords | 我们把 **history** 的 query builder 换成 **json**，然后用**版**跑 **pytest** |
| `gpt-transcribe` + keywords | 我们把 **Kysely** 的 query builder 换成 **Drizzle**，然后用 **Bun** 跑 **Vitest** ✅ |
| `gpt-4o-transcribe`（升级前的现状） | 我们把 **Heasley** 的 QueryBuilder 换成 Drizzle，然后用 **Banpaul** Vitest |

四个专有名词从全错到全对。

验证严谨性：OpenAI 对**未知的 multipart 字段是静默接受**的（传 `totally_bogus_param` 照样返回 200），所以"HTTP 200"不能证明参数生效。因此补了负向对照——把字段名拼错成 `keywordz`，输出**逐字退化成无 keywords 的版本**，确认 `keywords` 确实被读取。

`keywords` 的三种 multipart 编码（重复 `keywords`、`keywords[]`、单字段 JSON 数组）实测**都有效**，最终采用重复字段名的写法，与代码库里 ElevenLabs `keyterms` 的既有写法一致。

## 1.3 联调抓到一个单测抓不到的 bug

把 Rust 单测 dump 出的 payload **原样重放**到真实 API 时报错：

```
"Turn detection is not supported for this transcription model."
param: session.audio.input.turn_detection
```

我在移植时把 beta 版的 `server_vad` 配置块一起搬了过来。`gpt-live-transcribe` 会**直接拒绝**任何 `turn_detection` 块。

正确值是 `null`——而这恰好也是语义上对的：VoiceX 是按住热键说话、松开时自己发 `input_audio_buffer.commit`，本来就不该让服务端 VAD 去切分。

这条只有把真实 payload 打到真实服务上才会暴露，单测断言结构再完整也发现不了。

## 1.4 最终确认的 GA 接入方式

比计划文档里推断的**更简单**——不需要 ephemeral token：

| 项 | 结论 |
|---|---|
| WS URL | `wss://api.openai.com/v1/realtime?intent=transcription` |
| 鉴权 | 直接用 API key 的 `Authorization: Bearer`，**无需** `/realtime/client_secrets` 换票 |
| `?model=` | 转录会话**不能**带，会报 "is a transcription model and cannot be used as the realtime session model" |
| Beta header | 删除 |
| 配置时机 | 连接后发 `session.update` |
| `turn_detection` | 必须 `null` |
| 采样率 | 24000，与我们既有的 `resample_to_24k` 正好一致，无需改动 |

三种连接方式（raw key + intent / ephemeral / ephemeral + intent）实测都能工作，选了最简单的第一种，因而**删掉了整个 `create_transcription_session` 函数**。

事件名全部沿用，`input_audio_buffer.*` 与 `conversation.item.input_audio_transcription.*` 一字未变——所以事件循环、累积器、以及之前处理过的 "benign empty commit race" 逻辑全部保留。

## 1.5 改动清单

- [openai_client.rs](../src-tauri/src/asr/openai_client.rs)：按模型能力分支；新模型走 `keywords` + `languages`，旧模型退回 prompt 拼接
- [openai_realtime_client.rs](../src-tauri/src/asr/openai_realtime_client.rs)：GA 协议迁移，删除换票逻辑与 beta header
- 新增 `openai_asr_delay` 设置（`minimal`…`xhigh`），仅在 realtime + `gpt-live-transcribe` 时可用
- 语种设置复用既有的逗号分隔写法（`zh, en`），**无需 settings 迁移**
- 默认模型 `gpt-4o-transcribe` → `gpt-transcribe`
- i18n 中英文案同步更新，明确标注旧模型不支持 keywords

**没有做静默降级**：选择旧模型时，词典依然通过 prompt 生效，并在文案里写清楚这是退化路径。

验证：`cargo test --lib` 151 passed，`pnpm build` 通过，clippy 对改动文件无告警。

## 1.6 遗留

真机端到端（热键 → 说话 → 注入文本）尚未跑。协议层已用真实 API 逐层验证，但完整链路建议你本地 `pnpm tauri dev` 实际按一次热键确认。

---

# 二、Qwen3-ASR 本地方案：验证通过，建议推进

## 2.1 计划文档里说错的三件事

先纠正，因为它们直接影响决策：

| 计划文档的说法 | 实测 |
|---|---|
| 推荐走 transcribe.cpp | **应该走 `qwen-asr`**。它有独立的 library crate（v0.11.0），只有 **2 个传递依赖** |
| 0.6B 静态占用 2.77 GiB | 磁盘 **1.88 GB**（BF16），峰值 RSS **3.5 GB** |
| 热词只有 "very soft" 的 prompt biasing，能力弱 | **prompt biasing 效果很强**，CER 从 0.044 降到 0.018 |

另外 CLI 自称模型 "~490 MB"，实际下载下来是 1.88 GB。

## 2.2 精度实测

测试集：14 句，覆盖日常口语 / 技术词密集 / 中英混说 / 数字单位 / 长句 / 标点断句。

> ⚠️ 音频是 macOS TTS 合成的，比真实麦克风输入干净得多。**绝对 CER 偏乐观，这里有意义的是相对排序。**真实麦克风素材仍需补。

| 引擎 | CER | 中位延迟 |
|---|---|---|
| qwen 裸跑 | 0.242 | 0.35s |
| qwen + 强制语种 | 0.044 | 0.35s |
| **qwen + 语种 + 热词** | **0.018** | **0.37s** |
| coli（现状基线） | 0.132 | 0.58s |
| OpenAI `gpt-transcribe`（云端参照） | 0.006 | 1.64s |

**qwen 配置正确后比现有 coli 好 7 倍，且快 1.6 倍；比云端 OpenAI 慢在精度、快 4.4 倍在延迟。**

## 2.3 最关键的发现：不配置就会英文漂移

裸跑时模型会**整句切换成英文输出**：

```
参考： 这次一共处理了一千两百三十条记录，耗时四十五秒。
裸跑： This system processed 1,230 records, taking 45 seconds.

参考： 我在 debug 这个 memory leak，感觉是 event listener 没有 remove 掉。
裸跑： I see this in the memory list. I feel like the event listener has not been removed.
```

对输入法来说这比错几个字**严重得多**——用户说中文，蹦出一句英文。

`--language Chinese` / `set_force_language("Chinese")` 可完全消除。数字单位类的 CER 从 1.023 直接降到 0.000。

**结论：强制语种不是可选优化，是接入的必要条件。**

## 2.4 工程可行性：比预期好

用 library crate 写了个最小 spike（非 CLI 子进程），跑通了真实集成形态：

```rust
let mut ctx = QwenCtx::load(&model_dir)?;
ctx.set_force_language("Chinese");
ctx.set_prompt(&hotword_prompt);
ctx.token_cb = Some(Box::new(|tok| { /* 推增量文本给 HUD */ }));
let text = transcribe::transcribe_audio(&mut ctx, &samples_f32);
```

关键点：
- **`transcribe_audio` 直接吃 `&[f32]` 16kHz mono**——正是 VoiceX 采集管线已有的形态，热路径上**不需要落临时 WAV 文件**
- **`token_cb` 提供增量 token 回调**——可以驱动 HUD 实时出字
- 依赖极轻，`cargo add qwen-asr` 只锁了 2 个包

实测数字（M5 Pro / 64GB）：

| 指标 | 值 |
|---|---|
| 模型加载 | 58–94 ms |
| 首 token | 244–427 ms |
| 整句完成（4–12 秒音频） | 361–740 ms |
| 峰值 RSS | **3.5 GB** |
| 磁盘 | 1.88 GB |

## 2.5 内存问题及其解法

3.5 GB 峰值对常驻输入法来说太重。`int8-encoder` / `int8-prefill` feature **没有帮助**（3.51 GB vs 3.53 GB，速度也无变化）。

但**加载只要 ~60ms**，所以有个更好的解法——按需加载、用完释放：

```
cycle 0: load  94ms  transcribe 483ms
cycle 1: load  66ms  transcribe 477ms
cycle 2: load  58ms  transcribe 488ms
cycle 3: load  58ms  transcribe 480ms
cycle 4: load  57ms  transcribe 484ms
```

反复 load → transcribe → drop，加载成本稳定在 60ms 左右，相对 ~480ms 的推理**可以忽略**。

**这把"常驻 3.5 GB"变成了"仅转写期间 3.5 GB"**，代价是每次多 60ms。对按住热键说话的交互完全可接受。

## 2.6 风险清单

1. **`transcribe()` 返回 `Option` 而非 `Result`** —— 失败时拿不到任何原因。这与 CLAUDE.md 里"可见性优于隐藏"直接冲突。接入时需要在我们这层补足诊断（至少区分模型缺失/音频异常/推理失败）。
2. **CLI 流式输出观察到一次 UTF-8 截断**（`换成` → `���成`）。库接口的 `token_cb` 是否有同样问题需要专门验证——按 token 回调切中文多字节字符是典型雷区。
3. **`--stream` 与批量输出逐字节相同**，流式收益未被证实。对我们其实影响不大（短句本来就是整段转写），但别把它当成已验证能力。
4. **测试音频是 TTS**，不代表真实麦克风。上线前必须补真实录音。
5. **Windows 未验证**——按你的意见，Windows 下不提供本地模式即可。CLI 的 `--live` 本身就标注 macOS only。

## 2.7 建议

**推进接入**，理由是三条实测结论同时成立：精度显著超过现有 coli、延迟优于云端、集成形态干净（纯 Rust 库 + 直接吃 f32 + 增量回调）。

接入时必须带上的配置：
- `set_force_language("Chinese")` —— 非可选
- `set_prompt(词典)` —— 这是它唯一的热词通道，但确实有效
- 按需加载 + 用完释放
- Windows 下隐藏该 Provider，而不是让它报错

下一步建议先补**真实麦克风测试集**再动手写集成代码——现在这组 TTS 数据足以支撑"值得做"的判断，但不足以支撑"调参调到什么程度算好"。

---

## 附：复现方式

Spike 代码与测试集在本次会话的 scratchpad（`bench.py` / `testset.json` / `qwen_spike/`）。要复现：

```bash
cargo install qwen-asr-cli
qwen-asr download qwen3-asr-0.6b --output models
```

注意 HuggingFace 大文件下载会中断，用 `curl -C -` 断点续传更可靠。
