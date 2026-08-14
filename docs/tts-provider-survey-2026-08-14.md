# TTS 供应商调研：选中朗读的下一批云端引擎（2026-08-14）

> 调研目的：为「选中朗读」功能筛选下一批可接入的云端 TTS，优先满足三个条件：
> ① 中文与中英混合合成质量好；② 价格不贵；③ 尽量复用 ASR 侧已有的供应商账号/API key。
> 顺带盘点这些供应商的 ASR 能力（一个账号 ASR+TTS 两用）。
>
> 调研方式：两个并行 web 调研（官网文档 + 定价页 + 第三方评测/Arena 盲测/社区口碑），
> 2026-08-14 访问。价格统一换算为**每万字符**；换算基准：中文约 4 字/秒（1 万字符 ≈ 40 分钟音频），
> 1 USD ≈ 7.2 CNY。**所有价格与模型名接入前需在官网复核**，个别项已标注待验证。

## 0. 现状

- 已接入 TTS：火山引擎 Seed-TTS 2.0（现行旗舰，无需升级）、阿里云百炼（`qwen3-tts-flash` ¥0.8/万字符、`qwen-audio-3.0-tts-flash`）、macOS 本地（`say` / AVSpeech）。
- 播放管线支持：HTTP+SSE 流式 base64 MP3、WebSocket 流式音频、MP3/PCM 流式解码。凡是走这两类协议、输出 mp3/pcm 的服务，`volcengine.rs` / `aliyun.rs` 的骨架和 `decode.rs` / `playback.rs` 可全部复用。
- ASR 已有账号的供应商：火山、阿里 DashScope、Google（Cloud + Gemini 两种凭证）、OpenAI、ElevenLabs、Soniox、Cohere、StepFun、小米 MiMo。

## 1. 结论速览

**建议接入顺序**（综合 中文质量 × 价格 × 接入难度 × 账号复用）：

| 优先级 | 供应商 / 模型 | 为什么 | 价格/万字符 | 账号 |
|---|---|---|---|---|
| **P0** | 阿里 `cosyvoice-v3-flash`（增量模型） | 同 DashScope key、同 HTTP+SSE 链路，只是加一个模型下拉项；中文表现力国内第一梯队 | ¥1.0 | ✅ 已有 |
| **P0** | StepFun `step-tts-mini` / `step-tts-2` | key 已有；OpenAI 兼容 HTTP + WS 流式（首包 ~200ms）；中文原生、中英混读好；英文按 2 字母=1 字符计费，混合文本更便宜 | ¥0.9 / ¥2.8 | ✅ 已有 |
| **P0** | 小米 MiMo `mimo-v2.5-tts` | key 已有；**当前限时免费**；chat/completions 风格流式，支持 mp3/pcm | 免费（正式价未公布） | ✅ 已有 |
| **P1** | **MiniMax** `speech-2.6-turbo` / `speech-2.8-hd` | 中文质量天花板：Artificial Analysis Speech Arena 与 HF TTS Arena 盲测第一；WS 流式 + mp3/pcm，管线零阻抗。唯一代价是新开账号 | Turbo ¥1.8、HD ¥3.15（国内资源包） | ❌ 新增 |
| **P1** | SiliconFlow `CosyVoice2-0.5B` | 极便宜；OpenAI 兼容 HTTP `stream:true`，接入最简单；**附赠 SenseVoice ASR**，一个账号两用 | ≈¥1.5（按 UTF-8 字节计费，中文×3 已折算） | ❌ 新增 |
| **P2** | Soniox TTS `tts-rt-v2`（2026-04 新品） | key 已有、WS 流式；但中文质量无任何第三方评测，**先自测再决定** | ≈¥3.4 | ✅ 已有 |
| **P2** | Fish Audio `s2.1-pro` | `s2.1-pro-free` 免费 API 至 2026-08-31（请求可能被用于训练），零成本 A/B 中文质量；也有 ASR（$0.36/小时） | 免费档 / ¥3.2 | ❌ 新增 |
| **P2** | Azure Speech（晓晓 / DragonHD Flash zh-CN） | 中英混读最强（英文技术词发音是国产厂商短板、Azure 强项）；**每月 50 万字符免费**；但官方推荐走 SDK，裸 WS 是私有协议，Rust 侧成本中等偏高 | ¥1.15（标准）/ ¥1.6（HD）+ 免费额度 | ❌ 新增（ASR 同订阅两用） |
| P3 | Google Cloud Chirp 3 HD | 每月 1M 字符免费、按字符计费对中文划算；但流式要接双向 gRPC，非流式 REST 才简单 | ¥2.2 | ✅ 已有凭证 |
| P3 | Gemini TTS（`gemini-2.5-flash-preview-tts` 等） | AI Studio 免费层可白嫖，SSE 流式 PCM；非中文优化，质量中等 | 免费层 / ¥4.3+ | ✅ 已有 |
| 不建议 | OpenAI `gpt-4o-mini-tts` | 中文有"外国人口音"、多音字不稳；单请求 2000 token 上限对长选区不友好 | ≈¥4.3 | ✅ 已有 |
| 不建议 | ElevenLabs | 中文翻译腔明显（盲测 7.8/10），订阅制换算 ¥16/万字符，贵 5–20 倍 | ≈¥16 | ✅ 已有 |
| 不建议 | Mistral Voxtral TTS | 仅 9 种语言，以欧洲语言为主，中文大概率不支持 | ¥1.2 | — |
| 不建议 | Cartesia Sonic 3.5 | 低延迟标杆（<90ms）但中文是新补语言、零中文口碑 | ¥2.7–3.6（仅订阅） | — |
| 无产品 | Cohere | 无 TTS（2026-03 发的是 ASR 方向） | — | — |

**一句话结论**：零边际成本的三家（阿里 CosyVoice 增量、StepFun、MiMo）先接上；质量上限想再抬一档就新开 MiniMax；SiliconFlow 作为超低价兜底并顺便白嫖 SenseVoice ASR。

## 2. 已有账号供应商详情

### 2.1 阿里云百炼（增量模型，P0）

- 同一 DashScope key 全平台通用；各模型开通后 90 天内各有 100 万字符免费额度。
- `qwen3-tts` 没有"非 flash"版；值得加的增量：
  - `cosyvoice-v3-flash` ¥1.0/万、`cosyvoice-v3-plus` ¥2.0/万、`cosyvoice-v3.5-*`（2026 新，复刻+设计+指令）——中文表现力口碑国内第一梯队。
  - `qwen3-tts-instruct-flash`（+realtime WS 版）：flash 同价位加指令控风格，对朗读场景算免费升级。
  - `qwen3-tts-vc / vd`（声音复刻/设计）：将来想做"用户自定义音色"再考虑。
- 接入方式与现有 `aliyun.rs` 相同（HTTP+SSE base64 mp3），预计只需扩音色表与模型分派。
- 来源：help.aliyun.com/zh/model-studio/tts-model

### 2.2 阶跃星辰 StepFun（P0）

- 三档：`step-tts-mini` ¥0.9/万、`step-tts-2` ¥2.8/万（端到端，11 情感 17 风格）、`stepaudio-2.5-tts` ¥5.8/万（2026-05 旗舰）。
- 计费：1 汉字 = 1 字符，**2 个英文字母 = 1 字符**——中英混合文本实际更便宜。声音复刻 ¥9.9/音色。
- 接口：`POST /v1/audio/speech`（OpenAI 兼容，整段）+ WebSocket `/v1/realtime/audio` 流式（首包 ~200ms 口碑）。
- 中文口碑：国产第二梯队偏上，中英混读自然；音色库比火山/阿里小。
- 来源：platform.stepfun.com/docs/zh/pricing/details、platform.stepfun.ai/docs/en/guides/models/stepaudio-2.5-tts

### 2.3 小米 MiMo（P0，注意风险）

- `mimo-v2.5-tts`（精品音色）、`-voicedesign`（一句话定义音色）、`-voiceclone`（少样本克隆）。
- **当前限时免费**，正式定价未公布（参考同平台 V2.5-ASR ¥0.5/小时，走低价路线）。
- 接口：`POST /v1/chat/completions`（OpenAI chat 风格，audio 参数指定音色/格式），支持流式，输出 WAV/MP3/PCM。
- 风险：产品新，限速、稳定性、正式价格均待观察；免费期结束后需重新评估。
- 来源：mimo.mi.com/docs/zh-CN/news/v2.5-tts-release

### 2.4 Soniox（P2，先自测）

- 2026-04-23 发布 Soniox TTS（`tts-rt-v2`）：60+ 语言、主打字母数字/多语言切换准确、WS 实时流式。
- 定价 $4/1M 文本 token + $21.5/1M 音频 token ≈ $0.47/万字符（≈¥3.4）。
- 中文质量无第三方评测（产品太新）。key 已有、协议兼容，**试水成本低，值得快速验证中英混合效果**——它宣传的"字母数字/多语言切换"正好是朗读技术文本的痛点。
- 来源：soniox.com/blog/soniox-text-to-speech、soniox.com/pricing

### 2.5 Google（P3 两条线）

- **Gemini API 原生 TTS**（AI Studio key）：`gemini-3.1-flash-tts-preview`（$1/$20 per 1M token ≈ ¥8.6/万）、`gemini-2.5-flash-preview-tts`（≈¥4.3/万）。flash 系在 AI Studio 免费层免费（限速）。SSE 流式 PCM，首包 300–500ms 口碑。中文支持但非优化重点。
- **Google Cloud TTS**（GCP 凭证）：Chirp 3 HD $30/1M 字符 = ¥2.2/万；**每月 1M 字符免费长期有效**——个人朗读场景可能长期免费。但流式合成走双向 gRPC（`StreamingSynthesize`），Rust 接 gRPC 是额外工程量；REST 非流式版简单但首包=全段延迟。
- 来源：ai.google.dev/gemini-api/docs/pricing、cloud.google.com/text-to-speech/pricing

### 2.6 OpenAI / ElevenLabs（不建议做中文默认）

- OpenAI `gpt-4o-mini-tts`（最新快照 2025-12-15）：SSE base64 与现有链路完美匹配、≈¥4.3/万，但中文口音明显、多音字不稳、单请求 2000 token 上限。若要接，只作为"顺手支持"，不设为中文默认。
- ElevenLabs：`eleven_v3` / `flash_v2.5` 中文盲测自然度仅 7.8/10、翻译腔重；Creator 档换算 ¥16/万。英文朗读极佳，若未来加"英文文档朗读"场景可再评估。
- 火山引擎：确认无更新一代（现行旗舰仍是已接入的 Seed-TTS 2.0）；想做用户克隆音色可关注 Seed-ICL 2.0。

## 3. 新供应商详情

### 3.1 MiniMax（P1，质量天花板）

- 模型线（比外界普遍认知新两代）：`speech-2.8-hd/turbo`（2026 新，Sound Tags、录音棚音质）、`speech-2.6-hd/turbo`（2025-10，端到端 <250ms）、`speech-02` 仍在售。
- 中文口碑：**Artificial Analysis Speech Arena 与 Hugging Face TTS Arena 盲测第一**（超过 OpenAI/ElevenLabs）；中文自然度、情感控制公认第一梯队；40+ 语种无缝切换，中英混排是强项。
- 接口：HTTP T2A（≤1 万字符/请求）+ WebSocket T2A 流式 + 异步批量；输出 mp3/pcm/flac/wav——与现有 WS 管线直接兼容。
- 价格：国际站 HD $1/万、Turbo $0.6/万；**国内站资源包 HD ≈¥3.15/万、Turbo ≈¥1.8/万**（年包更低）。克隆约 ¥99/音色。
- ASR：官方文档未列独立 ASR API，按"仅 TTS"规划，无账号复用加分。
- 来源：minimax.io/news/minimax-speech-28、platform.minimaxi.com/docs/guides/pricing

### 3.2 SiliconFlow 硅基流动（P1，低价 + ASR 两用）

- 在售：`FunAudioLLM/CosyVoice2-0.5B`（中文最佳开源之一）、`fnlp/MOSS-TTSD-v0.5`（中英对话）；国际站另有 IndexTTS-2 等。
- 接口：OpenAI 兼容 `/audio/speech`，HTTP `stream:true` 流式，输出 mp3/wav/pcm/opus——**接入难度全场最低**（无需 WS）。
- 价格：CosyVoice2 约 $7.15/1M UTF-8 字节 → 中文 ≈¥1.5/万字符（**按字节计费，中文×3 已折算**）。新用户赠 ¥14。
- ASR：有 `FunAudioLLM/SenseVoiceSmall`（曾长期免费）、TeleSpeechASR——**一个账号 ASR+TTS 两用**。
- 风险：托管服务并发/稳定性口碑一般（社区反馈偶有排队）。
- 来源：docs.siliconflow.cn/cn/api-reference/audio/create-speech、siliconflow.com/pricing

### 3.3 Azure Speech（P2，中英混合最强 + 免费额度）

- zh-CN 音色：晓晓/云希等传统神经音色口碑经久；HD 线 DragonHD（zh-CN 有 Xiaochen、Yunfan，GA）、DragonHD Flash（zh-CN/en-US 专优，14 个 zh-CN HD 音色）；延迟 <300ms。
- **中英混读是全场最强项**：XiaoxiaoMultilingual 等多语种音色中英切换自然，英文技术词发音好——正好补国产厂商短板。
- 价格：标准神经 $16/1M 字符 ≈ ¥1.15/万；HD 2026-03 起降为 $22/1M ≈ ¥1.6/万；**F0 免费层每月 50 万字符**。ASR 同订阅（免费 5 小时/月）。
- 接入难点：官方推荐 Speech SDK，裸 WebSocket 是私有协议；Rust 侧要么啃私有 WS，要么用 REST（一次性返回、非流式）。是唯一的障碍。
- 来源：learn.microsoft.com/azure/ai-services/speech-service/high-definition-voices

### 3.4 Fish Audio（P2，零成本试用窗口）

- 最新 `s2.1-pro`（83 语种）；**`s2.1-pro-free` 免费 API 至少到 2026-08-31**（Fair Use、无 SLA、请求可能被用于训练——朗读内容会出网，需在 UI 告知）。
- 付费价 $15/1M UTF-8 字节 → 中文 ≈¥3.2/万（字节计费陷阱同上）。
- 接口：REST + WS 流式，mp3/pcm。中文可用性好、克隆强，拟人度低于 MiniMax/火山第一梯队。
- ASR：`transcribe-1` $0.36/小时——账号两用。
- 来源：docs.fish.audio/developer-guide/models-pricing/pricing-and-rate-limits、fish.audio/blog/s2-1-pro-free-api/

### 3.5 腾讯云 / 讯飞（国产备选，暂缓）

- **腾讯云**：精品音色 ¥0.30/万、大模型音色 ¥0.55–1.2/万、超自然 ¥4.9–6.5/万；免费额度大（大模型音色 100 万字符/3 个月）；WS 实时合成。中文中上但社区存在感弱于字节/MiniMax。ASR 侧已有 2026-08-08 混元 ASR 3.0 调研（内测限制大）。若想要"一个腾讯账号 ASR+TTS"可等混元 ASR 正式版一起评估。
- **讯飞**：超拟人合成（副语言：呼吸/叹气/口误），WS 双向流式，mp3/pcm；但官网价格页 JS 渲染未抓到具体单价，需登录控制台确认。价格查清前暂缓。

### 3.6 其他

- 七牛云：无有竞争力的自研 TTS，跳过。出门问问：C 端工具为主、API 存在感低，跳过。腾讯 TokenHub 播客 TTS：双人播客场景，非通用，跳过。

## 4. 计费与工程注意事项

1. **UTF-8 字节计费陷阱**：Fish Audio 与 SiliconFlow 按字节计费，中文 1 字 = 3 字节，名义价 ×3 才是中文实际价。本文表格已折算。
2. **StepFun 反向优惠**：2 英文字母 = 1 字符，中英混合文本比名义价便宜。
3. **订阅制 vs 按量**：ElevenLabs、Cartesia 只有订阅制，低用量下单价被月费抬高；按量制（阿里/StepFun/MiniMax/SiliconFlow）更适合个人工具。
4. **长文本切分**：OpenAI 2000 token/请求、MiniMax HTTP 1 万字符/请求；选中朗读可能选中整篇文章，接入层需要按供应商上限切分排队（现有 controller 已有分段朗读机制可挂）。
5. **key 复用是账号层面的**：设置里各 provider 仍是独立 key 字段（同 `volc_tts_api_key` / `aliyun_tts_api_key` 模式），用户把 ASR 侧的 key 粘过来即可；可考虑在 UI 上加"从 ASR 设置复制 key"的快捷入口。
6. **数据合规提示**：Fish 免费档明确说请求可能用于训练；接入时按 tts_plan 的既有做法，在选择云端引擎时一次性告知数据去向。

## 5. ASR 侧顺带发现

- **Fish Audio `transcribe-1`**（$0.36/小时）与 **SiliconFlow SenseVoiceSmall**（曾长期免费）：接了它们的 TTS 后，ASR 可作低成本备选顺带接入。
- **Azure STT**：同订阅免费 5 小时/月，若接 Azure TTS 可两用。
- **MiniMax**：无官方 ASR API，纯 TTS 供应商。
- 其余 ASR 格局与 [asr-provider-landscape-2026-08.md](asr-provider-landscape-2026-08.md)（08-08）无变化；腾讯混元 ASR 3.0 维持"暂不接入、关注正式版"结论（见 [tencent-hy-asr3-research.md](tencent-hy-asr3-research.md)）。

## 6. 建议的落地节奏

1. **第一批（零新增账号，预计各半天～一天）**：阿里 `cosyvoice-v3-flash`（扩模型下拉）→ StepFun（OpenAI 兼容 HTTP 先通、WS 流式后补）→ MiMo（免费期先接先用）。
2. **第二批（新账号，质量/价格两个方向各选一）**：MiniMax Turbo/HD（质量天花板）+ SiliconFlow CosyVoice2（超低价兜底 + SenseVoice ASR 两用）。
3. **穿插**：Soniox TTS 与 Fish 免费档各花一小时自测中英混合质量，数据好再决定是否正式接入。
4. **观望**：Azure（等愿意啃私有 WS 协议或接受非流式 REST 时再上）、讯飞（等价格确认）、腾讯（等混元 ASR 正式版一起打包评估）。
