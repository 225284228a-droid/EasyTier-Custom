<script setup lang="ts">
import { computed } from 'vue';
import { Checkbox, InputNumber, Select, SelectButton } from 'primevue';
import { useI18n } from 'vue-i18n';
import {
    ADVANCED_PARAMETERS, ADVANCED_SWITCHES, advancedLabel, valueKey,
    type AdvancedDistribution, type AdvancedParameter,
} from '../../modules/meshAdvanced';

// In `change` mode a setting left alone is `undefined` and keeps each
// member's value; in `preset` mode every setting has a value and `null`
// runs with the core's default.
const props = defineProps<{
    mode: 'change' | 'preset';
    distribution?: AdvancedDistribution;
    disabled?: boolean;
}>();
const values = defineModel<Record<string, any>>({ required: true });
const { t } = useI18n();

type Choice = 'keep' | 'default' | 'on' | 'off';
const switchChoices = computed(() => [
    props.mode === 'change'
        ? { value: 'keep', label: t('web.mesh_network.value_keep') }
        : { value: 'default', label: t('web.mesh_network.value_default') },
    { value: 'on', label: t('web.mesh_network.value_on') },
    { value: 'off', label: t('web.mesh_network.value_off') },
]);
const switchChoice = (field: string): Choice => {
    const value = values.value[field];
    if (value === undefined) return props.mode === 'change' ? 'keep' : 'default';
    if (value === null) return 'default';
    return value ? 'on' : 'off';
};
const setSwitch = (field: string, choice: Choice) => {
    const next = { ...values.value };
    if (choice === 'keep') delete next[field];
    else next[field] = choice === 'default' ? null : choice === 'on';
    values.value = next;
};

const parameterTouched = (field: string) => values.value[field] !== undefined;
const setParameterTouched = (parameter: AdvancedParameter, touched: boolean) => {
    const next = { ...values.value };
    if (touched) next[parameter.field] = null;
    else delete next[parameter.field];
    values.value = next;
};
const setParameter = (field: string, value: unknown) => {
    values.value = { ...values.value, [field]: value ?? null };
};
const optionsOf = (parameter: AdvancedParameter) =>
    (parameter.options ?? []).map(option => ({ label: t(option.label), value: option.value }));

const distributionText = (field: string) => {
    const counts = props.distribution?.[field];
    if (!counts?.size) return '';
    const name = (key: string) => {
        if (key === 'default') return t('web.mesh_network.value_default');
        if (key === 'true') return t('web.mesh_network.value_on');
        if (key === 'false') return t('web.mesh_network.value_off');
        const option = ADVANCED_PARAMETERS.find(parameter => parameter.field === field)?.options
            ?.find(option => valueKey(option.value) === key);
        return option ? t(option.label) : key;
    };
    return [...counts].map(([key, count]) => `${name(key)} ${count}`).join(' · ');
};
</script>

<template>
    <div class="advanced-editor">
        <section class="advanced-group">
            <h3>{{ t('flags_switch') }}</h3>
            <div class="advanced-grid">
                <div v-for="field in ADVANCED_SWITCHES" :key="field" class="advanced-row"
                    :class="{ touched: mode === 'change' && values[field] !== undefined }">
                    <div class="min-w-0">
                        <div class="advanced-name">
                            <span>{{ t(advancedLabel(field)) }}</span>
                            <span class="pi pi-question-circle text-xs muted" v-tooltip.top="t(`${field}_help`)"></span>
                        </div>
                        <div v-if="distribution" class="advanced-distribution">{{ distributionText(field) }}</div>
                    </div>
                    <SelectButton :model-value="switchChoice(field)" :options="switchChoices" option-label="label"
                        option-value="value" :allow-empty="false" size="small" :disabled="disabled"
                        :aria-label="t(advancedLabel(field))" @update:model-value="setSwitch(field, $event)" />
                </div>
            </div>
        </section>
        <section class="advanced-group">
            <h3>{{ t('web.mesh_network.parameters') }}</h3>
            <div class="advanced-grid">
                <div v-for="parameter in ADVANCED_PARAMETERS" :key="parameter.field" class="advanced-row"
                    :class="{ touched: mode === 'change' && parameterTouched(parameter.field) }">
                    <div class="min-w-0">
                        <div class="advanced-name">
                            <Checkbox v-if="mode === 'change'" :model-value="parameterTouched(parameter.field)" binary
                                :input-id="`advanced-touch-${parameter.field}`" :disabled="disabled"
                                @update:model-value="setParameterTouched(parameter, $event)" />
                            <label :for="mode === 'change' ? `advanced-touch-${parameter.field}` : `advanced-${parameter.field}`">
                                {{ t(advancedLabel(parameter.field)) }}</label>
                        </div>
                        <div v-if="distribution" class="advanced-distribution">{{ distributionText(parameter.field) }}</div>
                    </div>
                    <Select v-if="parameter.kind === 'select'" :input-id="`advanced-${parameter.field}`"
                        :model-value="values[parameter.field] ?? null" :options="optionsOf(parameter)"
                        option-label="label" option-value="value" size="small" class="advanced-input"
                        :disabled="disabled || (mode === 'change' && !parameterTouched(parameter.field))"
                        @update:model-value="setParameter(parameter.field, $event)" />
                    <InputNumber v-else :input-id="`advanced-${parameter.field}`"
                        :model-value="values[parameter.field] ?? null" :min="parameter.min" :max="parameter.max"
                        :use-grouping="false" size="small" class="advanced-input"
                        :placeholder="t('web.mesh_network.value_default')"
                        :disabled="disabled || (mode === 'change' && !parameterTouched(parameter.field))"
                        @update:model-value="setParameter(parameter.field, $event)" />
                </div>
            </div>
        </section>
    </div>
</template>

<style scoped>
.advanced-editor { display: flex; flex-direction: column; gap: 20px; }
.advanced-group h3 { margin: 0 0 10px; font-size: 14px; }
.advanced-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(100%, 420px), 1fr)); gap: 8px; }
.advanced-row {
    display: flex; align-items: center; justify-content: space-between; gap: 12px; min-width: 0;
    padding: 8px 12px; border: 1px solid var(--console-border); border-radius: 6px;
}
.advanced-row.touched { border-color: var(--p-primary-color); }
.advanced-name { display: flex; align-items: center; gap: 6px; font-size: 13px; overflow-wrap: anywhere; }
.advanced-distribution { margin-top: 2px; font-size: 11px; color: var(--p-text-muted-color); }
.advanced-input { width: 10rem; flex-shrink: 0; }
@media (max-width: 640px) {
    .advanced-row { flex-direction: column; align-items: stretch; }
    .advanced-input { width: 100%; }
}
</style>
