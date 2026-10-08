import { type Api, NetworkTypes, LocalConfigs, Capabilities, Utils } from "easytier-frontend-lib";
import * as backend from "~/composables/backend";

export class GUIRemoteClient implements Api.RemoteClient {
    private capabilities?: string[];
    private supportsRevision = false;
    private readonly persistedConfigs = new Map<string, NetworkTypes.NetworkConfig>();
    private knownInstanceIds = new Set<string>();
    private readonly boundScope: string;
    constructor(private readonly scopeSource: () => string = () => 'embedded') { this.boundScope = this.scope; }
    get scope(): string { return this.scopeSource(); }
    private ensureScope(): void {
        if (this.boundScope !== this.scope) throw new Error('Management connection changed');
    }

    private async targetCapabilities(): Promise<string[]> {
        const scope = this.scope;
        this.ensureScope();
        if (!this.capabilities) await this.list_network_instance_ids();
        if (scope !== this.scope) throw new Error('Management connection changed');
        return this.capabilities ?? [];
    }

    private async legacyWriteCapabilities(config: NetworkTypes.NetworkConfig): Promise<string[]> {
        const scope = this.scope;
        const capabilities = await this.targetCapabilities();
        if (this.knownInstanceIds.has(config.instance_id) && !this.persistedConfigs.has(config.instance_id)) {
            await this.get_network_config(config.instance_id);
        }
        if (scope !== this.scope) throw new Error('Management connection changed');
        Capabilities.assertLegacyConfigPreservable(this.persistedConfigs.get(config.instance_id), capabilities);
        return capabilities;
    }

    async validate_config(config: NetworkTypes.NetworkConfig): Promise<Api.ValidateConfigResponse> {
        return backend.validateConfig(config, await this.targetCapabilities());
    }
    async run_network(config: NetworkTypes.NetworkConfig, save: boolean): Promise<undefined> {
        await backend.runNetworkInstance(config, save, await this.legacyWriteCapabilities(config));
    }
    async get_network_info(inst_id: string): Promise<NetworkTypes.NetworkInstanceRunningInfo | undefined> {
        return backend.collectNetworkInfo(inst_id).then(infos => infos.info?.map?.[inst_id]);
    }
    async get_vpn_portal_info(inst_id: string): Promise<NetworkTypes.VpnPortalInfo | undefined> {
        return backend.getVpnPortalInfo(inst_id);
    }
    async add_vpn_portal_client(inst_id: string, client: { name: string, virtual_ip: string, groups: string[] }): Promise<undefined> {
        this.ensureScope();
        await backend.addVpnPortalClient(inst_id, client);
    }
    async remove_vpn_portal_client(inst_id: string, name: string): Promise<undefined> {
        this.ensureScope();
        await backend.removeVpnPortalClient(inst_id, name);
    }
    async clear_vpn_portal_clients(inst_id: string): Promise<undefined> {
        this.ensureScope();
        await backend.clearVpnPortalClients(inst_id);
    }
    async list_network_instance_ids(): Promise<Api.ListNetworkInstanceIdResponse> {
        const scope = this.scope;
        const response = await backend.listNetworkInstanceIds();
        if (scope !== this.scope) throw new Error('Management connection changed');
        this.capabilities = response.runtime_capabilities ?? [];
        this.knownInstanceIds = new Set([...response.running_inst_ids, ...response.disabled_inst_ids].map(id => typeof id === 'string' ? id : Utils.UuidToStr(id)));
        this.supportsRevision = response.support_local_config_revision === true
            || this.capabilities.includes(Capabilities.LOCAL_CONFIG_REVISION_CAPABILITY);
        return response;
    }
    async delete_network(inst_id: string, expectedRevision?: string): Promise<undefined> {
        this.ensureScope();
        await backend.deleteNetworkInstance(inst_id, expectedRevision);
    }
    async update_network_instance_state(inst_id: string, disabled: boolean, expectedRevision?: string): Promise<undefined> {
        this.ensureScope();
        await backend.updateNetworkConfigState(inst_id, disabled, expectedRevision);
    }
    async save_config(config: NetworkTypes.NetworkConfig): Promise<undefined> {
        await backend.saveNetworkConfig(config, await this.legacyWriteCapabilities(config));
    }
    async get_network_config(inst_id: string): Promise<NetworkTypes.NetworkConfig> {
        const scope = this.scope;
        const config = await backend.getConfig(inst_id, false);
        if (scope !== this.scope) throw new Error('Management connection changed');
        this.persistedConfigs.set(inst_id, structuredClone(config));
        return NetworkTypes.normalizeNetworkConfig(config);
    }
    async generate_config(config: NetworkTypes.NetworkConfig): Promise<Api.GenerateConfigResponse> {
        try {
            return { toml_config: await backend.parseNetworkConfig(config, await this.targetCapabilities()) };
        } catch (e) {
            return { error: e + "" };
        }
    }
    async parse_config(toml_config: string): Promise<Api.ParseConfigResponse> {
        try {
            return { config: await backend.generateNetworkConfig(toml_config) }
        } catch (e) {
            return { error: e + "" };
        }
    }
    async get_network_metas(instance_ids: string[]): Promise<Api.GetNetworkMetasResponse> {
        return await backend.getNetworkMetas(instance_ids);
    }

    async observe_local_configs(): Promise<LocalConfigs.LocalConfigSnapshot> {
        const scope = this.scope;
        const capabilities = await this.targetCapabilities();
        if (!this.supportsRevision) throw new Error('Local configuration revisions are unsupported');
        const response = await backend.observeLocalConfigs();
        if (scope !== this.scope) throw new Error('Management connection changed');
        return { ...response, online: true, stale: false, capabilities,
            support_local_config_revision: this.supportsRevision };
    }

    async patch_local_config(request: LocalConfigs.LocalConfigPatchRequest): Promise<LocalConfigs.LocalConfigPatchResult> {
        const capabilities = await this.targetCapabilities();
        if (!this.supportsRevision) throw new Error('Local configuration revisions are unsupported');
        Capabilities.assertPatchCapabilities(request.config, request.field_mask, capabilities);
        return backend.patchLocalConfig(request);
    }

    localConfigClient(): LocalConfigs.LocalConfigClient {
        const remote = this;
        return {
            get scope() { return remote.scope; },
            async list() {
                await remote.list_network_instance_ids();
                if (!remote.supportsRevision) return [];
                return [{ ...await remote.observe_local_configs(), machine_id: 'current' }];
            },
            observe: () => remote.observe_local_configs(),
            patch: (_machineId, request) => remote.patch_local_config(request),
            setEnabled: async (_machineId, instanceId, revision, enabled) => {
                remote.ensureScope();
                await backend.setLocalConfigEnabled(instanceId, revision, enabled);
                return { status: 0 };
            },
            remove: async (_machineId, instanceId, revision) => {
                remote.ensureScope();
                await backend.removeLocalConfig(instanceId, revision);
                return { status: 0 };
            },
        };
    }

}
