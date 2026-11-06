<script setup lang="ts">
import { computed, onMounted, ref, toRaw, watch } from 'vue';
import { v4 as uuidv4 } from 'uuid';
import { Button, Checkbox, InputText, Message, MultiSelect, Select, InputSwitch, SelectButton, Skeleton, Tag, useToast } from 'primevue';
import { useI18n } from 'vue-i18n';
import type {
    MeshAclGroup, MeshAclPeer, MeshAclProtocol, MeshAclRule, MeshAclTarget, MeshNetworkAcl, MeshNetworkAclView, MeshSyncResult,
} from '../../modules/api';
import { failureMessage, memberDetails, networkClients, type NetworkClient } from '../../modules/networkDiscovery';
import { deviceName, failedResults, resultSeverity, useMeshNetwork } from '../../modules/meshNetworkContext';

// The network's access control: every permanent member holds the same rules,
// written like an advanced setting, and the console rebuilds them from the
// members. Rules name groups or single members, never addresses.
const { t } = useI18n();
const toast = useToast();
const { api, networkName, network, reload } = useMeshNetwork();

const view = ref<MeshNetworkAclView>();
const draft = ref<MeshNetworkAcl | null>(null);
const dirty = ref(false);
const load = async () => {
    try {
        view.value = await api.get_mesh_network_acl(networkName.value);
        // The view is reactive, and a proxy cannot be cloned.
        draft.value = view.value.acl ? structuredClone(toRaw(view.value.acl)) : null;
        dirty.value = false;
    } catch (error) {
        view.value = { acl: null, members: [], temporary: [] };
        toast.add({ severity: 'error', summary: t('web.mesh_network.section_acl'), detail: failureMessage(error), life: 5000 });
    }
};
onMounted(load);

// Clients that joined without this console hold no group secret: a rule can
// only name one as a source, by its address.
const clients = ref<NetworkClient[]>([]);
const runningMembers = computed(() => (network.value?.members ?? [])
    .filter(member => member.online && member.running).map(member => member.key).sort().join(','));
watch(runningMembers, async () => {
    try {
        const members = network.value?.members ?? [];
        clients.value = networkClients(await memberDetails(api, members), members);
    } catch (error) {
        console.debug('Failed to read network clients', error);
    }
}, { immediate: true });
const clientCidr = (client: NetworkClient) => `${client.address?.split('/')[0]}/32`;
const touch = () => { dirty.value = true; };

const permanentMembers = computed(() => network.value?.members.filter(member => !member.credential) ?? []);
const temporaryLabel = (group: string) => {
    const member = view.value?.temporary.find(member => member.group === group);
    return member?.hostname || member?.device_id.slice(0, 8) || group;
};

// Picker values: `all`, `group:<name>` (credential groups too) and `device:<instance id>`.
const groupMemberOptions = computed(() => [
    ...permanentMembers.value.map(member => ({ label: deviceName(member), value: member.instanceId })),
    ...(view.value?.temporary ?? []).map(member => ({ label: `${temporaryLabel(member.group)} · ${t('web.mesh_network.temporary')}`, value: member.group })),
]);
const peerOptions = computed(() => [
    { label: t('web.mesh_network.acl_everyone'), value: 'all' },
    ...(draft.value?.groups ?? []).map(group => ({ label: `${t('web.mesh_network.acl_group')} · ${group.name}`, value: `group:${group.name}` })),
    ...permanentMembers.value.map(member => ({ label: deviceName(member), value: `device:${member.instanceId}` })),
    ...(view.value?.temporary ?? []).map(member => ({ label: `${temporaryLabel(member.group)} · ${t('web.mesh_network.temporary')}`, value: `group:${member.group}` })),
]);
// Sources may also be clients, including addresses no client holds now.
const sourceOptions = computed(() => {
    const options = [
        ...peerOptions.value,
        ...clients.value.filter(client => client.address).map(client => ({
            label: `${client.hostname} · ${t('web.mesh_network.acl_client')} · ${client.address!.split('/')[0]}`,
            value: `address:${clientCidr(client)}`,
        })),
    ];
    const known = new Set(options.map(option => option.value));
    for (const rule of draft.value?.rules ?? []) {
        for (const source of rule.sources) {
            if (source.type === 'address' && !known.has(`address:${source.cidr}`)) {
                known.add(`address:${source.cidr}`);
                options.push({ label: `${t('web.mesh_network.acl_client')} · ${source.cidr}`, value: `address:${source.cidr}` });
            }
        }
    }
    return options;
});
const peerValue = (peer: MeshAclPeer) => peer.type === 'all' ? 'all' : peer.type === 'group' ? `group:${peer.name}`
    : peer.type === 'address' ? `address:${peer.cidr}` : `device:${peer.instance_id}`;
const peerOf = (value: string): MeshAclPeer => {
    if (value === 'all') return { type: 'all' };
    const [kind, ...rest] = value.split(':');
    const name = rest.join(':');
    return kind === 'device' ? { type: 'device', instance_id: name }
        : kind === 'address' ? { type: 'address', cidr: name } : { type: 'group', name };
};
const peerPicks = (peers: (MeshAclPeer | MeshAclTarget)[]) => peers.filter((peer): peer is MeshAclPeer => peer.type !== 'subnet').map(peerValue);
const setSources = (rule: MeshAclRule, values: string[]) => { rule.sources = values.map(peerOf); touch(); };
const setDestinations = (rule: MeshAclRule, values: string[]) => {
    rule.destinations = [
        ...values.map(peerOf).filter((peer): peer is MeshAclTarget => peer.type !== 'address'),
        ...rule.destinations.filter(target => target.type === 'subnet'),
    ];
    touch();
};
const subnetText = (rule: MeshAclRule) => rule.destinations.flatMap(target => target.type === 'subnet' ? target.cidrs : []).join(', ');
const setSubnets = (rule: MeshAclRule, text: string) => {
    const cidrs = text.split(/[\s,]+/).map(cidr => cidr.trim()).filter(Boolean);
    rule.destinations = [...rule.destinations.filter(target => target.type !== 'subnet'), ...(cidrs.length ? [{ type: 'subnet' as const, cidrs }] : [])];
    touch();
};

const actionOptions = computed(() => [
    { label: t('web.mesh_network.acl_allow'), value: 'allow' },
    { label: t('web.mesh_network.acl_deny'), value: 'deny' },
]);
const protocolOptions: { label: string; value: MeshAclProtocol }[] = [
    { label: 'TCP', value: 'tcp' }, { label: 'UDP', value: 'udp' }, { label: 'ICMP', value: 'icmp' },
    { label: 'ICMPv6', value: 'icmpv6' }, { label: 'Any', value: 'any' },
];
const portsText = (ports: string[]) => ports.join(', ');
const setPorts = (target: { ports: string[] }, text: string) => {
    target.ports = text.split(/[\s,]+/).map(port => port.trim()).filter(Boolean);
    touch();
};

const enable = () => {
    draft.value = { default_action: 'allow', groups: [], rules: [] };
    touch();
};
const disable = () => { draft.value = null; touch(); };

const newGroup = ref('');
const groupNameValid = (name: string) => /^[A-Za-z0-9_.-]{1,64}$/.test(name);
const addGroup = () => {
    const name = newGroup.value.trim();
    if (!draft.value || !groupNameValid(name) || draft.value.groups.some(group => group.name === name)) return;
    draft.value.groups.push({ name, members: [] });
    newGroup.value = '';
    touch();
};
const removeGroup = (group: MeshAclGroup) => {
    if (!draft.value) return;
    draft.value.groups = draft.value.groups.filter(entry => entry !== group);
    for (const rule of draft.value.rules) {
        rule.sources = rule.sources.filter(peer => !(peer.type === 'group' && peer.name === group.name));
        rule.destinations = rule.destinations.filter(target => !(target.type === 'group' && target.name === group.name));
    }
    touch();
};

const addRule = () => {
    draft.value?.rules.push({
        id: uuidv4(), name: t('web.mesh_network.acl_new_rule'), enabled: true, action: 'allow',
        sources: [{ type: 'all' }], destinations: [{ type: 'all' }],
        protocols: [{ protocol: 'any', ports: [], stateful: false }],
    });
    touch();
};
const moveRule = (index: number, offset: number) => {
    const rules = draft.value?.rules;
    if (!rules || index + offset < 0 || index + offset >= rules.length) return;
    const [rule] = rules.splice(index, 1);
    rules.splice(index + offset, 0, rule);
    touch();
};
const removeRule = (index: number) => { draft.value?.rules.splice(index, 1); touch(); };

const outOfSync = computed(() => view.value?.members.filter(member => !member.in_sync) ?? []);
const ownDefault = computed(() => draft.value?.default_action === 'deny'
    ? view.value?.members.filter(member => member.own_default) ?? [] : []);
const statusName = (status: { device_id: string; instance_id: string }) => {
    const member = network.value?.members.find(member => member.key === `${status.device_id}/${status.instance_id}`);
    return member ? deviceName(member) : status.device_id;
};

const saving = ref(false);
const results = ref<MeshSyncResult[]>([]);
const save = async () => {
    saving.value = true;
    try {
        results.value = await api.set_mesh_network_acl(networkName.value, draft.value);
        const failed = failedResults(results.value).length;
        toast.add({
            severity: failed ? 'warn' : 'success',
            summary: t('web.mesh_network.section_acl'),
            detail: t('web.mesh_network.batch_done', { ok: results.value.length - failed, failed }),
            life: 3000,
        });
        await Promise.all([load(), reload()]);
    } catch (error) {
        toast.add({ severity: 'error', summary: t('web.mesh_network.section_acl'), detail: failureMessage(error), life: 5000 });
    } finally {
        saving.value = false;
    }
};
</script>

<template>
    <section class="console-panel">
        <header class="section-heading acl-heading">
            <h2>{{ t('web.mesh_network.section_acl') }}</h2>
            <div class="flex items-center flex-wrap gap-2 ml-auto">
                <Button v-if="draft" :label="t('web.mesh_network.acl_disable')" icon="pi pi-times" text severity="danger" size="small"
                    :disabled="saving" @click="disable" />
                <Button :label="t('web.mesh_network.discard')" icon="pi pi-undo" text severity="secondary" size="small"
                    :disabled="!dirty || saving" @click="load" />
                <Button :label="t('web.mesh_network.apply_settings')" icon="pi pi-check" size="small"
                    :loading="saving" :disabled="!dirty" @click="save" />
            </div>
        </header>
        <p class="section-hint">{{ t('web.mesh_network.acl_hint') }}</p>
        <div v-if="results.length" class="sync-results">
            <h3>{{ t('web.mesh_network.batch_result') }}</h3>
            <div v-for="result in results" :key="`${result.device_id}/${result.instance_id}`" class="sync-result">
                <span class="preview-name">{{ statusName(result) }}</span>
                <Tag :value="t(`web.mesh_network.result_${result.status}`)" :severity="resultSeverity(result.status)" />
                <span v-if="result.message" class="preview-secondary">{{ result.message }}</span>
            </div>
        </div>
        <div class="acl-body">
            <div v-if="view === undefined" class="loading-rows"><Skeleton v-for="i in 4" :key="i" height="2rem" /></div>
            <template v-else>
                <Message v-if="outOfSync.length" severity="warn" :closable="false">
                    {{ t('web.mesh_network.acl_out_of_sync', { members: outOfSync.map(statusName).join(', ') }) }}
                </Message>
                <Message v-if="ownDefault.length" severity="info" :closable="false">
                    {{ t('web.mesh_network.acl_own_default', { members: ownDefault.map(statusName).join(', ') }) }}
                </Message>
                <div v-if="!draft" class="acl-empty">
                    <p>{{ t('web.mesh_network.acl_off') }}</p>
                    <Button :label="t('web.mesh_network.acl_enable')" icon="pi pi-shield" size="small" @click="enable" />
                </div>
                <template v-else>
                    <div class="acl-row">
                        <span class="acl-label">{{ t('web.mesh_network.acl_default_action') }}</span>
                        <SelectButton v-model="draft.default_action" :options="actionOptions" option-label="label" option-value="value"
                            :allow-empty="false" @change="touch" />
                    </div>

                    <h3>{{ t('web.mesh_network.acl_groups') }}</h3>
                    <p class="acl-note">{{ t('web.mesh_network.acl_groups_hint') }}</p>
                    <div v-for="group in draft.groups" :key="group.name" class="acl-group">
                        <span class="mono-value acl-group-name">{{ group.name }}</span>
                        <MultiSelect v-model="group.members" :options="groupMemberOptions" option-label="label" option-value="value"
                            display="chip" filter class="acl-grow" :placeholder="t('web.mesh_network.acl_group_members')" @change="touch" />
                        <Button icon="pi pi-trash" text severity="danger" size="small" :aria-label="t('web.mesh_network.acl_remove')" @click="removeGroup(group)" />
                    </div>
                    <div class="acl-group">
                        <InputText v-model="newGroup" size="small" :placeholder="t('web.mesh_network.acl_group_name')"
                            :invalid="!!newGroup && !groupNameValid(newGroup.trim())" @keyup.enter="addGroup" />
                        <Button :label="t('web.mesh_network.acl_add_group')" icon="pi pi-plus" size="small" text
                            :disabled="!groupNameValid(newGroup.trim())" @click="addGroup" />
                    </div>

                    <h3>{{ t('web.mesh_network.acl_rules') }}</h3>
                    <p class="acl-note">{{ t('web.mesh_network.acl_rules_hint') }}</p>
                    <div v-for="(rule, index) in draft.rules" :key="rule.id" class="acl-rule" :class="{ disabled: !rule.enabled }">
                        <div class="acl-rule-head">
                            <InputSwitch v-model="rule.enabled" @change="touch" />
                            <InputText v-model="rule.name" size="small" class="acl-grow" @input="touch" />
                            <SelectButton v-model="rule.action" :options="actionOptions" option-label="label" option-value="value"
                                :allow-empty="false" @change="touch" />
                            <Button icon="pi pi-arrow-up" text size="small" :disabled="index === 0" :aria-label="t('web.mesh_network.acl_move_up')" @click="moveRule(index, -1)" />
                            <Button icon="pi pi-arrow-down" text size="small" :disabled="index === draft.rules.length - 1" :aria-label="t('web.mesh_network.acl_move_down')" @click="moveRule(index, 1)" />
                            <Button icon="pi pi-trash" text severity="danger" size="small" :aria-label="t('web.mesh_network.acl_remove')" @click="removeRule(index)" />
                        </div>
                        <div class="acl-row">
                            <span class="acl-label">{{ t('web.mesh_network.acl_sources') }}</span>
                            <MultiSelect :model-value="peerPicks(rule.sources)" :options="sourceOptions" option-label="label" option-value="value"
                                display="chip" filter class="acl-grow" @update:model-value="setSources(rule, $event)" />
                        </div>
                        <div class="acl-row">
                            <span class="acl-label">{{ t('web.mesh_network.acl_destinations') }}</span>
                            <MultiSelect :model-value="peerPicks(rule.destinations)" :options="peerOptions" option-label="label" option-value="value"
                                display="chip" filter class="acl-grow" @update:model-value="setDestinations(rule, $event)" />
                        </div>
                        <div class="acl-row">
                            <span class="acl-label">{{ t('web.mesh_network.acl_subnets') }}</span>
                            <InputText :model-value="subnetText(rule)" size="small" class="acl-grow" placeholder="192.168.1.0/24"
                                @change="setSubnets(rule, ($event.target as HTMLInputElement).value)" />
                        </div>
                        <div v-for="(protocol, position) in rule.protocols" :key="position" class="acl-row">
                            <span class="acl-label">{{ position === 0 ? t('web.mesh_network.acl_protocols') : '' }}</span>
                            <Select v-model="protocol.protocol" :options="protocolOptions" option-label="label" option-value="value" size="small"
                                @change="() => { if (protocol.protocol !== 'tcp') protocol.stateful = false; if (protocol.protocol.startsWith('icmp')) protocol.ports = []; touch(); }" />
                            <InputText :model-value="portsText(protocol.ports)" size="small" class="acl-grow" placeholder="22, 8000-8100"
                                :disabled="protocol.protocol.startsWith('icmp')" @change="setPorts(protocol, ($event.target as HTMLInputElement).value)" />
                            <label v-if="protocol.protocol === 'tcp'" class="acl-check">
                                <Checkbox v-model="protocol.stateful" binary @change="touch" />{{ t('web.mesh_network.acl_stateful') }}
                            </label>
                            <Button icon="pi pi-minus" text size="small" :disabled="rule.protocols.length === 1" :aria-label="t('web.mesh_network.acl_remove')"
                                @click="rule.protocols.splice(position, 1); touch()" />
                        </div>
                        <Button :label="t('web.mesh_network.acl_add_protocol')" icon="pi pi-plus" text size="small" class="self-start"
                            @click="rule.protocols.push({ protocol: 'tcp', ports: [], stateful: false }); touch()" />
                    </div>
                    <Button :label="t('web.mesh_network.acl_add_rule')" icon="pi pi-plus" size="small" outlined class="self-start" @click="addRule" />
                </template>
            </template>
        </div>
    </section>
</template>

<style scoped>
.acl-heading { flex-wrap: wrap; }
.section-hint { margin: 0; padding: 0 20px 4px; font-size: 13px; color: var(--p-text-muted-color); }
.acl-body { padding: 12px 20px 20px; display: flex; flex-direction: column; gap: 10px; }
.acl-body h3 { margin: 10px 0 0; font-size: 14px; }
.acl-note { margin: 0; font-size: 12px; color: var(--p-text-muted-color); }
.acl-empty { display: flex; flex-direction: column; align-items: flex-start; gap: 8px; font-size: 13px; }
.acl-row, .acl-group, .acl-rule-head { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; }
.acl-label { width: 7rem; flex-shrink: 0; font-size: 13px; color: var(--p-text-muted-color); }
.acl-group-name { min-width: 8rem; }
.acl-grow { flex: 1 1 16rem; min-width: 0; }
.acl-check { display: flex; align-items: center; gap: 6px; font-size: 13px; }
.acl-rule { display: flex; flex-direction: column; gap: 8px; padding: 12px; border: 1px solid var(--console-border); border-radius: 6px; }
.acl-rule.disabled { opacity: .6; }
.sync-results { display: flex; flex-direction: column; gap: 6px; margin: 8px 20px 0; padding: 12px; border: 1px solid var(--console-border); border-radius: 6px; }
.sync-results h3 { margin: 0 0 4px; font-size: 14px; }
.sync-result { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; font-size: 13px; }
@media (max-width: 640px) {
    .acl-label { width: 100%; }
}
</style>
