<script setup lang="ts">
import { computed } from 'vue'
import { LocalConfigBatch } from 'easytier-frontend-lib'
import type ApiClient from '../modules/api'
const props = defineProps<{ api: ApiClient }>()
const client = computed(() => props.api.get_local_config_client())
</script>

<template>
  <div class="console-page"><section class="console-panel"><LocalConfigBatch :client="client" /></section></div>
</template>
