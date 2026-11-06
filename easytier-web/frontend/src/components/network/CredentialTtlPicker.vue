<script setup lang="ts">
import { computed, ref } from 'vue';
import { InputNumber, Select, SelectButton } from 'primevue';
import { useI18n } from 'vue-i18n';
import { DAY, HOUR, MAX_TTL_SECONDS, PERMANENT_TTL, TTL_PRESETS, customTtl } from '../../modules/credentialTtl';

// A credential's validity in seconds: a preset, permanent, or a custom
// number of hours or days entered below the presets.
const ttl = defineModel<number>({ required: true });
const { t } = useI18n();

const CUSTOM = -1;
const presetLabels: Record<number, string> = {
    [DAY]: 'web.network_detail.ttl_24h',
    [7 * DAY]: 'web.network_detail.ttl_7d',
    [30 * DAY]: 'web.network_detail.ttl_30d',
    [PERMANENT_TTL]: 'web.network_detail.ttl_permanent',
};
const options = computed(() => [
    ...TTL_PRESETS.map(value => ({ label: t(presetLabels[value]), value })),
    { label: t('web.network_detail.ttl_custom'), value: CUSTOM },
]);

const custom = ref(!(TTL_PRESETS as readonly number[]).includes(ttl.value));
const unit = ref(ttl.value % DAY === 0 ? DAY : HOUR);
const amount = ref(Math.max(1, Math.round(ttl.value / unit.value)) || 1);
const units = computed(() => [
    { label: t('web.network_detail.ttl_hours'), value: HOUR },
    { label: t('web.network_detail.ttl_days'), value: DAY },
]);

const choice = computed({
    get: () => custom.value ? CUSTOM : ttl.value,
    set: (value: number | null) => {
        if (value === null || value === undefined) return;
        custom.value = value === CUSTOM;
        ttl.value = custom.value ? customTtl(amount.value, unit.value) : value;
    },
});
const setAmount = (value: number | null) => {
    amount.value = Math.max(1, value ?? 1);
    ttl.value = customTtl(amount.value, unit.value);
};
const setUnit = (value: number) => {
    unit.value = value;
    amount.value = Math.min(amount.value, MAX_TTL_SECONDS / value);
    ttl.value = customTtl(amount.value, unit.value);
};
</script>

<template>
    <div class="ttl-picker">
        <SelectButton v-model="choice" :options="options" option-label="label" option-value="value" size="small"
            :allow-empty="false" class="ttl-presets" />
        <div v-if="custom" class="ttl-custom">
            <InputNumber :model-value="amount" :min="1" :max="MAX_TTL_SECONDS / unit" show-buttons size="small"
                input-class="ttl-amount" :aria-label="t('web.network_detail.ttl_custom')" @update:model-value="setAmount" />
            <Select :model-value="unit" :options="units" option-label="label" option-value="value" size="small"
                :aria-label="t('web.network_detail.ttl_unit')" @update:model-value="setUnit" />
            <span class="text-xs muted">{{ t('web.network_detail.ttl_custom_hint') }}</span>
        </div>
    </div>
</template>

<style scoped>
.ttl-picker { display: flex; flex-direction: column; gap: 8px; min-width: 0; }
.ttl-presets { flex-wrap: wrap; }
.ttl-custom { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; }
.ttl-custom :deep(.ttl-amount) { width: 5.5rem; }
</style>
