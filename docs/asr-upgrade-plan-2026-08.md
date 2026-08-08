# VoiceX ASR 升级实施方案（2026-08-08）

承接 [asr-provider-landscape-2026-08.md](asr-provider-landscape-2026-08.md) 的调研结论，本文回答两个具体问题：

1. OpenAI 升级到底要改什么——模型名？接口？协议？
2. 本地模型（Qwen3-ASR-0.6B / 小红书 FireRedASR）选型与验证方案

> **状态更新（2026-08-08 晚）**：本文为动手前的方案推断，部分内容已被实测修正——
> GA 接入无需 ephemeral token、`turn_detection` 必须为 `null`、本地模型内存与运行时结论均有变化。
> **以 [asr-upgrade-results-2026-08.md](asr-upgrade-results-2026-08.md) 为准。**

---

# 一、OpenAI 升级：两条路径成本差一个数量级

**一句话结论：批量路径只需改模型名 + 加两个字段；Realtime 路径必须做协议迁移，而且这不是"升级"，是"修一个大概率已经坏了的功能"。**

## 1.1 批量路径 `/v1/audio/transcriptions` —— 协议不变

我们的实现 [openai_client.rs:61](../src-tauri/src/asr/openai_client.rs:61) 是标准 multipart + `response_format: json` + 解析 `{text}`。这套在新模型下**完全不用动**。

需要改的只有三处：

| 项 | 现在 | 改成 |
|---|---|---|
| 模型名 | `gpt-4o-transcribe` | `gpt-transcribe` |
| 热词 | 拼进 `prompt` 字符串（[openai_client.rs:109](../src-tauri/src/asr/openai_client.rs:109) `build_transcription_prompt`） | 独立的 `keywords` 数组字段 |
| 语种 | 单值 `language` | `languages` 数组（支持中英混说） |

官方给的参数语义：

- `prompt` — "unstructured context about the recording"（场景描述，**不该放词表**）
- `keywords` — "literal terms you expect to hear"，数组，例：`["premium plan", "AC-42", "billing"]`
- `languages` — "expected input languages"，数组，例：`["en", "fr"]`；我们的典型值是 `["zh", "en"]`

约束：官方明确 "Keep each keyword on one line and don't include `<`, `>`, a carriage return, or a line feed." —— 我们的词典条目需要按这个规则清洗，逻辑跟 [elevenlabs_client.rs:139](../src-tauri/src/asr/elevenlabs_client.rs:139) 的 `cleaned_elevenlabs_keyterms` 基本同构，可以抽公用。

**收益**：官方 Common Voice（22 语种）WER 从 `whisper-1` 的 40.37% 降到 `gpt-transcribe` 的 19.27%；价格 $0.0045/分钟。同时把热词从"prompt 里的自然语言暗示"变成"结构化字面词条"，命中率应有明显提升——而且反过来让 `prompt` 恢复它本来的作用（描述场景），不再被 200 条词表稀释。

**⚠️ 待实测**：`keywords` / `languages` 在 **multipart/form-data** 下的编码方式官方文档只给了 JSON 形态的例子。大概率是重复字段名（`form.text("keywords[]", w)` 或重复 `keywords`），跟我们 ElevenLabs 那边 `form.text("keyterms", ...)` 重复追加的写法一致。**动手前先用 curl 打一发确认**，别照猜的写。

**工作量**：小。一个函数 + config 字段 + UI 下拉。

## 1.2 Realtime 路径 —— 协议迁移，且优先级是"修 bug"

### 先说问题

OpenAI 官方 deprecations 页写得很明确：

> Realtime API Beta —— **Shutdown date: May 12, 2026** —— 替代品：Realtime API (GA)
> `OpenAI-Beta: realtime=v1` header —— **Shutdown date: May 12, 2026**

而我们的代码：

- [openai_realtime_client.rs:77](../src-tauri/src/asr/openai_realtime_client.rs:77) 仍在发 `OpenAI-Beta: realtime=v1`
- [openai_realtime_client.rs:351](../src-tauri/src/asr/openai_realtime_client.rs:351) 仍在用 `/realtime/transcription_sessions` 换 ephemeral token
- [openai_realtime_client.rs:421](../src-tauri/src/asr/openai_realtime_client.rs:421) 的 session payload 是 beta 的扁平结构

**结论：OpenAI Realtime 这条路径大概率从 2026-05-12 起就已经不可用了。** 第一步不是改代码，是**拿真 key 实测一次确认现状**——如果确实已坏，这项优先级要从"P0 升级"提到"P0 缺陷修复"。

### 迁移清单

| # | 项 | Beta（我们现在） | GA（目标） |
|---|---|---|---|
| 1 | Header | `OpenAI-Beta: realtime=v1` | **删除** |
| 2 | Ephemeral 端点 | `POST {base}/realtime/transcription_sessions` | `POST {base}/realtime/client_secrets` |
| 3 | 换票请求体 | 扁平 `{input_audio_format, input_audio_transcription, turn_detection}` | `{"session": {"type":"transcription", "audio":{"input":{"transcription":{...}}}}}` |
| 4 | 换票响应 | `client_secret.value` | 顶层 `value`（形如 `ek_...`） |
| 5 | WS URL | `wss://.../realtime` | `wss://api.openai.com/v1/realtime`，GA 下带 `?model=` 查询参数（transcription session 是否必需 **待实测**） |
| 6 | Session 配置 | 建票时一次性带上 | 连上后发 `session.update` 事件 |
| 7 | 音频格式 | 字符串 `"pcm16"` | 对象 `{"type":"audio/pcm","rate":24000}` |
| 8 | 配置嵌套 | `input_audio_transcription` / `turn_detection` 平铺在顶层 | 全部收进 `session.audio.input.{format, transcription, turn_detection}` |
| 9 | `include` | 顶层 `include` | `session.include` |
| 10 | 服务端事件名 | `transcription_session.updated` | 应统一为 `session.updated`（[:117](../src-tauri/src/asr/openai_realtime_client.rs:117) 需改）**待实测** |

GA 的 `session.update` 目标形态（官方文档原文）：

```json
{
  "type": "session.update",
  "session": {
    "type": "transcription",
    "audio": {
      "input": {
        "format": { "type": "audio/pcm", "rate": 24000 },
        "transcription": {
          "model": "gpt-live-transcribe",
          "prompt": "A customer support call about a premium plan and account AC-42.",
          "keywords": ["premium plan", "AC-42", "billing"],
          "languages": ["en", "fr"],
          "delay": "low"
        },
        "turn_detection": null
      }
    }
  }
}
```

### 好消息

- **采样率不用动**。GA 例子用 24000，我们已经在 [openai_realtime_client.rs:252](../src-tauri/src/asr/openai_realtime_client.rs:252) 用 `resample_to_24k` 重采样到 24k 了，正好对上。
- **音频收发与事件循环大概率不用动**。`input_audio_buffer.append` / `.commit` / `conversation.item.input_audio_transcription.delta` / `.completed` 在 GA 下应保持同名（**待实测确认**）。也就是说 [handle_runtime_payload](../src-tauri/src/asr/openai_realtime_client.rs:461) 里那套累积器、以及我们辛苦处理过的 "benign empty commit race"（[:476](../src-tauri/src/asr/openai_realtime_client.rs:476)）逻辑都能保留。
- 改动集中在 `build_transcription_session_request` + 建票函数 + header 三处，**不是重写**。

### 新增能力

- **模型**：`gpt-live-transcribe`（官方推荐，边说边出 delta）/ `gpt-transcribe`（整段提交后转写，且能返回检测到的语种）。建议 realtime 默认前者。
- **`delay` 档位**：`minimal` / `low` / `medium` / `high` / `xhigh`，直接就是"延迟 ↔ 准确率"旋钮。对语音输入法这是个很值钱的设置项，建议直接暴露给用户。
- `keywords` / `languages` 与批量路径同构，可复用同一套词典清洗代码。

**工作量**：中。核心是 3 个函数，但**必须配合真实联调**——上面标了 4 处"待实测"，纯看文档写完必然要返工。

## 1.3 建议执行顺序

```
1. 拿真 key 实测现有 realtime 路径 → 确认是否已失效（决定优先级）
2. curl 验证批量路径 keywords/languages 的 multipart 编码
3. 改批量路径（小、独立、可先发）
4. 改 realtime 路径（需联调）
5. 词典清洗逻辑抽公用（OpenAI keywords / ElevenLabs keyterms 共用）
```

---

# 二、本地模型：选型与验证方案

## 2.1 先给结论

**主线选 Qwen3-ASR-0.6B，走 `qwen-asr` 这个纯 Rust 推理引擎。FireRedASR 不作为落地候选，只作为"精度天花板"参照组。**

理由不是精度，是**可打包性**——这对桌面应用是一票否决项。

## 2.2 两个候选的真实差距

| 维度 | Qwen3-ASR-0.6B | FireRedASR2 / 2S（小红书） |
|---|---|---|
| 中文精度 | 普通话 SOTA 级；1.7B 在中文方言上比 Doubao-ASR 错误率低约 20%（15.94 vs 19.85） | **更强**：FireRedASR-LLM(8.3B) CER 3.05%、AED(1.1B) 3.18%，相对前 SOTA Seed-ASR 降 8.4% |
| 覆盖 | 52 种语言/方言 | 普通话 + 20+ 方言 + 中英混说 + 歌词；2S 还整合了 VAD/LID/Punc |
| 许可 | Apache-2.0 | Apache-2.0 |
| **推理运行时** | **纯 Rust（`qwen-asr` crate，MIT）**；另有纯 C 版、ggml 版、MLX 版 | **仅 PyTorch/Python**；`runtime/triton_tensorrt` 是服务端方案 |
| **打包进 Tauri** | ✅ 直接 `cargo add`，无外部进程 | ❌ 要么捆 Python 运行时，要么自己做 ONNX/ggml 移植 |
| 流式 | ✅ 2 秒 chunk + 5-token 回滚（C/Rust 实现原生支持）；官方 PyTorch 侧流式仅 vLLM 后端 | 2S 声称流式/非流式兼容，但仓库未明确文档化 |
| 输入长度 | 长音频分段（`-S 20/30`） | AED ≤60s，LLM ≤30s |
| 模型体积/内存 | 0.6B BF16 静态 2.77 GiB；1.7B 6.87 GiB | 1.1B / 8.3B，8.3B 桌面不现实 |

**FireRedASR 精度确实更好，但它没有任何非 Python 的推理路径。** 把它塞进 VoiceX 意味着捆一个 Python 运行时或自己做算子移植——这个成本远超它带来的 CER 收益，而且跟 CLAUDE.md 里"跨平台（macOS/Windows）"的要求直接冲突。

所以它的正确用法是：**验证阶段用 Python 跑一遍，作为"本地能达到的最好中文精度"的参照线**，用来判断 Qwen3-ASR-0.6B 离天花板还差多少。如果差距小到用户无感，选型就此定案；如果差距大到离谱，再重新评估是否值得投入移植成本。

## 2.3 为什么是 `qwen-asr` 而不是 transcribe.cpp

上一份简报里我推荐的是 transcribe.cpp。深入看完之后**改推 `qwen-asr`**：

| | `qwen-asr`（antirez 的 C 版 + huanglizhuo 的 Rust 移植） | transcribe.cpp |
|---|---|---|
| 语言 | 纯 Rust（也有纯 C 原版） | C++ + ggml，Rust 是 binding |
| 依赖 | **只要 libc**（C 版需 BLAS：macOS Accelerate / Linux OpenBLAS） | ggml + 各 GPU 后端（Metal/Vulkan/CUDA） |
| 分发 | crates.io 上的 `qwen-asr`，`cargo install qwen-asr-cli` | 需自行编译 + 从 HF 下 GGUF |
| Qwen3-ASR 流式 | ✅ 原生支持（2s chunk + prefix rollback） | ⚠️ 流式家族列的是 Moonshine / Nemotron / Parakeet，**Qwen3-ASR 不在流式列表** |
| 模型广度 | 只有 Qwen3-ASR | 16 个模型族 / 60+ 变体 |

对我们这个"只要一个好用的中文本地模型 + 要流式 + 要能塞进 Tauri"的需求，`qwen-asr` 的窄而深明显更合适。transcribe.cpp 的价值在模型广度，那是以后想做"本地模型任选"时才用得上的东西。

**性能参考**（M5 Pro，0.6B）：28.2 秒音频 613ms 转完，46× 实时。M3 Max 上 C 版：11 秒文件 1.4s（8× 实时）。对我们典型的 3–30 秒短句，这个速度绰绰有余。

## 2.4 已知风险（验证要重点打的靶子）

1. **Windows 支持存疑** —— Rust 移植的文档只提 macOS/Linux，C 原版针对 macOS NEON + Linux AVX 优化，作者明确不做 MPS。CLAUDE.md 要求新功能考虑 macOS/Windows 双平台。**这是最大的单点风险**，必须早验。
2. **内存占用偏高** —— 0.6B BF16 静态 2.77 GiB。VoiceX 是常驻的输入法工具，常驻 2.8G 对不少用户不可接受。需要看有没有量化路径（MLX 侧有 5-bit 量化在 1.7B 上做到 1.32% WER 的报告，说明量化空间存在）。
3. **热词/词典能力弱** —— 只有"system prompt biasing"，作者自己形容是 "very soft" guidance。这意味着本地模式下我们现有的词典体系会大幅退化。需要想清楚补偿方案（例如本地识别 + 后处理 LLM 纠错，或本地模式下明确降级提示）。
4. **模型分发** —— 0.6B BF16 safetensors 体积不小，不可能塞进安装包，需要设计首次使用时下载 + 校验 + 断点续传的流程。现在 `coli` 的模型管理逻辑（[coli_client.rs:27](../src-tauri/src/asr/coli_client.rs:27) 那几个 `*_CHECK_FILE` 常量）可以参考但要重做。

## 2.5 验证方案（Spike）

**目标：用最小成本回答"能不能上"，而不是做出一个能发布的功能。** 预期 3–5 天。

### 阶段 0：环境与素材准备（0.5 天）

建一份**中文语音输入法专属测试集**，这是整个验证的地基，别用公开测试集糊弄——公开集的音频特征跟"用户按住热键对着笔记本内置麦说一句话"差很远。

建议构成（各 20–30 条，总计 100–150 条）：
- 日常口语短句（3–10 秒）
- 技术词密集句（含项目里真实出现的英文术语、库名、缩写）
- 中英混说
- 带口音/语速快
- 噪声环境（咖啡厅、风扇、键盘声）

每条配人工校对的 ground truth。**这份素材是可复用资产**——以后评估任何 Provider 都用它，也能直接用来验证第一部分 OpenAI 升级的效果。

### 阶段 1：精度摸底（1 天，纯离线，不碰 VoiceX 代码）

用 CLI 跑，不写任何集成代码：

```bash
cargo install qwen-asr-cli
```

跑四组对比：

| 组 | 用途 |
|---|---|
| Qwen3-ASR-0.6B | 候选主线 |
| Qwen3-ASR-1.7B | 看"加钱能买到多少精度" |
| FireRedASR（Python 跑） | 本地精度天花板参照 |
| 现有 coli（SenseVoice-small） | 我们要超越的基线 |
| Volcengine（现有云端默认） | 用户实际体感的参照系 |

指标：CER（中文按字）、专有名词召回率、**空结果率 / 吞字率**（对输入法比 CER 更致命）。

**Go/No-Go 判据**：Qwen3-ASR-0.6B 的 CER 必须显著优于现有 coli 基线；与 FireRedASR 的差距若在 1.5 个百分点以内，则选型定案，不再考虑移植 FireRedASR。

### 阶段 2：工程可行性（1–2 天）

这一步跟精度无关，纯粹验证"能不能塞进 VoiceX"：

1. **Windows 编译** ← 最高优先级，先做这个。在 Windows 上 `cargo build` 一遍。**这一项失败，整个方案要重新考虑。**（注：按 [voicex_windows_build_verification](../.claude/memory) 的既有结论，不在 Mac 上交叉编译，需在真 Windows 环境验证）
2. 作为 **library crate** 引入一个空白 Rust 项目，确认不是只有 CLI 可用
3. 测量：模型加载耗时、常驻内存峰值（`0.6B` 声称 2.77 GiB，实测确认）、模型文件体积
4. 跑通流式模式，测**首字延迟**和**热键松开 → 终稿**的时间（这才是输入法的核心指标，不是 RTF）

**Go/No-Go 判据**：Windows 能编译且能跑；常驻内存在可接受范围内（或找到量化路径）。

### 阶段 3：接入形态决策（0.5 天，产出文档不产出代码）

拿阶段 1、2 的数据回答：

- 替换 `coli` 还是并存？（倾向并存一段时间，`coli` 的 SenseVoice 极速档仍有价值）
- `AsrProviderType` 加新枚举值，还是复用 `Coli` 走引擎选择子项？
- 本地模式下词典怎么办？——`qwen-asr` 的 prompt biasing 太软，是否需要接后处理 LLM 补偿
- 模型下载/校验流程怎么设计
- 内存占用是否需要"用完即卸载"策略（跟常驻输入法的即时性冲突，需权衡）

### 不在本次验证范围内

- UI/设置页
- 模型自动下载
- 多模型任选
- transcribe.cpp（等以后要做"本地模型任选"时再评估）

## 2.6 建议

**先做阶段 2 的第 1 步（Windows 编译）再做阶段 1。** 精度再好，编不出 Windows 版对我们也是死路——把最可能否决方案的风险放在最前面，能省下大量白做的功夫。

---

## 参考来源

**OpenAI**
- [Deprecations](https://developers.openai.com/api/docs/deprecations)（Realtime Beta / `OpenAI-Beta` header 2026-05-12 下线）
- [Realtime transcription 指南](https://developers.openai.com/api/docs/guides/realtime-transcription)（GA session.update 形态）
- [File transcription 指南](https://developers.openai.com/api/docs/guides/speech-to-text)（`keywords` / `languages`）
- [GPT Transcribe 模型页](https://developers.openai.com/api/docs/models/gpt-transcribe)
- [Create client secret](https://developers.openai.com/api/reference/resources/realtime/subresources/client_secrets/methods/create)
- [Realtime API 开发者笔记](https://developers.openai.com/blog/realtime-api)

**本地模型**
- [QwenLM/Qwen3-ASR](https://github.com/QwenLM/Qwen3-ASR)（官方，Apache-2.0，流式需 vLLM）
- [antirez/qwen-asr](https://github.com/antirez/qwen-asr)（纯 C，MIT，流式 2s chunk）
- [huanglizhuo/QwenASR](https://github.com/huanglizhuo/QwenASR)（纯 Rust 移植，crates.io: `qwen-asr`）
- [FireRedTeam/FireRedASR](https://github.com/FireRedTeam/FireRedASR)（Apache-2.0，PyTorch）
- [FireRedASR2S 介绍](https://www.aipuzi.cn/ai-news/fireredasr2s.html)
- [handy-computer/transcribe.cpp](https://github.com/handy-computer/transcribe.cpp)
