package com.plugin.vpnservice

import android.app.Activity
import android.content.Intent
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.VpnService
import androidx.activity.result.ActivityResult
import app.tauri.annotation.Command
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import android.webkit.WebView

@InvokeArg
class PingArgs {
    var value: String? = null
}

@InvokeArg
class StartVpnArgs {
    var ipv4Addr: String? = null
    var ipv6Addr: String? = null
    var routes: Array<String> = emptyArray()
    var dnsServers: Array<String> = emptyArray()
    var disallowedApplications: Array<String> = emptyArray()
    var mtu: Int? = null
    var instanceId: String? = null
    var generation: String? = null
}

@InvokeArg
class CompleteVpnReconfigureArgs {
    var generation: String? = null
    var installed: Boolean = false
    var errorMsg: String? = null
}

@TauriPlugin
class VpnServicePlugin(private val activity: Activity) : Plugin(activity) {
    companion object {
        @Volatile
        private var tileActionCallback: (String) -> Boolean = { false }

        fun dispatchTileAction(action: String): Boolean = tileActionCallback(action)
    }

    private val implementation = Example()
    private val tileActionHandler: (String) -> Boolean = { action ->
        val data = JSObject()
        data.put("action", action)
        trigger("vpn_tile_action", data)
        true
    }

    override fun load(webView: WebView) {
        println("load vpn service plugin")
        TauriVpnService.triggerCallback = { event, data ->
            println("vpn: triggerCallback $event $data")
            trigger(event, data)
        }
        tileActionCallback = tileActionHandler
    }

    override fun onDestroy() {
        if (tileActionCallback === tileActionHandler) {
            tileActionCallback = { false }
        }
        super.onDestroy()
    }

    @Command
    fun ping(invoke: Invoke) {
        val args = invoke.parseArgs(PingArgs::class.java)

        val ret = JSObject()
        ret.put("value", implementation.pong(args.value ?: "default value :("))
        invoke.resolve(ret)
    }

    @Command
    fun prepareVpn(invoke: Invoke) {
        activity.runOnUiThread {
            println("prepare vpn in plugin")
            val it = VpnService.prepare(activity)
            if (it != null) {
                startActivityForResult(invoke, it, "onPrepareVpnResult")
                return@runOnUiThread
            }
            val ret = JSObject()
            ret.put("granted", true)
            invoke.resolve(ret)
        }
    }

    @ActivityCallback
    fun onPrepareVpnResult(invoke: Invoke, result: ActivityResult) {
        val ret = JSObject()
        ret.put("granted", result.resultCode == Activity.RESULT_OK)
        invoke.resolve(ret)
    }

    @Command
    fun startVpn(invoke: Invoke) {
        startVpnWithArgs(invoke, invoke.parseArgs(StartVpnArgs::class.java))
    }

    @Command
    fun reconfigureVpn(invoke: Invoke) {
        startVpnWithArgs(invoke, invoke.parseArgs(StartVpnArgs::class.java))
    }

    private fun startVpnWithArgs(invoke: Invoke, args: StartVpnArgs) {
        activity.runOnUiThread {
            println("configure vpn in plugin, args: $args")

            val it = VpnService.prepare(activity)
            val ret = JSObject()
            if (it != null) {
                ret.put("errorMsg", "need_prepare")
            } else {
                val intent = Intent(activity, TauriVpnService::class.java)
                intent.putExtra(TauriVpnService.IPV4_ADDR, args.ipv4Addr)
                intent.putExtra(TauriVpnService.IPV6_ADDR, args.ipv6Addr)
                intent.putExtra(TauriVpnService.ROUTES, args.routes)
                intent.putExtra(TauriVpnService.DNS_SERVERS, args.dnsServers)
                intent.putExtra(TauriVpnService.DISALLOWED_APPLICATIONS, args.disallowedApplications)
                intent.putExtra(TauriVpnService.MTU, args.mtu)
                intent.putExtra(TauriVpnService.INSTANCE_ID, args.instanceId)
                intent.putExtra(TauriVpnService.GENERATION, args.generation)

                if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.O) {
                    activity.startForegroundService(intent)
                } else {
                    activity.startService(intent)
                }
            }
            invoke.resolve(ret)
        }
    }

    @Command
    fun stopVpn(invoke: Invoke) {
        activity.runOnUiThread {
            println("stop vpn in plugin")
            activity.stopService(Intent(activity, TauriVpnService::class.java))
            println("stop vpn in plugin end")
            invoke.resolve(JSObject())
        }
    }

    @Command
    fun completeVpnReconfigure(invoke: Invoke) {
        val args = invoke.parseArgs(CompleteVpnReconfigureArgs::class.java)
        val accepted = TauriVpnService.self?.completeReconfigure(
            args.generation ?: "",
            args.installed,
        ) ?: false
        if (!args.installed && !args.errorMsg.isNullOrBlank()) {
            println("vpn candidate install failed generation=${args.generation}: ${args.errorMsg}")
        }
        val ret = JSObject()
        ret.put("accepted", accepted)
        invoke.resolve(ret)
    }

    @Command
    fun getVpnStatus(invoke: Invoke) {
        val ret = JSObject()
        ret.put("running", TauriVpnService.self != null)
        ret.put("ipv4Addr", TauriVpnService.ipv4Addr)
        ret.put("ipv6Addr", TauriVpnService.ipv6Addr)
        ret.put("routes", TauriVpnService.routes)
        ret.put("dnsServers", TauriVpnService.dnsServers)
        ret.put("generation", TauriVpnService.generation)
        ret.put("generationHighWater", TauriVpnService.getGenerationHighWater(activity))
        invoke.resolve(ret)
    }

    @Command
    fun getUnderlyingDnsServers(invoke: Invoke) {
        val ret = JSObject()
        try {
            val connectivity = activity.getSystemService(Activity.CONNECTIVITY_SERVICE) as ConnectivityManager
            val dnsServers = linkedSetOf<String>()
            val systemDnsServers = linkedSetOf<String>()
            for (network in connectivity.allNetworks) {
                val capabilities = connectivity.getNetworkCapabilities(network) ?: continue
                if (!capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) ||
                    capabilities.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) continue
                val properties = connectivity.getLinkProperties(network) ?: continue
                for (address in properties.dnsServers) {
                    val server = address.hostAddress ?: continue
                    dnsServers.add(server)
                    systemDnsServers.add(server.substringBefore('%'))
                }
            }
            ret.put("dnsServers", dnsServers.toTypedArray())
            ret.put("systemDnsServers", systemDnsServers.toTypedArray())
        } catch (error: Exception) {
            ret.put("errorMsg", error.message ?: error.toString())
            ret.put("dnsServers", emptyArray<String>())
            ret.put("systemDnsServers", emptyArray<String>())
        }
        invoke.resolve(ret)
    }

    @Command
    fun consumeVpnTileAction(invoke: Invoke) {
        val ret = JSObject()
        ret.put("action", EasyTierVpnTileService.consumePendingAction(activity))
        invoke.resolve(ret)
    }
}
