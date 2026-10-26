import { type Api, NetworkTypes, LocalConfigs, Capabilities } from "easytier-frontend-lib";
import * as backend from "~/composables/backend";

/**
 * The GUI manages only EasyTier Custom Cores. Their config directory is the
 * single store of network configurations, so every read observes it and
 * every write is a revision-checked patch of one file.
 */
export class GUIRemoteClient implements Api.RemoteClient {
    private capabilities?: string[];
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

    async validate_config(config: NetworkTypes.NetworkConfig): Promise<Api.ValidateConfigResponse> {
        return backend.validateConfig(config, await this.targetCapabilities());
    }
    async run_network(): Promise<undefined> {
        throw new Error('Networks are saved through the Core config directory');
    }
    async save_config(): Promise<undefined> {
        throw new Error('Networks are saved through the Core config directory');
    }
    async get_network_info(inst_id: string): Promise<NetworkTypes.NetworkInstanceRunningInfo | undefined> {
        return backend.collectNetworkInfo(inst_id).then(infos => infos.info?.map?.[inst_id]);
    }
    async get_vpn_portal_info(inst_id: string): Promise<NetworkTypes.VpnPortalInfo | undefined> {
        return backend.getVpnPortalInfo(inst_id);
    }
    async get_vpn_status(inst_id: string): Promise<NetworkTypes.GetVpnStatusResponse | undefined> {
        return backend.getVpnStatus(inst_id);
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
        return { ...response, support_local_config_revision: true };
    }
    async delete_network(inst_id: string, expectedRevision?: string): Promise<undefined> {
        this.ensureScope();
        if (!expectedRevision) throw new Error('A configuration revision is required');
        await backend.deleteNetworkInstance(inst_id, expectedRevision);
    }
    async update_network_instance_state(inst_id: string, disabled: boolean, expectedRevision?: string): Promise<undefined> {
        this.ensureScope();
        if (!expectedRevision) throw new Error('A configuration revision is required');
        await backend.updateNetworkConfigState(inst_id, disabled, expectedRevision);
    }
    async get_network_config(inst_id: string): Promise<NetworkTypes.NetworkConfig> {
        const entry = (await this.observe_local_configs()).entries
            .find(entry => LocalConfigs.localConfigInstanceId(entry) === inst_id);
        if (!entry?.config) throw new Error(`Configuration ${inst_id} is unavailable`);
        return NetworkTypes.normalizeNetworkConfig(entry.config);
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
        const response = await backend.observeLocalConfigs();
        if (scope !== this.scope) throw new Error('Management connection changed');
        return { ...response, online: true, stale: false, capabilities, support_local_config_revision: true };
    }

    async patch_local_config(request: LocalConfigs.LocalConfigPatchRequest): Promise<LocalConfigs.LocalConfigPatchResult> {
        const capabilities = await this.targetCapabilities();
        Capabilities.assertPatchCapabilities(request.config ?? {}, request.field_mask, capabilities);
        return backend.patchLocalConfig(request);
    }
}
