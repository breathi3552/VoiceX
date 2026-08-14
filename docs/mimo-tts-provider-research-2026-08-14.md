# 小米 MiMo 语音合成接入调研（2026-08-14）

调研目标：确认 MiMo `mimo-v2.5-tts` 的协议细节与接入方式，验证与现有
播放管线（HTTP/SSE 流式 + MP3 解码）的兼容性。

**结论基于实测**——官方文档（mimo.mi.com，2026-08-14 查阅）没写长度上限、
没写鉴权替代形式，且长文本会**静默截断**，这些都是探出来的。
复现脚本：[`scripts/tts/mimo_probe.py`](../scripts/tts/mimo_probe.py)，
读取应用自己数据库里的 MiMo Key（与 ASR 同一份），跑的就是应用将来要跑的账号。

> **状态：已实现**（同日）。后端在
> [`src-tauri/src/tts/mimo.rs`](../src-tauri/src/tts/mimo.rs)。
> 音色表已用 `mimo::tests::voice_table_matches_what_the_service_accepts`
> 对真实账号逐个验证（9 个全过）；端到端链路用
> `mimo::tests::live_synthesis_decodes_and_plays` 验证
> （48 kHz 设备、pcm16 流重采样出声，8.16 秒音频）。
> 首版走 MP3 流上线后真机听感卡顿，已改为 pcm16 流，见 §2.1 的实测修正。

---

## 1. 协议速览

| 项 | 值 |
|---|---|
| 端点 | `POST https://api.xiaomimimo.com/v1/chat/completions`（与 MiMo ASR 同一端点） |
| 鉴权 | 文档写 `api-key: <key>`，实测 `Authorization: Bearer <key>` 同样有效（与 ASR 客户端一致，代码用 Bearer） |
| 模型 | `mimo-v2.5-tts`（预置音色）；另有 `-voicedesign`（描述定制音色）与 `-voiceclone`（少样本克隆），首版不接 |
| 请求形态 | chat 消息：**assistant 消息 = 要合成的文本**（必需，缺了直接 400 "messages must contain an assistant role for TTS model"）；**user 消息 = 风格指令**（可选，可整条省略） |
| 音频参数 | `audio: {format, voice}`。format 支持 `wav / mp3 / pcm / pcm16`（服务端错误消息确认的全集）；无语速/音调/音量/采样率参数 |
| 流式 | `stream: true` → SSE，音频在 `choices[0].delta.audio.data`（base64），终止符 `data: [DONE]`；非流式在 `message.audio.data` |
| 采样率 | 固定 24 kHz 单声道，无参数可改 |
| 计费 | 限时免费（正式定价未公布）；usage 按 token 报（completion ≈ 6.3 token/秒音频） |

## 2. 实测记录（关键结论）

### 2.1 流式必须用 pcm16，不能用 MP3 ⚠️ 实测修正

本文初版结论是「MP3 chunk 拼接可完整解码，`decode.rs` 管线原样复用」。
**该结论被真机听感推翻**（同日）：接入后播放有节奏性卡顿。复查数据：

| 同一句话 | 时长 |
|---|---|
| pcm16 流（真实时长） | **4.80 s** |
| MP3 流拼接后解码 | **6.03 s** |
| MP3 各 chunk 单独解码求和 | 5.05 s |

- 每个 SSE 音频 chunk 是**独立编码的 MP3 文件**（各自可解码、各带编码器
  延迟填充），不是连续帧流的切片。拼接解码时每个 ~340 ms 的 chunk 会
  多出 ~80 ms 静音——听感就是持续卡顿。
- 「能解码」不等于「解码结果正确」：初版只验证了字节流可解码，没对比
  时长。时长差 25% 就是当时漏掉的信号。
- **修正后的接入方式：流式用 `pcm16`**（裸 16-bit LE PCM，无任何 chunk
  框架可出错），跳过 MP3 解码器，转 f32 后直接进重采样器。付出的代价是
  不复用 `decode.rs`，换来的是边界零伪影。
- 首包延迟：短文本 690–935 ms，671 字仍是 699 ms（不随长度增长）；
  2000 字时 1.1 s，5000 字时 4.9 s。比火山（282–621 ms）、阿里（400–600 ms）慢一档。

### 2.2 长文本会静默截断（决定 `MAX_CHARS = 2000`）

| 文本长度 | finish_reason | 音频量 | 判定 |
|---|---|---|---|
| 1071 字 | stop | 15.7 MB WAV ≈ 328 s | ✅ 完整 |
| 2000 字 | stop | 4.30 MB MP3 ≈ 537 s | ✅ 完整（2000 字 ÷ ~3.7 字/秒 ≈ 540 s，对得上） |
| 5000 字 | stop | 3.69 MB MP3，**比 2000 字还少** | ❌ **静默截断**，且 finish_reason 照样报 stop |

`max_tokens=8192` 也救不回来。上限在 2000–5000 之间的某处，未继续二分——
取已证完整的 2000 作为 `max_chars`，超长选区走现有的截断机制。

### 2.3 其他实测确认

- **user 指令消息可省略**：只发 assistant 消息正常合成；空字符串 user 也接受。
  只发 user 消息则 400。
- **音色表**（服务端错误消息给出全集）：`mimo_default`、`冰糖`、`茉莉`、`苏打`、
  `白桦`（中文，ID 就是中文名）、`Mia`、`Chloe`、`Milo`、`Dean`（英文）。
  逐个实测全部可用；中英混合文本各音色均正常。
- **无任何韵律参数**：语速/音调只能靠自然语言指令（user 消息）控制，音量靠
  本地播放增益。设置页对 MiMo 隐藏语速滑块、提供"朗读风格"文本框。
- 非流式等待时间随文本线性增长（1071 字要 72 s），**必须走流式**。

## 3. 接入设计（已落地）

- **一个供应商 `mimo`，无模型下拉**（voicedesign/voiceclone 是另一种设置界面，
  等有"自定义音色"需求再说）。
- 流式格式选 **pcm16**（原因见 §2.1 的实测修正；初版选 mp3 已被推翻）。
  网络 chunk 可能把一个 16-bit 样本劈成两半，转换器带跨 chunk 的字节保态。
- **重采样是本供应商独有的一级**：服务固定 24 kHz 且设备常见拒开 24 kHz
  （本机 48 kHz 设备实测 `NoUsableSampleRate`），火山/阿里靠"向服务要设备采样率"
  绕开的问题在这里绕不开。`mimo.rs` 内置线性插值重采样器
  （24 kHz → 设备协商率，跨 chunk 保态），语音源带限远低于 12 kHz Nyquist，
  线性插值足够。
- 设置字段：`mimo_tts_api_key`（与 ASR key 同源不同字段，沿用既有隔离原则）、
  `mimo_tts_voice`、`mimo_tts_instruction`（风格指令）、`mimo_tts_volume`（本地增益）。
- 三线程骨架（HTTP 流 → 解码 → 播放）与错误上报路径照搬 `aliyun.rs`。

## 4. 风险与关注项

- **限时免费**：正式定价未公布（参考同平台 ASR ¥0.5/小时，预计走低价路线）。
  免费期结束后需回来评估，i18n 文案里已标注"限时免费"。
- **静默截断**：如服务端上限调整，`MAX_CHARS` 需随动；复测跑
  `mimo_probe.py length`。
- 首包 0.7–1.1 s 比火山/阿里慢，作为默认引擎体验略逊，适合作为免费备选。
- 产品新，限速策略未公布；探测期间未触发过限流。

## 5. 复测方式

```bash
# 协议探测（读应用数据库里的 key）
uv run python scripts/tts/mimo_probe.py all

# 后端 live 测试（会出声）
MIMO_TTS_API_KEY=... cargo test --lib mimo::tests::live -- --ignored --nocapture
MIMO_TTS_API_KEY=... cargo test --lib mimo::tests::voice_table -- --ignored --nocapture
```
