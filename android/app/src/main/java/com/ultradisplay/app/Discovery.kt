package com.ultradisplay.app

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.util.Log
import java.util.concurrent.ConcurrentHashMap

/**
 * Finds UltraDisplay hosts on the LAN via mDNS (`_ultradisplay._tcp`). Hosts are keyed by
 * the `id` TXT record, which the paired host also handed out over USB.
 */
class Discovery(context: Context) {
    private val nsd = context.getSystemService(NsdManager::class.java)
    private val found = ConcurrentHashMap<String, String>() // host id -> address
    private var listener: NsdManager.DiscoveryListener? = null

    fun addressOf(hostId: String): String? = found[hostId]

    fun start() {
        if (listener != null) return
        val l = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(serviceType: String) {}
            override fun onDiscoveryStopped(serviceType: String) {}
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "mDNS discovery failed: $errorCode")
            }
            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}
            override fun onServiceFound(info: NsdServiceInfo) = resolve(info)
            override fun onServiceLost(info: NsdServiceInfo) {}
        }
        listener = l
        nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, l)
    }

    fun stop() {
        listener?.let { runCatching { nsd.stopServiceDiscovery(it) } }
        listener = null
    }

    @Suppress("DEPRECATION") // resolveService is fine through API 33 (the Tab S7+)
    private fun resolve(info: NsdServiceInfo) {
        nsd.resolveService(info, object : NsdManager.ResolveListener {
            override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) {
                Log.d(TAG, "resolve ${info.serviceName} failed: $errorCode")
            }

            override fun onServiceResolved(info: NsdServiceInfo) {
                val id = info.attributes["id"]?.let { String(it) } ?: return
                val addr = info.host?.hostAddress ?: return
                Log.i(TAG, "found host ${info.serviceName} ($id) at $addr")
                found[id] = addr
            }
        })
    }

    companion object {
        private const val TAG = "UD.Discovery"
        private const val SERVICE_TYPE = "_ultradisplay._tcp"
    }
}
