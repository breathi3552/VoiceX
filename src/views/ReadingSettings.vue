<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { NButton, NInput, NSelect, NSlider, NSwitch } from 'naive-ui'
import { useI18n } from 'vue-i18n'
import { useSettingsStore } from '../stores/settings'
import { formatHotkey } from '../utils/hotkey'

interface TtsVoiceOption {
  id: string
  name: string
  language: string
}

interface ReadSelectionStatus {
  bound: boolean
  enabled: boolean
  conflictsWithDictation: boolean
  display: string | null
}

// The engine's own default rate, i.e. where the 1x mark sits on the stored
// 0..1 scale. Everything the sliders show is a multiple of it.
const DEFAULT_RATE = 0.5

const settingsStore = useSettingsStore()
const { t } = useI18n()

const isMacOS =
  navigator.platform?.toLowerCase().includes('mac') ||
  navigator.userAgent?.toLowerCase().includes('mac')

const voices = ref<TtsVoiceOption[]>([])
const voicesError = ref('')
const hotkeyStatus = ref<ReadSelectionStatus | null>(null)
const isRecording = ref(false)
const previewLoading = ref(false)
const previewError = ref('')
const showAdvanced = ref(false)

const ttsEnabled = computed({
  get: () => settingsStore.settings.ttsEnabled,
  set: (value: boolean) => {
    settingsStore.updateSetting('ttsEnabled', value)
    void applyHotkey()
  }
})

type ProviderValue = 'system' | 'volcengine'

const providerOptions = computed(() => [
  { label: t('reading.providerSystem'), value: 'system' },
  { label: t('reading.providerVolcengine'), value: 'volcengine' }
])

const ttsProviderType = computed({
  get: () => settingsStore.settings.ttsProviderType,
  set: (value: ProviderValue) => {
    settingsStore.updateSetting('ttsProviderType', value)
    // The voice list is per provider and shares nothing across them.
    void loadVoices(value)
  }
})

const isVolcengine = computed(() => settingsStore.settings.ttsProviderType === 'volcengine')

const voiceOptions = computed(() => {
  const listed = voices.value.map((voice) => ({
    label: `${voice.name} · ${voice.language}`,
    value: voice.id
  }))
  // The cloud provider has no default-voice concept, and an empty speaker is
  // not a valid request — so only the local engine gets that entry.
  return isVolcengine.value
    ? listed
    : [{ label: t('reading.voiceDefault'), value: '' }, ...listed]
})

// Voice identifiers do not carry across providers, so each keeps its own.
const ttsVoiceId = computed({
  get: () =>
    isVolcengine.value
      ? settingsStore.settings.volcTtsSpeaker
      : settingsStore.settings.systemTtsVoiceId,
  set: (value: string) =>
    isVolcengine.value
      ? settingsStore.updateSetting('volcTtsSpeaker', value)
      : settingsStore.updateSetting('systemTtsVoiceId', value)
})

const volcTtsApiKey = computed({
  get: () => settingsStore.settings.volcTtsApiKey,
  set: (value: string) => settingsStore.updateSetting('volcTtsApiKey', value)
})

const volcTtsResourceId = computed({
  get: () => settingsStore.settings.volcTtsResourceId,
  set: (value: string) => settingsStore.updateSetting('volcTtsResourceId', value)
})

// Rate and volume belong to the provider, not to the feature: engines differ
// in baseline speed and loudness, so tuning one must not move the other.
const rateMultiplier = computed({
  get: () =>
    round2(
      (isVolcengine.value
        ? settingsStore.settings.volcTtsRate
        : settingsStore.settings.systemTtsRate) / DEFAULT_RATE
    ),
  set: (value: number) => {
    const stored = clamp(value * DEFAULT_RATE, 0, 1)
    isVolcengine.value
      ? settingsStore.updateSetting('volcTtsRate', stored)
      : settingsStore.updateSetting('systemTtsRate', stored)
  }
})

const volumePercent = computed({
  get: () =>
    Math.round(
      (isVolcengine.value
        ? settingsStore.settings.volcTtsVolume
        : settingsStore.settings.systemTtsVolume) * 100
    ),
  set: (value: number) => {
    const stored = clamp(value / 100, 0, 1)
    isVolcengine.value
      ? settingsStore.updateSetting('volcTtsVolume', stored)
      : settingsStore.updateSetting('systemTtsVolume', stored)
  }
})

// System voice only — no cloud provider exposes pitch.
const pitchMultiplier = computed({
  get: () => round2(settingsStore.settings.systemTtsPitch),
  set: (value: number) => settingsStore.updateSetting('systemTtsPitch', clamp(value, 0.5, 2))
})

const clipboardFallback = computed({
  get: () => settingsStore.settings.ttsClipboardFallback,
  set: (value: boolean) => settingsStore.updateSetting('ttsClipboardFallback', value)
})

const displayHotkey = computed(
  () =>
    hotkeyStatus.value?.display ??
    formatHotkey(settingsStore.settings.ttsHotkeyConfig) ??
    'Option + Command + R'
)

const showConflict = computed(() => hotkeyStatus.value?.conflictsWithDictation ?? false)

function clamp(value: number, min: number, max: number) {
  return Math.min(max, Math.max(min, value))
}

function round2(value: number) {
  return Math.round(value * 100) / 100
}

async function applyHotkey() {
  try {
    hotkeyStatus.value = await invoke<ReadSelectionStatus>('apply_read_selection_hotkey', {
      config: settingsStore.settings.ttsHotkeyConfig,
      enabled: settingsStore.settings.ttsEnabled
    })
  } catch (error) {
    console.error('Failed to apply the reading hotkey', error)
  }
}

async function startRecording() {
  isRecording.value = true
  try {
    const result = await invoke<{ storage: string; display: string }>('record_hotkey')
    settingsStore.updateSetting('ttsHotkeyConfig', result.storage)
    await applyHotkey()
  } catch (error) {
    console.error('Hotkey record failed', error)
  } finally {
    isRecording.value = false
  }
}

async function resetHotkey() {
  settingsStore.updateSetting('ttsHotkeyConfig', null)
  await applyHotkey()
}

async function loadVoices(provider: ProviderValue = settingsStore.settings.ttsProviderType) {
  // The cloud provider is network-only, so its voice list works everywhere;
  // only the system voice needs macOS.
  if (!isMacOS && provider !== 'volcengine') {
    voices.value = []
    return
  }
  // Clear first: a slow reply must not leave the previous provider's voices on
  // screen, which is how system voices used to show up under the cloud engine.
  voices.value = []
  try {
    // Passed explicitly — the store's save is debounced, so the backend would
    // still read the previous provider from the database.
    voices.value = await invoke<TtsVoiceOption[]>('list_tts_voices', { provider })
    voicesError.value = ''
  } catch (error) {
    voices.value = []
    voicesError.value = error instanceof Error ? error.message : String(error)
  }
}

async function runPreview() {
  previewLoading.value = true
  previewError.value = ''
  try {
    // The backend reads the voice parameters from the store, so the debounced
    // save has to land before the preview starts or it auditions stale values.
    await settingsStore.forceSaveSettings()
    await invoke('preview_tts', { text: t('reading.previewText') })
  } catch (error) {
    previewError.value = error instanceof Error ? error.message : String(error)
  } finally {
    previewLoading.value = false
  }
}

async function stopPreview() {
  try {
    await invoke('stop_tts')
  } catch (error) {
    console.error('Failed to stop speech', error)
  }
}

onMounted(async () => {
  // The dictation hotkey may have changed on another page since this binding
  // was applied, so ask what the state actually is rather than assuming.
  try {
    hotkeyStatus.value = await invoke<ReadSelectionStatus>('read_selection_hotkey_status')
  } catch (error) {
    console.error('Failed to read the reading hotkey status', error)
  }
  await loadVoices()
})
</script>

<template>
  <div class="page settings-page reading-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('reading.title') }}</h1>
    </div>

    <div v-if="!isMacOS" class="surface-card asr-card">
      <div class="warning-box">{{ t('reading.unsupportedPlatform') }}</div>
    </div>

    <div class="surface-card asr-card">
      <div class="card-header">
        <div class="card-title">{{ t('reading.provider') }}</div>
        <div class="card-sub">{{ t('reading.providerSub') }}</div>
      </div>
      <div class="field-list">
        <div class="field-row">
          <div class="field-text">
            <div class="field-label">{{ t('reading.ttsProvider') }}</div>
          </div>
          <NSelect
            v-model:value="ttsProviderType"
            :options="providerOptions"
            size="small"
            class="field-control"
          />
        </div>

        <template v-if="isVolcengine">
          <!-- Plan §3.4: told once, when the engine is chosen, rather than
               asked on every read. -->
          <div class="notice-box">{{ t('reading.cloudPrivacy') }}</div>
          <div class="field-row">
            <div class="field-text">
              <div class="field-label">{{ t('reading.volcApiKey') }}</div>
              <div class="field-note">{{ t('reading.volcApiKeyNote') }}</div>
            </div>
            <NInput
              v-model:value="volcTtsApiKey"
              type="password"
              show-password-on="click"
              size="small"
              class="field-control"
              :placeholder="t('reading.volcApiKeyPlaceholder')"
            />
          </div>
          <div class="field-row">
            <div class="field-text">
              <div class="field-label">{{ t('reading.volcResourceId') }}</div>
              <div class="field-note">{{ t('reading.volcResourceIdNote') }}</div>
            </div>
            <NInput
              v-model:value="volcTtsResourceId"
              size="small"
              class="field-control"
              placeholder="seed-tts-2.0"
            />
          </div>
        </template>
      </div>
    </div>

    <div class="surface-card asr-card">
      <div class="card-header">
        <div class="card-title">{{ t('reading.general') }}</div>
        <div class="card-sub">{{ t('reading.generalSub') }}</div>
      </div>
      <div class="field-list">
        <div class="field-row">
          <div class="field-text">
            <div class="field-label">{{ t('reading.enabled') }}</div>
            <div class="field-note">{{ t('reading.enabledNote') }}</div>
          </div>
          <div class="field-control end">
            <NSwitch v-model:value="ttsEnabled" :disabled="!isMacOS" />
          </div>
        </div>

        <div class="field-row">
          <div class="field-text">
            <div class="field-label">{{ t('reading.hotkey') }}</div>
            <div class="field-note">{{ t('reading.hotkeyNote') }}</div>
            <div class="field-note">{{ t('reading.hotkeySystemNote') }}</div>
          </div>
          <div class="field-control end">
            <div class="hotkey-display" :class="{ recording: isRecording }">
              {{ isRecording ? t('reading.pressHotkey') : displayHotkey }}
            </div>
            <div class="hotkey-actions">
              <NButton
                :disabled="isRecording || !ttsEnabled || !isMacOS"
                size="small"
                @click="startRecording"
              >
                {{ t('reading.record') }}
              </NButton>
              <NButton
                v-if="settingsStore.settings.ttsHotkeyConfig && !isRecording"
                quaternary
                size="small"
                @click="resetHotkey"
              >
                {{ t('reading.clear') }}
              </NButton>
            </div>
          </div>
        </div>

        <div v-if="showConflict" class="warning-box">
          {{ t('reading.hotkeyConflict') }}
        </div>
      </div>
    </div>

    <div class="surface-card asr-card">
      <div class="card-header">
        <div class="card-title">{{ t('reading.voice') }}</div>
        <div class="card-sub">{{ t('reading.voiceSub') }}</div>
      </div>
      <div class="field-list">
        <div class="field-row">
          <div class="field-text">
            <div class="field-label">{{ t('reading.voiceLabel') }}</div>
            <div class="field-note">
              {{ isVolcengine ? t('reading.volcSpeakerNote') : t('reading.voiceNote') }}
            </div>
          </div>
          <NSelect
            v-model:value="ttsVoiceId"
            :options="voiceOptions"
            :disabled="!isMacOS && !isVolcengine"
            :tag="isVolcengine"
            filterable
            size="small"
            class="field-control"
          />
        </div>

        <div v-if="voicesError" class="warning-box">
          {{ t('reading.voiceLoadFailed') }} — {{ voicesError }}
        </div>

        <div class="field-row">
          <div class="field-text">
            <div class="field-label">{{ t('reading.rate') }}</div>
          </div>
          <div class="field-control end">
            <NSlider
              v-model:value="rateMultiplier"
              :min="0.5"
              :max="2"
              :step="0.05"
              :disabled="!isMacOS"
              class="slider"
            />
            <span class="slider-value">{{ rateMultiplier.toFixed(2) }}x</span>
          </div>
        </div>

        <!-- Volcengine's audio_params carry no pitch, so the row would be a
             control that quietly does nothing. -->
        <div v-if="!isVolcengine" class="field-row">
          <div class="field-text">
            <div class="field-label">{{ t('reading.pitch') }}</div>
          </div>
          <div class="field-control end">
            <NSlider
              v-model:value="pitchMultiplier"
              :min="0.5"
              :max="2"
              :step="0.05"
              :disabled="!isMacOS"
              class="slider"
            />
            <span class="slider-value">{{ pitchMultiplier.toFixed(2) }}x</span>
          </div>
        </div>

        <div class="field-row">
          <div class="field-text">
            <div class="field-label">{{ t('reading.volume') }}</div>
          </div>
          <div class="field-control end">
            <NSlider
              v-model:value="volumePercent"
              :min="0"
              :max="100"
              :step="5"
              :disabled="!isMacOS"
              class="slider"
            />
            <span class="slider-value">{{ volumePercent }}%</span>
          </div>
        </div>

        <div class="field-row">
          <div class="field-text">
            <div class="field-label">{{ t('reading.preview') }}</div>
            <div class="field-note">{{ t('reading.previewNote') }}</div>
          </div>
          <div class="field-control end">
            <NButton size="small" :disabled="!isMacOS" @click="stopPreview">
              {{ t('reading.previewStop') }}
            </NButton>
            <NButton
              :loading="previewLoading"
              :disabled="!isMacOS"
              type="primary"
              secondary
              size="small"
              @click="runPreview"
            >
              {{ t('reading.preview') }}
            </NButton>
          </div>
        </div>

        <div v-if="previewError" class="warning-box">
          {{ t('reading.previewFailed') }} — {{ previewError }}
        </div>
      </div>
    </div>

    <div class="surface-card asr-card">
      <button class="advanced-toggle" @click="showAdvanced = !showAdvanced">
        <span class="card-title">{{ t('reading.advanced') }}</span>
        <span class="advanced-chevron" :class="{ open: showAdvanced }">›</span>
      </button>
      <div v-if="showAdvanced" class="field-list advanced-body">
        <div class="card-sub">{{ t('reading.advancedSub') }}</div>
        <div class="field-row">
          <div class="field-text">
            <div class="field-label">{{ t('reading.clipboardFallback') }}</div>
            <div class="field-note">{{ t('reading.clipboardFallbackNote') }}</div>
          </div>
          <div class="field-control end">
            <NSwitch v-model:value="clipboardFallback" :disabled="!isMacOS" />
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
@import '../styles/asr-settings.css';

.settings-page {
  width: 100%;
  max-width: 1120px;
  padding-bottom: var(--spacing-2xl);
}

.field-control.end {
  display: flex;
  align-items: center;
  gap: var(--spacing-sm);
  justify-content: flex-end;
}

.hotkey-display {
  flex: 1;
  padding: 6px var(--spacing-lg);
  background-color: var(--color-bg-tertiary);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-md);
  font-family: ui-monospace, monospace;
  font-size: var(--font-md);
  color: var(--color-text-primary);
  min-height: 28px;
  display: flex;
  align-items: center;
}

.hotkey-display.recording {
  border-color: var(--color-accent);
  box-shadow: 0 0 0 2px var(--color-accent-light);
}

.hotkey-actions {
  display: flex;
  gap: var(--spacing-sm);
}

.slider {
  flex: 1;
}

.slider-value {
  width: 56px;
  text-align: right;
  font-size: var(--font-xs);
  color: var(--color-text-secondary);
  font-variant-numeric: tabular-nums;
}

.advanced-toggle {
  display: flex;
  align-items: center;
  gap: var(--spacing-sm);
  width: 100%;
  text-align: left;
  color: var(--color-text-primary);
}

.advanced-chevron {
  color: var(--color-text-tertiary);
  transition: transform var(--transition-fast);
}

.advanced-chevron.open {
  transform: rotate(90deg);
}

.advanced-body {
  margin-top: var(--spacing-md);
}
</style>
