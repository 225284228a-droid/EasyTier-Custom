<script setup lang="ts">
import { nextTick, ref } from 'vue';
import { InputText } from 'primevue';
import { useI18n } from 'vue-i18n';

// A card's name with an optional quieter line under it. An editable name
// turns into an input on click; Enter or leaving it saves, Escape cancels.
const props = defineProps<{
    name: string;
    secondary?: string;
    /** What editing starts from when it differs from the shown name. */
    value?: string;
    /** Saves the new name; editing stays open when it throws. */
    rename?: (value: string) => Promise<void>;
    placeholder?: string;
    maxlength?: number;
}>();

const { t } = useI18n();
const editing = ref(false);
const saving = ref(false);
const draft = ref('');
const input = ref<{ $el: HTMLInputElement }>();

const begin = async (current: string) => {
    draft.value = current;
    editing.value = true;
    await nextTick();
    input.value?.$el.focus();
    input.value?.$el.select();
};
const commit = async () => {
    if (!editing.value || saving.value || !props.rename) return;
    saving.value = true;
    try {
        await props.rename(draft.value.trim());
        editing.value = false;
    } catch {
        // The caller reports the failure; the draft stays for another try.
    } finally {
        saving.value = false;
    }
};
</script>

<template>
    <div class="entity-names">
        <InputText v-if="editing" ref="input" v-model="draft" class="entity-name-input" size="small" autocomplete="off"
            :maxlength="maxlength" :placeholder="placeholder" :disabled="saving" :aria-label="t('web.console.rename')"
            @keydown.enter.prevent="commit" @keydown.esc.prevent="editing = false" @blur="commit" />
        <button v-else-if="rename" type="button" class="entity-name editable" v-tooltip.top="t('web.console.rename_hint')"
            @click="begin(value ?? name)"><span>{{ name }}</span><i class="pi pi-pencil" aria-hidden="true"></i></button>
        <span v-else class="entity-name" :title="name"><span>{{ name }}</span></span>
        <span class="entity-sub" :title="secondary">{{ secondary }}</span>
    </div>
</template>
