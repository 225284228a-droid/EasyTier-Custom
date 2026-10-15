package com.plugin.vpnservice

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.content.Context
import android.net.VpnService
import android.system.OsConstants
import android.os.Build
import android.os.ParcelFileDescriptor
import android.os.Bundle
import android.content.pm.ServiceInfo
import androidx.core.app.NotificationCompat

import app.tauri.plugin.JSObject

class TauriVpnService : VpnService() {
    companion object {
        @JvmField var triggerCallback: (String, JSObject) -> Unit = { _, _ -> }
        @JvmField var self: TauriVpnService? = null
        @JvmField var ipv4Addrs: Array<String> = emptyArray()
        @JvmField var routes: Array<String> = emptyArray()
        @JvmField var dnsServers: Array<String> = emptyArray()
        @JvmField var generation: String? = null
        @JvmField var generationHighWater: String? = null

        const val IPV4_ADDRS = "IPV4_ADDRS"
        const val ROUTES = "ROUTES"
        const val DNS_SERVERS = "DNS_SERVERS"
        const val DISALLOWED_APPLICATIONS = "DISALLOWED_APPLICATIONS"
        const val MTU = "MTU"
        const val INSTANCE_IDS = "INSTANCE_IDS"
        const val GENERATION = "GENERATION"

        private const val NOTIFICATION_CHANNEL_ID = "easytier_vpn_channel"
        private const val NOTIFICATION_ID = 1356
        private const val GENERATION_PREFERENCES = "vpn_service_state"
        private const val GENERATION_HIGH_WATER_KEY = "generation_high_water"

        @JvmStatic
        fun getGenerationHighWater(context: Context): String {
            val persisted = context.getSharedPreferences(GENERATION_PREFERENCES, Context.MODE_PRIVATE)
                .getLong(GENERATION_HIGH_WATER_KEY, 0L)
            val current = generationHighWater?.toLongOrNull() ?: 0L
            return maxOf(persisted, current).toString()
        }
    }

    private val interfaceLifecycle by lazy {
        val preferences = getSharedPreferences(GENERATION_PREFERENCES, MODE_PRIVATE)
        VpnInterfaceLifecycle<ParcelFileDescriptor>(
            closeInterface = { it.close() },
            initialGeneration = preferences.getLong(GENERATION_HIGH_WATER_KEY, 0L),
            onGenerationReserved = { next ->
                generationHighWater = next.toString()
                preferences.edit().putLong(GENERATION_HIGH_WATER_KEY, next).apply()
            },
        )
    }
    // Read on the main thread (onStartCommand/onRevoke/onDestroy) and from
    // Tauri command dispatch threads (completeReconfigure).
    @Volatile private var disconnected = false

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        println("vpn on start command ${intent?.getExtras()} $intent")
        startVpnForegroundService()
        val args = intent?.getExtras()
        if (args == null) return START_STICKY

        val generationText = args.getString(GENERATION)
        val generation = generationText
            ?.takeIf { value -> value.isNotEmpty() && value.all { it in '0'..'9' } }
            ?.toLongOrNull()
        val instanceIds = args.getStringArray(INSTANCE_IDS) ?: emptyArray()
        if (generation == null || generation <= 0) {
            val eventData = JSObject()
            eventData.put("generation", generationText ?: "")
            eventData.put("instanceIds", instanceIds)
            eventData.put("error", "Invalid VPN generation")
            triggerCallback("vpn_service_error", eventData)
            return START_STICKY
        }
        if (!interfaceLifecycle.reserveGeneration(generation)) {
            println("ignoring stale vpn generation=$generation")
            return START_STICKY
        }
        try {
            val candidate = createVpnInterface(args)
            if (!interfaceLifecycle.addCandidate(generation, candidate)) {
                candidate.close()
                return START_STICKY
            }
            // A live candidate re-arms disconnect(): a later revoke or destroy
            // must still close this fd and notify the frontend. self must point
            // here again because onRevoke cleared it and onCreate does not
            // re-run while this instance stays alive.
            disconnected = false
            self = this

            ipv4Addrs = args.getStringArray(IPV4_ADDRS) ?: emptyArray()
            routes = args.getStringArray(ROUTES) ?: emptyArray()
            dnsServers = args.getStringArray(DNS_SERVERS) ?: emptyArray()
            TauriVpnService.generation = generation.toString()

            println("vpn candidate created ${candidate.fd} generation=$generation")
            val eventData = JSObject()
            eventData.put("fd", candidate.fd)
            eventData.put("generation", generation.toString())
            eventData.put("instanceIds", instanceIds)
            triggerCallback("vpn_service_start", eventData)
        } catch (error: Exception) {
            val eventData = JSObject()
            eventData.put("generation", generation.toString())
            eventData.put("instanceIds", instanceIds)
            eventData.put("error", error.message ?: error.toString())
            triggerCallback("vpn_service_error", eventData)
            println("vpn interface configuration failed generation=$generation: $error")
        }
        EasyTierVpnTileService.requestStateUpdate(this)

        return START_STICKY
    }

    override fun onCreate() {
        super.onCreate()
        self = this
        generationHighWater = getGenerationHighWater(this)
        println("vpn on create")
    }

    override fun onDestroy() {
        println("vpn on destroy")
        disconnect()
        setMainForegroundServiceEnabled(true)
        stopForeground(STOP_FOREGROUND_REMOVE)
        self = null
        EasyTierVpnTileService.requestStateUpdate(this)
        super.onDestroy()
    }

    override fun onRevoke() {
        println("vpn on revoke")
        disconnect()
        setMainForegroundServiceEnabled(true)
        stopForeground(STOP_FOREGROUND_REMOVE)
        self = null
        EasyTierVpnTileService.requestStateUpdate(this)
        super.onRevoke()
    }

    private fun disconnect() {
        if (disconnected) return
        disconnected = true
        if (self == this) {
            val eventData = JSObject()
            eventData.put("generation", generation)
            triggerCallback("vpn_service_stop", eventData)
        }
        interfaceLifecycle.closeAll()
        clearStatus()
    }

    private fun clearStatus() {
        ipv4Addrs = emptyArray()
        routes = emptyArray()
        dnsServers = emptyArray()
        generation = null
    }

    private fun startVpnForegroundService() {
        createNotificationChannel()

        val launchIntent = packageManager.getLaunchIntentForPackage(packageName)?.apply {
            addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        }
        val contentIntent = launchIntent?.let {
            PendingIntent.getActivity(
                this,
                0,
                it,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
        }
        val notification = NotificationCompat.Builder(this, NOTIFICATION_CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_menu_manage)
            .setContentTitle("EasyTier VPN is running")
            .setContentText("VPN connection is active")
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .apply { contentIntent?.let(::setContentIntent) }
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }

        // A TUN-backed network is now protected by this foreground service, so the
        // no-TUN keepalive service is redundant and would show a second notification.
        setMainForegroundServiceEnabled(false)
    }

    private fun setMainForegroundServiceEnabled(enabled: Boolean) {
        val intent = Intent().setClassName(packageName, "$packageName.MainForegroundService")
        if (!enabled) {
            stopService(intent)
            return
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            startForegroundService(intent)
        } else {
            startService(intent)
        }
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                NOTIFICATION_CHANNEL_ID,
                "EasyTier VPN",
                NotificationManager.IMPORTANCE_LOW,
            )
            getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        }
    }

    private fun createVpnInterface(args: Bundle?): ParcelFileDescriptor {
        var builder = Builder()
                .setSession("TauriVpnService")
                .setBlocking(false)
        
        var mtu = args?.getInt(MTU) ?: 1500
        val addresses = args?.getStringArray(IPV4_ADDRS) ?: emptyArray()
        var dnsServers = args?.getStringArray(DNS_SERVERS) ?: emptyArray()
        var routes = args?.getStringArray(ROUTES) ?: emptyArray()
        var disallowedApplications = args?.getStringArray(DISALLOWED_APPLICATIONS) ?: emptyArray()

        require(addresses.isNotEmpty()) { "VPN requires at least one address" }
        for (address in addresses.distinct()) {
            val ipParts = address.split("/")
            require(ipParts.size == 2) { "Invalid IP address CIDR" }
            builder.addAddress(ipParts[0], ipParts[1].toInt())
        }

        builder.setMtu(mtu)
        for (dns in dnsServers) {
            builder.addDnsServer(dns)
        }

        for (route in routes) {
            val ipParts = route.split("/")
            if (ipParts.size != 2) throw IllegalArgumentException("Invalid route cidr string")
            builder.addRoute(ipParts[0], ipParts[1].toInt())
        }

        val hasIpv6Traffic = addresses.any { it.substringBefore('/').contains(':') } ||
            routes.any { it.substringBefore('/').contains(':') } ||
            dnsServers.any { it.contains(':') }
        if (!hasIpv6Traffic) {
            builder.allowFamily(OsConstants.AF_INET6)
        }
        
        builder.addDisallowedApplication(packageName)
        for (app in disallowedApplications.distinct()) {
            if (app == packageName) continue
            builder.addDisallowedApplication(app)
        }

        return builder.also {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                it.setMetered(false)
            }
        }
        .establish()
        ?: throw IllegalStateException("Failed to init VpnService")
    }

    fun completeReconfigure(generation: String, installed: Boolean): Boolean {
        val parsedGeneration = generation.toLongOrNull() ?: return false
        val accepted = interfaceLifecycle.complete(parsedGeneration, installed)
        if (accepted && installed) disconnected = false
        return accepted
    }
}
