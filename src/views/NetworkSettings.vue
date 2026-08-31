<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { NButton, NInput, NRadio, NRadioGroup } from 'naive-ui'
import { useI18n } from 'vue-i18n'

type ProxyMode = 'direct' | 'system' | 'custom'

interface NetworkProxyConfig {
  mode: ProxyMode
  customProxy: string
}

const { t } = useI18n()
const mode = ref<ProxyMode>('custom')
const customProxy = ref('http://127.0.0.1:7890')
const saving = ref(false)
const loading = ref(true)
const status = ref('')
const statusError = ref(false)

const isCustom = computed(() => mode.value === 'custom')
const canSave = computed(() => {
  if (saving.value || loading.value) return false
  if (!isCustom.value) return true
  return /^http:\/\/[^\s/]+(?::\d+)?(?:\/.*)?$/i.test(customProxy.value.trim())
})

async function loadProxyConfig() {
  loading.value = true
  status.value = ''
  statusError.value = false
  try {
    const config = await invoke<NetworkProxyConfig>('get_network_proxy_config')
    mode.value = config.mode
    customProxy.value = config.customProxy || 'http://127.0.0.1:7890'
  } catch (error) {
    statusError.value = true
    status.value = t('network.loadFailed', { error: error instanceof Error ? error.message : String(error) })
  } finally {
    loading.value = false
  }
}

async function saveProxyConfig() {
  if (!canSave.value) {
    statusError.value = true
    status.value = t('network.invalidProxy')
    return
  }

  saving.value = true
  status.value = ''
  statusError.value = false
  try {
    const config = await invoke<NetworkProxyConfig>('set_network_proxy_config', {
      config: {
        mode: mode.value,
        customProxy: customProxy.value.trim()
      }
    })
    mode.value = config.mode
    customProxy.value = config.customProxy
    status.value = t('network.saved')
  } catch (error) {
    statusError.value = true
    status.value = t('network.saveFailed', { error: error instanceof Error ? error.message : String(error) })
  } finally {
    saving.value = false
  }
}

onMounted(loadProxyConfig)
</script>

<template>
  <div class="page settings-page network-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('network.title') }}</h1>
    </div>

    <div class="surface-card network-card">
      <div class="card-header">
        <div class="card-title">{{ t('network.proxyTitle') }}</div>
        <div class="card-sub">{{ t('network.proxySub') }}</div>
      </div>

      <div class="field-list">
        <NRadioGroup v-model:value="mode" :disabled="loading || saving" class="proxy-mode-group">
          <label class="proxy-mode-option">
            <NRadio value="direct">{{ t('network.modeDirect') }}</NRadio>
            <span class="field-note">{{ t('network.modeDirectNote') }}</span>
          </label>
          <label class="proxy-mode-option">
            <NRadio value="system">{{ t('network.modeSystem') }}</NRadio>
            <span class="field-note">{{ t('network.modeSystemNote') }}</span>
          </label>
          <label class="proxy-mode-option">
            <NRadio value="custom">{{ t('network.modeCustom') }}</NRadio>
            <span class="field-note">{{ t('network.modeCustomNote') }}</span>
          </label>
        </NRadioGroup>

        <div v-if="isCustom" class="field-row align-start">
          <div class="field-text">
            <div class="field-label">{{ t('network.customProxy') }}</div>
            <div class="field-note">{{ t('network.customProxyNote') }}</div>
          </div>
          <NInput
            v-model:value="customProxy"
            :placeholder="t('network.customProxyPlaceholder')"
            :disabled="loading || saving"
            class="proxy-input"
            @keyup.enter="saveProxyConfig"
          />
        </div>

        <div class="network-actions">
          <div v-if="status" class="field-note" :class="{ 'status-error': statusError, 'status-ok': !statusError }">
            {{ status }}
          </div>
          <NButton type="primary" secondary size="small" :loading="saving" :disabled="!canSave" @click="saveProxyConfig">
            {{ t('network.save') }}
          </NButton>
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

.network-card {
  max-width: 860px;
}

.proxy-mode-group {
  display: grid;
  gap: 10px;
}

.proxy-mode-option {
  display: grid;
  grid-template-columns: auto 1fr;
  column-gap: 10px;
  row-gap: 3px;
  align-items: start;
  padding: 12px 14px;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-md);
  background: rgba(255, 255, 255, 0.02);
  cursor: pointer;
}

.proxy-mode-option .field-note {
  grid-column: 2;
}

.proxy-input {
  width: min(430px, 100%);
}

.network-actions {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 12px;
  min-height: 30px;
}

.status-error {
  color: #f87171;
}

.status-ok {
  color: #4ade80;
}
</style>
