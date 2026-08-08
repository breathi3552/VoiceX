<script setup lang="ts">
import { computed } from 'vue'
import { NInput, NSelect, NSwitch } from 'naive-ui'
import { useI18n } from 'vue-i18n'
import { useSettingsStore } from '../../stores/settings'

const settingsStore = useSettingsStore()
const { t } = useI18n()

const qwenLocalCommandPath = computed({
  get: () => settingsStore.settings.qwenLocalCommandPath,
  set: (v: string) => settingsStore.updateSetting('qwenLocalCommandPath', v)
})

const qwenLocalModelDir = computed({
  get: () => settingsStore.settings.qwenLocalModelDir,
  set: (v: string) => settingsStore.updateSetting('qwenLocalModelDir', v)
})

const qwenLocalLanguage = computed({
  get: () => settingsStore.settings.qwenLocalLanguage,
  set: (v: string) => settingsStore.updateSetting('qwenLocalLanguage', v)
})

const qwenLocalUseDictionary = computed({
  get: () => settingsStore.settings.qwenLocalUseDictionary,
  set: (v: boolean) => settingsStore.updateSetting('qwenLocalUseDictionary', v)
})

// Leaving this on auto lets the model drift into English mid-utterance, so the
// blank option is deliberately labelled as not recommended rather than hidden.
const languageOptions = computed(() => [
  { label: t('asr.qwenLocalLanguageChinese'), value: 'Chinese' },
  { label: t('asr.qwenLocalLanguageEnglish'), value: 'English' },
  { label: t('asr.qwenLocalLanguageAuto'), value: '' }
])
</script>

<template>
  <div class="surface-card asr-card">
    <div class="card-header">
      <div class="card-title">{{ t('asr.qwenLocalConfiguration') }}</div>
      <div class="card-sub">{{ t('asr.qwenLocalConfigurationSub') }}</div>
    </div>
    <div class="field-list">
      <div class="field-row">
        <div class="field-text">
          <div class="field-label">{{ t('asr.qwenLocalModelDir') }}</div>
          <div class="field-note">{{ t('asr.qwenLocalModelDirNote') }}</div>
        </div>
        <NInput
          v-model:value="qwenLocalModelDir"
          placeholder="/path/to/qwen3-asr-0.6b"
          class="field-control"
        />
      </div>
      <div class="field-row">
        <div class="field-text">
          <div class="field-label">{{ t('asr.qwenLocalCommandPath') }}</div>
          <div class="field-note">{{ t('asr.qwenLocalCommandPathNote') }}</div>
        </div>
        <NInput
          v-model:value="qwenLocalCommandPath"
          placeholder="qwen-asr"
          class="field-control"
        />
      </div>
      <div class="field-row">
        <div class="field-text">
          <div class="field-label">{{ t('asr.qwenLocalLanguage') }}</div>
          <div class="field-note">{{ t('asr.qwenLocalLanguageNote') }}</div>
        </div>
        <NSelect
          v-model:value="qwenLocalLanguage"
          :options="languageOptions"
          size="small"
          class="field-control"
        />
      </div>
      <div class="field-row">
        <div class="field-text">
          <div class="field-label">{{ t('asr.qwenLocalUseDictionary') }}</div>
          <div class="field-note">{{ t('asr.qwenLocalUseDictionaryNote') }}</div>
        </div>
        <NSwitch v-model:value="qwenLocalUseDictionary" />
      </div>
    </div>
  </div>
</template>
