package com.ultradisplay.app

import java.io.InputStream
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.security.MessageDigest
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLSocket
import javax.net.ssl.X509TrustManager

/** An open connection to the host. */
class Connection(
    val wifi: Boolean,
    val label: String,
    private val socket: Socket,
) {
    val input: InputStream get() = socket.getInputStream()
    val output: OutputStream get() = socket.getOutputStream()
    fun close() = runCatching { socket.close() }
}

object Transport {
    /** USB: `adb reverse` makes the host reachable on the tablet's loopback. */
    fun connectUsb(port: Int): Connection {
        val s = Socket()
        tune(s)
        s.connect(InetSocketAddress("127.0.0.1", port), 300)
        return Connection(wifi = false, label = "USB", socket = s)
    }

    /** Wi-Fi: TLS 1.3, accepting only the certificate the host paired with over USB. */
    fun connectWifi(address: String, host: PairedHost): Connection {
        val raw = Socket()
        tune(raw)
        raw.connect(InetSocketAddress(address, host.lanPort), 2000)
        val ctx = SSLContext.getInstance("TLSv1.3")
        ctx.init(null, arrayOf(PinnedTrustManager(host.fingerprint)), null)
        val tls = ctx.socketFactory.createSocket(raw, address, host.lanPort, true) as SSLSocket
        tls.enabledProtocols = arrayOf("TLSv1.3")
        tls.soTimeout = 5000
        tls.startHandshake()
        tls.soTimeout = 0
        return Connection(wifi = true, label = "Wi-Fi ${host.hostName} ($address)", socket = tls)
    }

    private fun tune(s: Socket) {
        s.tcpNoDelay = true
        s.receiveBufferSize = 4 shl 20
    }
}

/** Trusts exactly one certificate, identified by the SHA-256 of its DER encoding. */
private class PinnedTrustManager(private val fingerprint: String) : X509TrustManager {
    override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {
        val leaf = chain.firstOrNull() ?: throw CertificateException("empty chain")
        val actual = MessageDigest.getInstance("SHA-256").digest(leaf.encoded)
            .joinToString("") { "%02x".format(it) }
        if (!MessageDigest.isEqual(actual.toByteArray(), fingerprint.toByteArray())) {
            throw CertificateException("host certificate does not match the paired host")
        }
    }

    override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) =
        throw CertificateException("not a server")

    override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
}
