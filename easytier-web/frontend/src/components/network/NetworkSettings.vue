<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import { v4 as uuidv4 } from 'uuid';
import { Button, Checkbox, ConfirmDialog, InputSwitch, InputText, Message, Password, Select, Skeleton, Tag, useConfirm, useToast } from 'primevue';
import { useRouter } from 'vue-router';
import { useI18n } from 'vue-i18n';
import type { MeshCoreChange, MeshMember, MeshSyncResult } from '../../modules/api';
import { moveIntoSubnet, parseVirtualSubnet, toNetworkCidr } from '../../modules/cidr';
import { failureMessage } from '../../modules/networkDiscovery';
import { failedResults, resultSeverity, useMeshNetwork } from '../../modules/meshNetworkContext';

const { t } = useI18n();
const toast = useToast();
const confirm = useConfirm();
const router = useRouter();
const { api, networkName, network, view, reload } = useMeshNetwork();

// The select shows no option for an empty value, so the default has a name.
const DEFAULT_ALGORITHM = 'default';
const algorithms = computed(() => [
    { label: t('web.mesh_network.value_default'), value: DEFAULT_ALGORITHM },
    { label: 'AES-GCM', value: 'aes-gcm' },
    { label: 'AES-256-GCM', value: 'aes-256-gcm' },
    { label: 'ChaCha20', value: 'chacha20' },
    { label: 'XOR', value: 'xor' },
]);

interface Form {
    display_name: string;
    network_name: string;
    network_secret: string;
    no_password: boolean;
    secure_mode: boolean;
    disable_encryption: boolean;
    encryption_algorithm: string;
    virtual_subnet: string;
}
const form = ref<Form>();
const formOf = (): Form | undefined => view.value && {
    display_name: view.value.core.display_name ?? '',
    network_name: view.value.core.network_name,
    network_secret: view.value.core.network_secret ?? '',
    no_password: view.value.core.network_secret === '',
    secure_mode: view.value.core.secure_mode,
    disable_encryption: view.value.core.disable_encryption,
    encryption_algorithm: view.value.core.encryption_algorithm || DEFAULT_ALGORITHM,
    virtual_subnet: (view.value.core.virtual_ipv4 && toNetworkCidr(view.value.core.virtual_ipv4)) || '',
};
// Reload the form only when the console's view changes, not on every poll.
watch(() => JSON.stringify(view.value?.core), () => { form.value = formOf(); }, { immediate: true });

const change = computed<MeshCoreChange>(() => {
    const before = formOf();
    const after = form.value;
    if (!before || !after) return {};
    const result: MeshCoreChange = {};
    if (after.display_name.trim() !== before.display_name) result.display_name = after.display_name.trim();
    if (after.network_name.trim() !== before.network_name) result.network_name = after.network_name.trim();
    const secret = after.no_password ? '' : after.network_secret.trim();
    if (secret !== before.network_secret) result.network_secret = secret;
    if (after.secure_mode !== before.secure_mode) result.secure_mode = after.secure_mode;
    if (after.disable_encryption !== before.disable_encryption) result.disable_encryption = after.disable_encryption;
    if (after.encryption_algorithm !== before.encryption_algorithm) {
        result.encryption_algorithm = after.encryption_algorithm === DEFAULT_ALGORITHM ? '' : after.encryption_algorithm;
    }
    if (subnet.value && subnet.value !== before.virtual_subnet) result.virtual_subnet = subnet.value;
    return result;
});

// The subnet moves static members and keeps their host parts; running DHCP
// members restart into it. An empty field leaves it as it is.
const parsedSubnet = computed(() => parseVirtualSubnet(form.value?.virtual_subnet ?? ''));
const subnet = computed(() => parsedSubnet.value ?? undefined);
const subnetInvalid = computed(() => parsedSubnet.value === null);
const permanentMembers = ref<MeshMember[]>([]);
const loadMembers = async () => {
    try {
        permanentMembers.value = (await api.list_mesh_members())
            .filter(member => member.network_name === networkName.value && !member.credential);
    } catch (error) {
        console.debug('Failed to read members', error);
    }
};
onMounted(loadMembers);
const subnetPreview = computed(() => {
    const target = change.value.virtual_subnet;
    if (!target) return [];
    const rows = permanentMembers.value.filter(member => member.virtual_ipv4).map(member => ({
        key: `${member.device_id}/${member.instance_id}`,
        from: member.virtual_ipv4!,
        to: moveIntoSubnet(member.virtual_ipv4!, target),
    }));
    const addresses = rows.map(row => row.to?.split('/')[0]);
    return rows.map(row => ({
        ...row,
        problem: !row.to ? 'no_host' : addresses.filter(address => address === row.to!.split('/')[0]).length > 1 ? 'conflict' : undefined,
    }));
});
const dhcpCount = computed(() => permanentMembers.value.filter(member => !member.virtual_ipv4).length);

const invalid = computed(() => !form.value?.network_name.trim()
    || (!form.value.no_password && !form.value.network_secret.trim())
    || subnetInvalid.value || subnetPreview.value.some(row => row.problem));

const saving = ref(false);
const results = ref<MeshSyncResult[]>([]);
const memberName = (key: string) => {
    const member = network.value?.members.find(member => member.key === key);
    return member ? member.device.alias || member.device.hostname || member.device.machine_id : key.split('/')[0];
};
const save = async () => {
    if (!Object.keys(change.value).length || invalid.value) return;
    saving.value = true;
    try {
        const renamed = change.value.network_name;
        results.value = await api.apply_mesh_core(networkName.value, change.value);
        const failed = failedResults(results.value).length;
        toast.add({
            severity: failed ? 'warn' : 'success',
            summary: t('web.mesh_network.section_settings'),
            detail: t('web.mesh_network.batch_done', { ok: results.value.length - failed, failed }),
            life: 3000,
        });
        if (renamed && renamed !== networkName.value) {
            await router.replace({ name: 'meshSettings', params: { networkName: renamed } });
        }
        await Promise.all([reload(), loadMembers()]);
    } catch (error) {
        toast.add({ severity: 'error', summary: t('web.mesh_network.section_settings'), detail: failureMessage(error), life: 5000 });
    } finally {
        saving.value = false;
    }
};

const deleteNetwork = () => confirm.require({
    header: t('web.mesh_network.delete'),
    message: t('web.mesh_network.delete_confirm', { name: network.value?.label ?? networkName.value }),
    acceptLabel: t('web.mesh_network.delete'),
    rejectLabel: t('web.common.cancel'),
    acceptProps: { severity: 'danger' },
    accept: async () => {
        try {
            const paused = await api.delete_mesh_network(networkName.value);
            const failed = failedResults(paused).length;
            toast.add({
                severity: failed ? 'warn' : 'success',
                summary: t('web.mesh_network.delete'),
                detail: t('web.mesh_network.delete_done', { count: paused.length - failed, failed }),
                life: 4000,
            });
            await router.push({ name: 'networkList' });
            await reload();
        } catch (error) {
            toast.add({ severity: 'error', summary: t('web.mesh_network.delete'), detail: failureMessage(error), life: 5000 });
        }
    },
});
</script>

<template>
    <ConfirmDialog />
    <section class="console-panel">
        <header class="section-heading"><h2>{{ t('web.mesh_network.section_settings') }}</h2></header>
        <p class="section-hint">{{ t('web.mesh_network.settings_hint') }}</p>
        <div v-if="!form" class="loading-rows"><Skeleton v-for="i in 4" :key="i" height="2rem" /></div>
        <div v-else class="settings-form">
            <div class="field">
                <label for="settings-display-name">{{ t('web.mesh_network.display_name') }}</label>
                <InputText id="settings-display-name" v-model="form.display_name" :placeholder="form.network_name" />
                <small class="muted">{{ t('web.mesh_network.display_name_hint') }}</small>
            </div>
            <div class="field">
                <label for="settings-network-name">{{ t('web.mesh_network.network_name') }}</label>
                <InputText id="settings-network-name" v-model="form.network_name" class="font-mono" :invalid="!form.network_name.trim()" />
                <small class="muted">{{ t('web.mesh_network.network_name_hint') }}</small>
            </div>
            <div class="field">
                <label for="settings-network-secret">{{ t('web.mesh_network.network_secret') }}</label>
                <div class="flex flex-col sm:flex-row gap-2">
                    <Password input-id="settings-network-secret" v-model="form.network_secret" toggle-mask :feedback="false" fluid
                        class="flex-1 min-w-0" :disabled="form.no_password" :invalid="!form.no_password && !form.network_secret.trim()"
                        :input-props="{ autocomplete: 'new-password' }" />
                    <Button :label="t('web.network_list.generate_secret')" icon="pi pi-refresh" severity="secondary" outlined
                        :disabled="form.no_password" @click="form.network_secret = uuidv4()" />
                </div>
                <div class="flex items-center gap-2">
                    <Checkbox input-id="settings-no-password" v-model="form.no_password" binary />
                    <label for="settings-no-password" class="text-sm">{{ t('web.network_list.no_password') }}</label>
                </div>
            </div>
            <div class="field-row">
                <InputSwitch input-id="settings-secure-mode" v-model="form.secure_mode" />
                <label for="settings-secure-mode">{{ t('web.mesh_network.secure_mode') }}</label>
                <span class="pi pi-question-circle text-sm" v-tooltip.top="t('web.network_list.secure_mode_hint')"></span>
            </div>
            <div class="field-row">
                <InputSwitch input-id="settings-disable-encryption" v-model="form.disable_encryption" />
                <label for="settings-disable-encryption">{{ t('disable_encryption') }}</label>
                <span class="pi pi-question-circle text-sm" v-tooltip.top="t('disable_encryption_help')"></span>
            </div>
            <div class="field">
                <label for="settings-algorithm">{{ t('web.mesh_network.encryption_algorithm') }}</label>
                <Select input-id="settings-algorithm" v-model="form.encryption_algorithm" :options="algorithms" option-label="label"
                    option-value="value" :disabled="form.disable_encryption" class="w-full sm:w-80" />
            </div>
            <div class="field">
                <label for="settings-virtual-subnet">{{ t('web.mesh_network.virtual_subnet') }}</label>
                <InputText id="settings-virtual-subnet" v-model="form.virtual_subnet" class="font-mono w-full sm:w-80"
                    :placeholder="t('web.mesh_network.virtual_subnet_default')" :invalid="subnetInvalid" />
                <small v-if="subnetInvalid" class="text-red-500">{{ t('web.mesh_network.virtual_subnet_invalid') }}</small>
                <small v-else class="muted">{{ t('web.mesh_network.virtual_subnet_hint') }}</small>
                <div v-if="change.virtual_subnet && (subnetPreview.length || dhcpCount)" class="subnet-preview">
                    <h3>{{ t('web.mesh_network.virtual_subnet_preview') }}</h3>
                    <div v-for="row in subnetPreview" :key="row.key" class="subnet-row">
                        <span class="preview-name">{{ memberName(row.key) }}</span>
                        <span class="mono-value">{{ row.from }} → {{ row.to ?? '—' }}</span>
                        <Tag v-if="row.problem" :value="t(`web.mesh_network.virtual_subnet_${row.problem}`)" severity="danger" />
                    </div>
                    <span v-if="dhcpCount" class="preview-secondary">{{ t('web.mesh_network.virtual_subnet_dhcp', { count: dhcpCount }) }}</span>
                </div>
            </div>
            <Message v-if="Object.keys(change).length" severity="info" :closable="false">{{ t('web.mesh_network.core_hint') }}</Message>
            <div class="flex gap-2">
                <Button :label="t('web.mesh_network.apply_settings')" icon="pi pi-check" :loading="saving"
                    :disabled="!Object.keys(change).length || invalid" @click="save" />
                <Button :label="t('web.mesh_network.discard')" icon="pi pi-undo" text severity="secondary"
                    :disabled="!Object.keys(change).length" @click="form = formOf()" />
            </div>
            <div v-if="results.length" class="sync-results">
                <h3>{{ t('web.mesh_network.batch_result') }}</h3>
                <div v-for="result in results" :key="`${result.device_id}/${result.instance_id}`" class="sync-result">
                    <span class="preview-name">{{ memberName(`${result.device_id}/${result.instance_id}`) }}</span>
                    <Tag :value="t(`web.mesh_network.result_${result.status}`)" :severity="resultSeverity(result.status)" />
                    <span v-if="result.message" class="preview-secondary">{{ result.message }}</span>
                </div>
            </div>
        </div>
    </section>
    <section class="console-panel danger-panel">
        <header class="section-heading"><h2>{{ t('web.network_detail.danger_zone') }}</h2></header>
        <div class="settings-form">
            <p class="text-sm muted m-0">{{ t('web.mesh_network.delete_hint') }}</p>
            <div><Button :label="t('web.mesh_network.delete')" icon="pi pi-trash" severity="danger" outlined :disabled="!view" @click="deleteNetwork" /></div>
        </div>
    </section>
</template>

<style scoped>
.section-hint { margin: 0; padding: 0 20px 4px; font-size: 13px; color: var(--p-text-muted-color); }
.settings-form { display: flex; flex-direction: column; gap: 16px; padding: 16px 20px 20px; max-width: 40rem; }
.field { display: flex; flex-direction: column; gap: 6px; }
.field-row { display: flex; align-items: center; gap: 10px; }
.danger-panel { border-color: var(--p-red-200); }
.sync-results { display: flex; flex-direction: column; gap: 6px; padding: 12px; border: 1px solid var(--console-border); border-radius: 6px; }
.sync-results h3 { margin: 0 0 4px; font-size: 14px; }
.sync-result { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; font-size: 13px; }
.subnet-preview { display: flex; flex-direction: column; gap: 6px; margin-top: 4px; padding: 12px; border: 1px solid var(--console-border); border-radius: 6px; }
.subnet-preview h3 { margin: 0 0 4px; font-size: 14px; }
.subnet-row { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; font-size: 13px; }
</style>
