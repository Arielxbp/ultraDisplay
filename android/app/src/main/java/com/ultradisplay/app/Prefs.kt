package com.ultradisplay.app

import android.content.Context
import org.json.JSONObject
import java.util.UUID

/** Data the host handed out over USB, needed to connect over Wi-Fi. */
data class PairedHost(
    val hostId: String,
    val hostName: String,
    val fingerprint: String,
    val lanPort: Int,
    val token: String,
) {
    companion object {
        fun fromJson(json: JSONObject) = PairedHost(
            hostId = json.getString("host_id"),
            hostName = json.getString("host_name"),
            fingerprint = json.getString("fingerprint"),
            lanPort = json.getInt("lan_port"),
            token = json.getString("token"),
        )
    }
}

/** App settings and pairing state (SharedPreferences, private to the app). */
class Prefs(context: Context) {
    private val sp = context.getSharedPreferences("ultradisplay", Context.MODE_PRIVATE)

    /** Stable per-install id, the host's pairing key for this tablet. */
    val clientId: String
        get() = sp.getString(KEY_CLIENT_ID, null) ?: UUID.randomUUID().toString().also {
            sp.edit().putString(KEY_CLIENT_ID, it).apply()
        }

    var pairedHost: PairedHost?
        get() = sp.getString(KEY_PAIRED, null)?.let { PairedHost.fromJson(JSONObject(it)) }
        set(value) {
            val json = value?.let {
                JSONObject()
                    .put("host_id", it.hostId).put("host_name", it.hostName)
                    .put("fingerprint", it.fingerprint).put("lan_port", it.lanPort)
                    .put("token", it.token)
                    .toString()
            }
            sp.edit().putString(KEY_PAIRED, json).apply()
        }

    /** Last address a Wi-Fi connection succeeded on, tried when mDNS finds nothing. */
    var lastHostAddress: String?
        get() = sp.getString(KEY_LAST_ADDR, null)
        set(value) = sp.edit().putString(KEY_LAST_ADDR, value).apply()

    /** Address typed by the user, for networks that block mDNS. */
    var manualHostAddress: String?
        get() = sp.getString(KEY_MANUAL_ADDR, null)
        set(value) = sp.edit().putString(KEY_MANUAL_ADDR, value?.ifBlank { null }).apply()

    companion object {
        private const val KEY_CLIENT_ID = "client_id"
        private const val KEY_PAIRED = "paired_host"
        private const val KEY_LAST_ADDR = "last_host_address"
        private const val KEY_MANUAL_ADDR = "manual_host_address"
    }
}
