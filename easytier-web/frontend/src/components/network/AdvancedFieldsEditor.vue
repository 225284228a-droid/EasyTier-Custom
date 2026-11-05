<script setup lang="ts">
import { Button, InputNumber, SelectButton } from 'primevue';
import { useI18n } from 'vue-i18n';
import {
    ADVANCED_DEFAULTS, ADVANCED_PARAMETERS, ADVANCED_SWITCHES, UNSUPPORTED, advancedLabel, isDefaultValue, sharedKey, valueKey,
    type AdvancedDistribution, type AdvancedValue,
} from '../../modules/meshAdvanced';

// In `change` mode a setting left alone is `undefined` and keeps each
// member's value, and an untouched row shows what the members share; in
// `preset` mode every setting has a value. `null` runs with the core's
// default in both.
const props = defineProps<{
    mode: 'change' | 'preset';
    distribution?: AdvancedDistribution;
    disabled?: boolean;
}>();
const values = defineModel<Record<string, any>>({ required: true });
const { t } = useI18n();

const parameterOf = (field: string) => ADVANCED_PARAMETERS.find(parameter => parameter.field === field);
const groups = [
    { title: 'flags_switch', fields: ADVANCED_SWITCHES },
    { title: 'web.mesh_network.parameters', fields: ADVANCED_PARAMETERS.map(parameter => parameter.field) },
];

const valueName = (field: string, value: AdvancedValue) => {
    if (typeof value === 'boolean') return t(value ? 'web.mesh_network.value_on' : 'web.mesh_network.value_off');
    const option = parameterOf(field)?.options?.find(option => option.value === value);
    return option ? t(option.label) : String(value);
};
const defaultName = (field: string) => t('web.mesh_network.value_default_is', { value: valueName(field, ADVANCED_DEFAULTS[field]) });

// A choice equal to the default is the default, so it is offered once.
const choicesOf = (field: string) => {
    const options: AdvancedValue[] = parameterOf(field)?.options?.map(option => option.value) ?? [true, false];
    return [
        { key: 'default', label: defaultName(field) },
        ...options.filter(value => !isDefaultValue(field, value)).map(value => ({ key: String(value), label: valueName(field, value) })),
    ];
};

const touched = (field: string) => props.mode === 'change' && values.value[field] !== undefined;
/** The chosen value's key, else what every member shares; `undefined` when they differ. */
const shownKey = (field: string) => props.mode === 'preset' || touched(field)
    ? valueKey(field, values.value[field])
    : sharedKey(props.distribution?.[field]);

const set = (field: string, value: unknown) => {
    values.value = { ...values.value, [field]: value ?? null };
};
const revert = (field: string) => {
    const next = { ...values.value };
    delete next[field];
    values.value = next;
};
const choose = (field: string, key: string) => {
    if (key === 'default') return set(field, null);
    const options: AdvancedValue[] = parameterOf(field)?.options?.map(option => option.value) ?? [true, false];
    set(field, options.find(value => String(value) === key));
};

const shownNumber = (field: string) => {
    const key = shownKey(field);
    return key === undefined || key === 'default' ? null : Number(key);
};
const numberPlaceholder = (field: string) => shownKey(field) === undefined
    ? t('web.mesh_network.value_mixed') : defaultName(field);

const counts = (field: string) => [...props.distribution?.[field] ?? []]
    .sort(([a], [b]) => rank(a) - rank(b));
const rank = (key: string) => key === 'default' ? 0 : key === UNSUPPORTED ? 2 : 1;
const countName = (field: string, key: string) => {
    if (key === 'default') return t('web.mesh_network.value_default');
    if (key === UNSUPPORTED) return t('web.mesh_network.value_unsupported');
    if (key === 'true' || key === 'false') return valueName(field, key === 'true');
    const option = parameterOf(field)?.options?.find(option => String(option.value) === key);
    return option ? t(option.label) : key;
};
/** No member can store it: there is nothing to change. */
const unavailable = (field: string) => {
    const fieldCounts = props.distribution?.[field];
    return !!fieldCounts?.get(UNSUPPORTED) && fieldCounts.size === 1;
};
</script>

<template>
    <div class="advanced-editor">
        <section v-for="group in groups" :key="group.title" class="advanced-group">
            <h3>{{ t(group.title) }}</h3>
            <div class="advanced-grid">
                <div v-for="field in group.fields" :key="field" class="advanced-row"
                    :class="{ touched: touched(field), unavailable: unavailable(field) }">
                    <div class="advanced-info">
                        <div class="advanced-name">
                            <label :for="`advanced-${field}`">{{ t(advancedLabel(field)) }}</label>
                            <span v-if="!parameterOf(field)" class="pi pi-question-circle advanced-help" v-tooltip.top="t(`${field}_help`)"></span>
                        </div>
                        <div v-if="distribution && counts(field).length" class="advanced-counts">
                            <span v-for="[key, count] in counts(field)" :key="key" class="advanced-count"
                                :class="{ unsupported: key === UNSUPPORTED, current: key === shownKey(field) }"
                                v-tooltip.top="key === UNSUPPORTED ? t('web.mesh_network.unsupported_hint') : undefined">
                                {{ countName(field, key) }} <strong>{{ count }}</strong></span>
                        </div>
                    </div>
                    <div class="advanced-control">
                        <template v-if="parameterOf(field)?.kind === 'number'">
                            <InputNumber :input-id="`advanced-${field}`" :model-value="shownNumber(field)"
                                :min="parameterOf(field)?.min" :max="parameterOf(field)?.max" :use-grouping="false" size="small" fluid
                                class="advanced-number" :placeholder="numberPlaceholder(field)"
                                :disabled="disabled || unavailable(field)" @update:model-value="set(field, $event)" />
                            <Button :label="t('web.mesh_network.value_default')" size="small"
                                :severity="shownKey(field) === 'default' ? undefined : 'secondary'" :outlined="shownKey(field) !== 'default'"
                                :disabled="disabled || unavailable(field)" @click="set(field, null)" />
                        </template>
                        <SelectButton v-else :id="`advanced-${field}`" :model-value="shownKey(field)" :options="choicesOf(field)"
                            option-label="label" option-value="key" :allow-empty="false" size="small"
                            :disabled="disabled || unavailable(field)" :aria-label="t(advancedLabel(field))"
                            @update:model-value="choose(field, $event)" />
                        <Button v-if="mode === 'change'" icon="pi pi-undo" text rounded size="small" severity="secondary"
                            class="advanced-revert" :class="{ idle: !touched(field) }" :disabled="disabled"
                            :aria-label="t('web.mesh_network.revert_edit')" v-tooltip.top="t('web.mesh_network.revert_edit')"
                            @click="revert(field)" />
                    </div>
                </div>
            </div>
        </section>
    </div>
</template>

<style scoped>
.advanced-editor { display: flex; flex-direction: column; gap: 22px; }
.advanced-group h3 { margin: 0 0 10px; font-size: 13px; font-weight: 600; color: var(--p-text-muted-color); }
.advanced-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(100%, 400px), 1fr)); gap: 8px; }
.advanced-row {
    display: grid; grid-template-columns: minmax(0, 1fr) auto; align-items: center; gap: 12px; min-width: 0;
    padding: 10px 8px 10px 14px; border: 1px solid var(--console-border); border-radius: 8px; background: var(--console-panel);
    transition: border-color .15s, box-shadow .15s;
}
.advanced-row.touched { border-color: var(--p-primary-color); box-shadow: inset 3px 0 0 var(--p-primary-color); }
.advanced-row.unavailable .advanced-name { color: var(--p-text-muted-color); }
.advanced-info { min-width: 0; }
.advanced-name { display: flex; align-items: center; gap: 6px; font-size: 13px; font-weight: 500; overflow-wrap: anywhere; }
.advanced-help { font-size: 12px; color: var(--p-text-muted-color); cursor: help; }
.advanced-counts { display: flex; flex-wrap: wrap; gap: 4px; margin-top: 6px; }
.advanced-count {
    display: inline-flex; align-items: center; gap: 4px; padding: 0 7px; border-radius: 999px; line-height: 20px;
    font-size: 11px; color: var(--p-text-muted-color); background: color-mix(in srgb, var(--p-text-color) 6%, transparent);
}
.advanced-count strong { font-weight: 600; color: var(--p-text-color); }
.advanced-count.current { color: var(--p-primary-color); background: color-mix(in srgb, var(--p-primary-color) 12%, transparent); }
.advanced-count.unsupported { color: var(--p-orange-600); background: color-mix(in srgb, var(--p-orange-500) 14%, transparent); cursor: help; }
.advanced-count.unsupported strong { color: inherit; }
.advanced-control { display: flex; align-items: center; justify-content: flex-end; gap: 6px; min-width: 0; }
.advanced-control :deep(.p-selectbutton) { flex-wrap: nowrap; }
.advanced-control :deep(.p-togglebutton) { white-space: nowrap; }
.advanced-number { width: 9rem; flex-shrink: 1; min-width: 6rem; }
.advanced-revert.idle { visibility: hidden; }
@media (max-width: 560px) {
    .advanced-row { grid-template-columns: minmax(0, 1fr); padding-right: 12px; }
    .advanced-control { justify-content: flex-start; }
    .advanced-number { flex: 1; width: auto; }
}
</style>
