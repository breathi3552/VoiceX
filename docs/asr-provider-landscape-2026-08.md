# VoiceX ASR Provider 调研简报（2026-08-08）

> 调研目的：判断现有 13 个 ASR Provider 是否需要升级模型/调用方式，以及是否需要引入新的 Provider。
> 调研方法：先盘点 `src-tauri/src/asr/` 的实际实现，再对每家做公开资料核查。
> 场景约束：VoiceX 是**热键触发的中文为主语音输入法**——短句（多为 3–30 秒）、中英混杂、大量专有名词/技术词、要求低延迟出字。所有结论都按这个场景加权，不是按会议转写/呼叫中心加权。

> **状态更新（2026-08-08 晚）**：本文的 P0 项已实施并联调验证，结论与实测差异见
> [asr-upgrade-results-2026-08.md](asr-upgrade-results-2026-08.md)。其中两处本文的推断被实测推翻：
> OpenAI Realtime 不是"疑似失效"而是**确已返回 404**；本地方案应走 `qwen-asr` 纯 Rust 引擎
> 而非本文推荐的 transcribe.cpp。

---

## 一、结论速览

| 优先级 | 动作 | 影响 | 工作量 |
|---|---|---|---|
| **P0** | OpenAI：`gpt-4o-transcribe` → `gpt-transcribe` / `gpt-live-transcribe`，并把热词从 prompt 拼接改为原生 `keywords` + `languages` | 准确率与热词命中率明显提升，成本下降 | 中 |
| **P0** | Soniox：默认模型 `stt-rt-v4` → `stt-rt-v5`（v4 已于 2026-06-30 下线，目前靠别名自动路由） | 消除隐性依赖，拿到 v5 的精度/断句提升 | 小 |
| **P1** | Volcengine：核实并接入豆包语音识别 2.0（Seed-ASR-2.0） | 中文 + 专名场景的最大单点收益 | 中（需先核实 resource_id） |
| **P1** | Volcengine：`corpus.context` 增加 `correct_words` 纠错词映射 | 词典"强制改写"类需求可直接下推到 ASR 层 | 小 |
| **P1** | 新增 Provider：Mistral Voxtral Transcribe 2（含 realtime） | 便宜、支持中文、有流式，作为国际线低成本备选 | 中 |
| **P2** | Gemini：把 `gemini-3.1-flash-lite-preview` 升到 3.5 Flash-Lite / 3.6 Flash 一档并做 A/B | 可能提升，但需实测 | 小 |
| **P2** | 本地路径：用 `transcribe.cpp`（Rust binding）替换/并行现有 `coli` | 本地识别质量代际升级（SenseVoice 2024 → Qwen3-ASR / Parakeet） | 大 |
| **不建议** | AssemblyAI、Speechmatics Melia-1、Deepgram Flux | 中文流式能力缺失或不匹配场景 | — |

**总体判断：不需要重构，但有 4 个"低成本高回报"的点值得马上做（P0 两项 + Volcengine 纠错词 + Soniox 默认值）。**

---

## 二、现状盘点

代码位置：`src-tauri/src/asr/`，枚举定义在 [config.rs:7](src-tauri/src/asr/config.rs:7)。共 13 个 Provider。

| # | Provider | 当前默认模型 | 模式 | 热词/上下文机制 | 时效性判断 |
|---|---|---|---|---|---|
| 1 | Volcengine | `bigmodel` / `volc.seedasr.sauc.duration` | 流式 WS | `corpus.context`（hotwords + dialog_ctx）+ `boosting_table_id` | ⚠️ **落后一代**（2.0 已发布） |
| 2 | Google STT V2 | `chirp_3` | 流式 | phrase boost | ✅ 已是最新（无 Chirp 4） |
| 3 | Fun-ASR | `fun-asr-realtime` | 流式 WS | `input.context` | ✅ 当前 |
| 4 | Qwen ASR | `qwen3-asr-flash-realtime` / `qwen3-asr-flash` | 流式 + 批量 | `vocabulary` + `vocabulary_id` | ✅ 当前，已含 Qwen-Audio 3.0 选项 |
| 5 | Gemini | `gemini-3.1-flash-lite-preview` | 批量 | prompt 拼接 | ⚠️ 有更新档位可试 |
| 6 | Gemini Live | `gemini-3.1-flash-live-preview` | 流式 | — | ✅ 当前（3.1 Flash Live 仍是主力音频模型） |
| 7 | Cohere | `cohere-transcribe-03-2026` | 批量 | — | ✅ 当前 |
| 8 | OpenAI | `gpt-4o-transcribe` | 批量 + Realtime | **prompt 拼接热词** | ⚠️ **落后一代 + 调用方式过时** |
| 9 | ElevenLabs | `scribe_v2` / `scribe_v2_realtime` | 批量 + 流式 | `keyterms` | ✅ 当前（无 v3） |
| 10 | Soniox | `stt-rt-v4` | 流式 WS | `context.terms` | ⚠️ **v4 已下线**，靠别名兜底 |
| 11 | StepAudio | `stepaudio-2.5-asr` | 批量 | 有 hotword 处理 | ✅ 当前 |
| 12 | MiMo | `mimo-v2.5-asr` | 批量 | ❌ 未接热词 | ✅ 当前（V2 系列已于 2026-06-30 下线，我们已在 V2.5） |
| 13 | coli（本地） | sherpa-onnx + SenseVoice-small(2024-07) / whisper-tiny.en | 本地流式 | — | ⚠️ **模型明显过时** |

代码层面已经做得不错的地方（不需要动）：
- Volcengine 已经把词典 + 最近历史组装成 `corpus.context`，并且在有 inline 热词时主动跳过 `boosting_table_id` 以避免重复（[client.rs:390](src-tauri/src/asr/client.rs:390)）。
- Fun-ASR 已经处理了"新快照 `2026-02-28` 不支持上下文增强"这个坑，并在 UI 明确警告（[AsrFunAsrSettings.vue:37](src/components/asr/AsrFunAsrSettings.vue:37)）。这类"模型能力差异"的显式提示应该保持。

---

## 三、逐项发现与建议

### P0-1 · OpenAI：换代 + 换调用方式（收益最大的一项）

**发生了什么：**
- OpenAI 于 2026-07-28 发布 `gpt-transcribe`（文件/committed turn）与 `gpt-live-transcribe`（低延迟流式）。`gpt-4o-transcribe` / `gpt-4o-mini-transcribe` 已被官方描述为 legacy，"不是新集成的推荐起点"。
- 官方公布的 Common Voice（22 语种）WER：`whisper-1` 40.37% → `gpt-transcribe` 19.27%。
- 价格 $0.0045/分钟（≈ $0.27/小时）。

**对我们最关键的一点——热词机制变了：**
`/v1/audio/transcriptions` 和 realtime transcription session 现在都原生支持三个结构化参数：
- `prompt`：自由文本，描述录音场景
- `keywords`：**字面词条列表**（产品名、缩写、术语）
- `languages`：期望语种列表（ISO 639-1），支持多语种混合

我们现在的做法是把词典塞进 prompt 字符串——见 [openai_realtime_client.rs:422](src-tauri/src/asr/openai_realtime_client.rs:422) 的 `build_transcription_prompt(prompt, hotwords)`。这在新 API 下是次优解：`keywords` 是专门为这个用途设计的通道，而 prompt 被稀释后反而会削弱场景描述的作用。

realtime session 还新增了 `delay` 参数（`minimal` / `low` / `medium` / `high` / `xhigh`），直接对应"延迟 ↔ 准确率"权衡——这对语音输入法是个很好的可调旋钮，可以作为设置项暴露。

**建议：**
1. 模型下拉增加 `gpt-transcribe`（批量默认）、`gpt-live-transcribe`（realtime 默认），保留 4o 系列为 legacy。
2. 词典改走 `keywords`，`prompt` 只保留场景描述。
3. `languages` 由现有 `openai_asr_language` 扩展成列表（中文用户典型值 `["zh","en"]`）。
4. realtime 暴露 `delay` 设置。
5. 官方文档明确提醒："keywords 是提示，不是强制输出" —— UI 文案不要承诺 100% 命中。

### P0-2 · Soniox：默认模型改 `stt-rt-v5`

- `stt-rt-v5` 于 2026-06-16 上线；`stt-rt-v4` **已于 2026-06-30 下线**，请求会被自动路由到 v5，API 无需改动。
- 也就是说我们现在实际跑的已经是 v5，但配置里写的是 v4 —— 这是个隐性依赖（别名策略随时可能变），且用户界面显示的模型名是错的。
- v5 的改进方向对我们有利：更快的语义断句（endpointing）、更强的噪声/口音鲁棒性、更好的上下文识别。
- `context.terms`（我们已在用，[soniox_client.rs:117](src-tauri/src/asr/soniox_client.rs:117)）在 v5 继续支持。

**建议：** 默认值改 `stt-rt-v5`，下拉里保留 v4 别名并标注"已下线，自动路由"。

### P1-1 · Volcengine：豆包语音识别 2.0

- 火山引擎已发布 **豆包语音识别模型 2.0（Doubao-Seed-ASR-2.0）**，基于 Seed MoE 架构，沿用 1.0 的 20 亿参数音频编码器。
- 官方宣称的改进正好命中我们的痛点：**上下文关键词召回率 +20%**，专门优化专有名词、人名、地名、品牌名、易混淆多音字；新增 13 个海外语种。
- 关键差异化：2.0 用 PPO 强化学习做上下文理解，**不依赖目标词汇的历史出现记录** —— 意味着即使词典里没有的新词，只要上下文合理也能识别对。这比纯热词表更适合"用户随时说出新术语"的输入法场景。
- 支持双向流式。

**⚠️ 待核实（无法从公开检索确认）：** 流式 `/api/v3/sauc/` 通道下 2.0 对应的 `X-Api-Resource-Id` 与 `model_name` 取值。公开可查的 resource_id 只有 `volc.bigasr.auc` / `volc.bigasr.auc_turbo`（录音文件类）和我们在用的 `volc.seedasr.sauc.duration`。**需要在火山引擎控制台/开通页面确认 2.0 的流式接入参数后再动手。**

### P1-2 · Volcengine：补上 `correct_words` 纠错词

火山的 `corpus.context` 除了我们已经在用的 `hotwords` 和 `context_type: dialog_ctx`，还支持 `correct_words` —— 形如 `{"deep seek": "DeepSeek"}` 的强制映射。

这正好对应词典里"用户明确知道 ASR 会听错成 X、想要 Y"的那类条目。目前这类需求只能靠后处理 LLM 或前端替换来做，下推到 ASR 层更省一次往返、也更准。

**建议：** 词典数据结构增加可选的"纠错映射"字段；Volcengine 走 `correct_words`，其他 Provider 保持现有后处理路径。

### P1-3 · 新增 Mistral Voxtral Transcribe 2

2026-02 发布，是本次调研里唯一一个"能力对得上、价格明显更低、且我们还没接"的国际线服务：

| 项 | 值 |
|---|---|
| 批量模型 | `voxtral-mini-latest` — $0.003/分钟 |
| 流式模型 | `voxtral-mini-transcribe-realtime-2602` — $0.006/分钟，可配延迟低至 sub-200ms |
| 语种 | 13 种，**含中文** |
| 精度 | FLEURS ≈ 4% WER；480ms 延迟下流式仅比批量差 1–2% |
| 接入 | `/v1/audio/transcriptions`（OpenAI 兼容风格） |

官方声称批量精度优于 GPT-4o mini Transcribe / Gemini 2.5 Flash / AssemblyAI Universal / Deepgram Nova。接入成本低（复用现有 OpenAI 兼容 client 骨架）。

**注意：** 官方宣传的对比基线是 2025 年的模型，且中文不是它的强项语种；接之前建议先用我们自己的中文样本跑一轮，再决定是否设为推荐项。

### P2-1 · Gemini 档位

- `gemini-3.1-flash-live-preview`（我们的 Gemini Live）仍是 Google 当前主力实时音频模型（2026-03 发布），**不需要动**。
- 批量侧的 `gemini-3.1-flash-lite-preview` 可以试 `gemini-3.5-flash-lite`（官方文档明确列为支持 audio 输入，且把 "audio transcription with context" 列为最佳用途之一）或 `gemini-3.6-flash`。
- 这属于"改个字符串就能测"的事，建议先在设置页把模型输入框保持自由填写（现在已经是 placeholder 形式，OK），做一轮内部 A/B 再改默认值。

### P2-2 · 本地路径：coli → transcribe.cpp

现状 [coli_client.rs](src-tauri/src/asr/coli_client.rs) 依赖外部 `coli` CLI + sherpa-onnx，模型是 **SenseVoice-small（2024-07 版）** 和 whisper-tiny.en。这两个在 2026 年已经明显落后。

**transcribe.cpp**（2026-06-30 由 Mozilla AI 发布，作者是 Handy 转写应用的维护者）是目前最值得关注的本地路径：
- ggml 运行时（llama.cpp 同族），支持 **16 个 ASR 模型族 / 60+ 变体**：Whisper、Parakeet、Canary、Moonshine、**Qwen3-ASR**、Cohere Transcribe、SenseVoice、Voxtral
- GPU 加速：**Metal（Apple Silicon）** / Vulkan / CUDA
- 支持流式与批量
- **提供 Rust binding** —— 对 Tauri 项目来说这是决定性优势，可以从"外部 CLI 进程 + JSON 管道"变成进程内调用，省掉一大堆错误处理和路径管理

配套的模型选择（中文场景）：
- **Qwen3-ASR-0.6B**：阿里 2026-01 开源，52 语种/方言。0.6B 版专为生产设计，离线/在线场景都能保持低 RTF。适合做默认本地模型。
- **Qwen3-ASR-1.7B**：20 个主流语种平均 WER 优于同期开源模型；中文方言上比 Doubao-ASR 错误率低约 20%（15.94 vs 19.85）。RTF < 0.3，但需要约 10–14GB 显存 —— 对 Mac 统一内存机型可行，对普通 Windows 笔记本偏重。
- **SenseVoice-small + MLX**：速度极致（有实测称 27 分钟中文播客 13.83 秒转完），但精度不如 Qwen3-ASR。可以留作"极速档"。

**建议：** 这是唯一一个"大工作量"项。可以先做一个 spike：用 transcribe.cpp 的 Rust binding + Qwen3-ASR-0.6B 跑通一条最小链路，实测 Mac 上的首字延迟和中文 CER，再决定是否替换 coli。

---

## 四、评估过但不建议引入的 Provider

| Provider | 状态 | 不建议原因 |
|---|---|---|
| **AssemblyAI** | Universal-3.5 Pro（批量 18 语种含中文，7.0% WER） | **流式不支持中文**。Universal-Streaming 多语种目前只有英/西/法/德/意/葡。中文流式只能退回 `whisper-rt`，没有优势。 |
| **Speechmatics Melia-1** | 56+ 语种原生 code-switching，批量 $0.129/小时（极便宜），7 月基准 6.4% WER 领先 | **仅批量，无 realtime 版本，且功能集缩减**。中英混说的 code-switching 能力对我们很有吸引力，但流式缺失是硬伤。可作为"录音文件重转写"场景的候选，不适合主链路。 |
| **Deepgram** | 官方文档当前列 Nova-3（支持 zh 系列全语种变体 + streaming）与 Flux（voice agent 专用，**不支持中文**） | Nova-3 中文可用但没有相对我们已有 Provider 的明显优势。第三方文章提到的 "Nova-4" 在官方文档中查无实据，暂不采信。 |
| **ElevenLabs Scribe v3** | 不存在 | 官方文档最新仍是 Scribe v2 / v2 Realtime，我们已在用。 |
| **Google Chirp 4** | 不存在 | Speech-to-Text release notes 最新条目仍停在 2025 年的 Chirp 3 GA，我们已在用 `chirp_3`。 |

---

## 五、跨 Provider 的最佳实践改进

调研中反复出现、且我们可以横向应用的几条：

1. **结构化热词优于 prompt 拼接。** OpenAI 新增 `keywords`、Soniox 有 `context.terms`、ElevenLabs 有 `keyterms`、Qwen 有 `vocabulary`、Volcengine 有 `corpus.context.hotwords`。目前只有 OpenAI 路径还在用 prompt 拼接，应当收敛。

2. **MiMo 目前完全没接热词**（`grep hotwords` 在 `mimo_client.rs` 无命中）。如果 MiMo 平台支持，应补上；如果不支持，UI 应像 Fun-ASR 那样**显式提示"该 Provider 会忽略词典"**，而不是静默失效。同理 Cohere、Gemini Live 也需要确认。

3. **"上下文增强"正在成为行业标配，且实现方式在从"词表匹配"转向"语义理解"。** 豆包 2.0 的 PPO 上下文、Qwen-Audio-3.0 的"基于声学证据与上下文的智能判断而非简单替换"（宣称热词准确率 >90%、多数场景 >99%）都是这个方向。这意味着**把最近几条识别历史喂给 ASR 的收益在变大**——我们在 Volcengine 和 Fun-ASR 上已经这么做了，值得推广到所有支持 context 的 Provider。

4. **延迟档位应该是用户可调的。** OpenAI 的 `delay`、Voxtral Realtime 的可配延迟、Soniox 的 `max_endpoint_delay_ms`（我们已有）都是同一类旋钮。语音输入法用户对"快"和"准"的偏好差异很大，统一抽象一个"延迟偏好"设置项可能比每家单独暴露参数更好。

5. **OpenAI 官方给的评测方法论值得照搬到我们的回归测试：** 用真实生产音频而非干净样本；每个目标语种单独测；把"空转录 / 截断 / 延迟"作为独立指标跟踪，而不是只看 WER。对输入法来说"空结果"和"吞字"比 WER 高几个点更致命。

---

## 六、价格参考（2026-08）

| Provider | 价格 | 备注 |
|---|---|---|
| StepAudio 2.5 ASR | **¥0.15/小时** | 比上一代 Step ASR 2 便宜 90%，MTP 技术，5 分钟音频 1 秒出结果 |
| Speechmatics Melia-1 | $0.129/小时 | 批量；每月 10 小时免费 |
| OpenAI gpt-transcribe | $0.0045/分钟（$0.27/小时） | |
| Mistral Voxtral Mini V2 | $0.003/分钟（$0.18/小时） | 批量 |
| Mistral Voxtral Realtime | $0.006/分钟（$0.36/小时） | 流式 |
| Deepgram Nova-3 | $0.0043/分钟批量、$0.0077/分钟流式 | |
| ElevenLabs Scribe | $0.40/小时 | 已降价 45% |
| Volcengine Seed-ASR 2.0 | ¥4.5/小时（报道值，QPS 10） | ⚠️ 未在官方定价页核实 |

StepAudio 的 ¥0.15/小时 值得注意——我们已经接了，但可能没作为"低成本默认"推荐给用户。

---

## 七、建议的执行顺序

1. **本周可做（小改动，风险低）**
   - Soniox 默认模型 → `stt-rt-v5`
   - Volcengine `corpus.context` 增加 `correct_words`
   - 排查 MiMo / Cohere / Gemini Live 的词典支持情况，不支持的在 UI 显式提示

2. **下一个迭代**
   - OpenAI 换代：`gpt-transcribe` / `gpt-live-transcribe` + `keywords` + `languages` + `delay`
   - Gemini 批量档位 A/B（3.1 Flash-Lite vs 3.5 Flash-Lite vs 3.6 Flash）

3. **需要先做核实/实验**
   - 到火山引擎控制台确认 Seed-ASR 2.0 的流式 `resource_id` 与 `model_name` → 确认后接入
   - Voxtral Transcribe 2 用自有中文样本实测 → 通过后新增 Provider

4. **规划中（大工作量）**
   - transcribe.cpp Rust binding + Qwen3-ASR-0.6B 的本地路径 spike

---

## 八、参考来源

**现有 Provider**
- [Soniox Models](https://soniox.com/docs/stt/models) · [Soniox v5 Real-Time](https://soniox.com/blog/soniox-v5-real-time)
- [OpenAI GPT Transcribe](https://developers.openai.com/api/docs/models/gpt-transcribe) · [Realtime transcription](https://developers.openai.com/api/docs/guides/realtime-transcription) · [File transcription](https://developers.openai.com/api/docs/guides/transcription) · [Advancing voice intelligence](https://openai.com/index/advancing-voice-intelligence-with-new-models-in-the-api/)
- [ElevenLabs Models](https://elevenlabs.io/docs/overview/models)
- [Google Chirp 3](https://docs.cloud.google.com/speech-to-text/docs/models/chirp-3) · [STT Release Notes](https://docs.cloud.google.com/speech-to-text/docs/release-notes)
- [Gemini Models](https://ai.google.dev/gemini-api/docs/models) · [Gemini 3.5 Flash-Lite](https://ai.google.dev/gemini-api/docs/models/gemini-3.5-flash-lite) · [Gemini 3.1 Flash Live](https://blog.google/innovation-and-ai/models-and-research/gemini-models/gemini-3-1-flash-live/)
- [阿里云 实时语音识别](https://help.aliyun.com/zh/model-studio/qwen-real-time-speech-recognition) · [Qwen-Audio-3.0-ASR-Flash 发布](https://www.163.com/dy/article/L36AS0Q405118HA4.html)
- [豆包语音识别模型 2.0](https://zhuanlan.zhihu.com/p/1981419964855502084) · [火山引擎 大模型流式识别 SDK](https://www.volcengine.com/docs/6561/1395846?lang=zh)
- [Cohere Transcribe](https://docs.cohere.com/docs/transcribe) · [cohere-transcribe-03-2026 发布说明](https://huggingface.co/blog/CohereLabs/cohere-transcribe-03-2026-release)
- [StepAudio 2.5 ASR Model Card](https://stepaudiollm.github.io/step-audio-2.5-asr/model-card/) · [StepAudio 2.5 ASR 发布](https://www.ithome.com/0/943/340.htm)
- [Xiaomi MiMo 语音识别文档](https://mimo.mi.com/docs/zh-CN/quick-start/usage-guide/audio/Speech-Recognition)

**候选 Provider**
- [Mistral Voxtral Transcribe 2](https://mistral.ai/news/voxtral-transcribe-2/)
- [Deepgram Models & Languages](https://developers.deepgram.com/docs/models-languages-overview)
- [Speechmatics Melia](https://www.speechmatics.com/company/articles-and-news/introducing-melia-multilingual-speech-to-text-model) · [Speechmatics Models](https://docs.speechmatics.com/speech-to-text/models)
- [AssemblyAI 流式多语种](https://www.assemblyai.com/docs/streaming/universal-streaming/multilingual-transcription)

**本地方案**
- [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp) · [发布解读](https://www.remio.ai/post/transcribe-cpp-release-brings-16-asr-model-families-to-one-cross-platform-ggml-runtime)
- [Qwen3-ASR](https://github.com/QwenLM/Qwen3-ASR) · [Qwen3-ASR 开源解读](https://zhuanlan.zhihu.com/p/2000326281414333812)
- [中文开源 ASR 模型横评](https://cloud.tencent.com/developer/article/2642961)
