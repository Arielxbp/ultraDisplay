package com.ultradisplay.app

import android.content.Context
import android.net.wifi.WifiManager
import android.util.Log
import android.view.Surface
import org.json.JSONArray
import org.json.JSONObject
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.io.DataInputStream
import java.io.IOException
import java.io.OutputStream
import java.util.Timer
import kotlin.concurrent.fixedRateTimer

/** What the HELLO message advertises about this device. */
data class DeviceInfo(
    val model: String,
    val width: Int,
    val height: Int,
    val refreshRates: List<Float>,
    val codecs: List<String>,
)

/**
 * Connects to the host and streams video into a [VideoDecoder] until [stop] is called.
 * USB (127.0.0.1 via `adb reverse`) is tried first; otherwise the host paired over USB is
 * reached over Wi-Fi (TLS pinned to its certificate).
 */
class StreamClient(
    context: Context,
    private val surface: Surface,
    private val device: DeviceInfo,
    private val onStatus: (String) -> Unit,
) {
    private val prefs = Prefs(context)
    private val discovery = Discovery(context)
    private val wifi = context.applicationContext.getSystemService(WifiManager::class.java)

    @Volatile private var stopped = false
    @Volatile private var connection: Connection? = null
    private var thread: Thread? = null
    /** Shown under the waiting message until the next successful connection. */
    @Volatile private var lastProblem: String? = null

    fun start() {
        discovery.start()
        thread = Thread(::runLoop, "stream-client").apply {
            priority = Thread.MAX_PRIORITY
            start()
        }
    }

    fun stop() {
        stopped = true
        connection?.close()
        thread?.join(1000)
        discovery.stop()
    }

    private fun runLoop() {
        while (!stopped) {
            val conn = open()
            if (conn == null) {
                onStatus(waitingMessage())
                Thread.sleep(1000)
                continue
            }
            connection = conn
            try {
                session(conn)
            } catch (e: IOException) {
                if (!stopped) Log.i(TAG, "${conn.label} session ended: ${e.message}")
            } catch (e: Exception) {
                Log.e(TAG, "session failed", e)
                lastProblem = "Error: ${e.message}"
            } finally {
                conn.close()
                connection = null
            }
            if (!stopped) Thread.sleep(500)
        }
    }

    private fun open(): Connection? {
        try {
            return Transport.connectUsb(Protocol.DEFAULT_PORT)
        } catch (_: IOException) {
        }
        val host = prefs.pairedHost ?: return null
        val candidates = listOfNotNull(
            discovery.addressOf(host.hostId),
            prefs.manualHostAddress,
            prefs.lastHostAddress,
        ).distinct()
        for (addr in candidates) {
            try {
                val conn = Transport.connectWifi(addr, host)
                prefs.lastHostAddress = addr
                return conn
            } catch (e: Exception) {
                Log.d(TAG, "Wi-Fi $addr: ${e.message}")
                if (e is javax.net.ssl.SSLException || e.cause is java.security.cert.CertificateException) {
                    lastProblem = "$addr is not the paired host (certificate mismatch)"
                }
            }
        }
        return null
    }

    private fun waitingMessage(): String {
        val host = prefs.pairedHost
        val wifiLine = if (host == null) {
            "Wi-Fi: not paired yet. Connect over USB once to pair."
        } else {
            val addr = discovery.addressOf(host.hostId) ?: prefs.manualHostAddress
            "Wi-Fi: looking for ${host.hostName}" + (addr?.let { " ($it)" } ?: "…")
        }
        return listOfNotNull(
            "Waiting for the UltraDisplay host",
            "USB: plug in the cable (USB debugging on)",
            wifiLine,
            lastProblem,
        ).joinToString("\n")
    }

    private fun session(conn: Connection) {
        val input = DataInputStream(BufferedInputStream(conn.input, 1 shl 20))
        val output = BufferedOutputStream(conn.output, 64 shl 10)

        send(output, Protocol.HELLO, hello().toString().toByteArray())

        val decoder = VideoDecoder(surface)
        val stats = StatsReporter(decoder, output, conn.label)
        val wifiLock = if (conn.wifi) {
            wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "ultradisplay").apply { acquire() }
        } else {
            null
        }
        try {
            while (!stopped) {
                val msg = Protocol.read(input)
                when (msg.type) {
                    Protocol.CONFIG -> {
                        val cfg = JSONObject(String(msg.payload))
                        Log.i(TAG, "${conn.label}: stream config $cfg")
                        decoder.configure(
                            cfg.getString("codec"), cfg.getInt("width"),
                            cfg.getInt("height"), cfg.getInt("fps"),
                        )
                        lastProblem = null
                        onStatus("")
                    }
                    Protocol.VIDEO -> {
                        stats.bytes += msg.payload.size
                        val key = (msg.flags and Protocol.FLAG_KEYFRAME) != 0
                        if (!decoder.feed(msg.payload, 8, key, msg.u64())) {
                            send(output, Protocol.REQUEST_IDR)
                        }
                    }
                    Protocol.PAIRING -> {
                        val host = PairedHost.fromJson(JSONObject(String(msg.payload)))
                        if (prefs.pairedHost != host) {
                            prefs.pairedHost = host
                            Log.i(TAG, "paired with ${host.hostName} for Wi-Fi")
                        }
                    }
                    Protocol.PONG -> stats.onPong(msg.u64())
                    Protocol.PING -> send(output, Protocol.PONG, msg.payload)
                    Protocol.BYE -> handleBye(String(msg.payload))
                }
            }
        } finally {
            wifiLock?.release()
            stats.cancel()
            decoder.release()
        }
    }

    private fun handleBye(reason: String): Nothing {
        lastProblem = when {
            reason.startsWith("not paired") -> {
                // The host no longer knows this tablet (e.g. its pairing data was reset).
                prefs.pairedHost = null
                "Wi-Fi pairing was rejected. Connect over USB to pair again."
            }
            reason == "busy" -> "The host is streaming to another tablet."
            else -> "Host: $reason"
        }
        throw IOException("host said bye: $reason")
    }

    private fun hello() = JSONObject().apply {
        put("protocol", Protocol.VERSION)
        put("device", device.model)
        put("width", device.width)
        put("height", device.height)
        put("refresh_rates", JSONArray(device.refreshRates.map { it.toDouble() }))
        put("codecs", JSONArray(device.codecs))
        put("client_id", prefs.clientId)
        prefs.pairedHost?.let { put("token", it.token) }
    }

    /** Sends STATS and PINGs every 500 ms, and feeds the on-screen overlay. */
    private inner class StatsReporter(
        private val decoder: VideoDecoder,
        private val output: OutputStream,
        private val label: String,
    ) {
        @Volatile var bytes = 0L
        @Volatile private var rttMs = 0f
        private var lastFrames = 0
        private var lastTime = System.nanoTime()

        private val timer: Timer = fixedRateTimer("stats", true, 500, 500) { tick() }

        private fun tick() {
            val now = System.nanoTime()
            val frames = decoder.framesRendered.get()
            val secs = (now - lastTime) / 1e9f
            val fps = (frames - lastFrames) / secs
            val samples = decoder.decodeSamples.getAndSet(0)
            val decodeMs = if (samples > 0) decoder.decodeNanosTotal.getAndSet(0) / 1e6f / samples else 0f
            val mbps = bytes * 8 / secs / 1e6f
            bytes = 0
            lastFrames = frames
            lastTime = now

            val body = JSONObject()
                .put("fps", fps.toDouble())
                .put("decode_ms", decodeMs.toDouble())
                .put("dropped", 0)
                .put("rtt_ms", rttMs.toDouble())
            try {
                send(output, Protocol.STATS, body.toString().toByteArray())
                send(output, Protocol.PING, Protocol.u64Bytes(System.nanoTime()))
            } catch (_: IOException) {
            }
            overlay = "%s · %.1f fps · decode %.1f ms · %.1f Mbit/s · rtt %.1f ms"
                .format(label, fps, decodeMs, mbps, rttMs)
        }

        fun onPong(sentNanos: Long) {
            rttMs = (System.nanoTime() - sentNanos) / 1e6f
        }

        fun cancel() = timer.cancel()
    }

    private fun send(out: OutputStream, type: Int, payload: ByteArray = ByteArray(0)) {
        synchronized(out) { Protocol.write(out, type, payload) }
    }

    companion object {
        private const val TAG = "UD.Client"

        /** Latest stats line for the overlay. */
        @Volatile var overlay: String = ""
    }
}
