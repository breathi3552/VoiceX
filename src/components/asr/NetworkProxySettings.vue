<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { NButton, NInput } from 'naive-ui'

const proxy = ref('http://127.0.0.1:7890')
const saving = ref(false)
const status = ref('')

onMounted(async () => {
  try {
    proxy.value = await invoke<string>('get_http_proxy')
  } catch (error) {
    status.value = error instanceof Error ? error.message : String(error)
  }
})

async function saveProxy() {
  saving.value = true
  status.value = ''
  try {
    proxy.value = await invoke<string>('set_http_proxy', { proxy: proxy.value })
    status.value = proxy.value.trim() ? 'Proxy saved' : 'Proxy disabled'
  } catch (error) {
    status.value = error instanceof Error ? error.message : String(error)
  } finally {
    saving.value = false
  }
}
</script>

<template>
  <div class="surface-card asr-card">
    <div class="card-header">
      <div class="card-title">HTTP Proxy</div>
      <div class="card-sub">Gemini Live WebSocket and Google Cloud STT OAuth/gRPC use this explicit proxy. Leave empty to disable.</div>
    </div>
    <div class="field-list">
      <div class="field-row">
        <div class="field-text">
          <div class="field-label">Proxy URL</div>
          <div class="field-note">Default: http://127.0.0.1:7890 · HTTP CONNECT supported</div>
        </div>
        <div class="proxy-control">
          <NInput v-model:value="proxy" placeholder="http://127.0.0.1:7890" />
          <NButton size="small" :loading="saving" @click="saveProxy">Save</NButton>
        </div>
      </div>
      <div v-if="status" class="field-note proxy-status">{{ status }}</div>
    </div>
  </div>
</template>

<style scoped>
@import '../../styles/asr-settings.css';
.proxy-control {
  display: flex;
  align-items: center;
  gap: 8px;
  width: min(420px, 100%);
}
.proxy-status { text-align: right; }
</style>
