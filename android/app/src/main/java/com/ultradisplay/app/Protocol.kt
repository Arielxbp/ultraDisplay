package com.ultradisplay.app

import java.io.DataInputStream
import java.io.EOFException
import java.io.IOException
import java.io.OutputStream
import java.nio.ByteBuffer

/** UltraDisplay wire protocol v1; see docs/protocol.md. */
object Protocol {
    const val VERSION = 1
    const val DEFAULT_PORT = 47800
    const val HEADER_LEN = 8
    const val MAX_PAYLOAD = 16 shl 20
    const val FLAG_KEYFRAME = 1

    const val HELLO = 0x01
    const val CONFIG = 0x02
    const val VIDEO = 0x03
    const val STATS = 0x04
    const val REQUEST_IDR = 0x05
    const val PING = 0x06
    const val PONG = 0x07
    const val BYE = 0x08
    const val PAIRING = 0x09

    class Message(val type: Int, val flags: Int, val payload: ByteArray) {
        fun u64(): Long = ByteBuffer.wrap(payload, 0, 8).long
    }

    fun read(input: DataInputStream): Message {
        val header = ByteArray(HEADER_LEN)
        try {
            input.readFully(header)
        } catch (e: EOFException) {
            throw IOException("host closed the connection", e)
        }
        if (header[0] != 'U'.code.toByte() || header[1] != 'D'.code.toByte()) {
            throw IOException("bad magic")
        }
        val len = ByteBuffer.wrap(header, 4, 4).int
        if (len < 0 || len > MAX_PAYLOAD) throw IOException("bad payload length $len")
        val payload = ByteArray(len)
        input.readFully(payload)
        return Message(header[2].toInt() and 0xff, header[3].toInt() and 0xff, payload)
    }

    /** Writes one message; callers synchronize on [out]. */
    fun write(out: OutputStream, type: Int, payload: ByteArray = ByteArray(0)) {
        val header = ByteBuffer.allocate(HEADER_LEN)
            .put('U'.code.toByte()).put('D'.code.toByte())
            .put(type.toByte()).put(0)
            .putInt(payload.size)
            .array()
        out.write(header)
        out.write(payload)
        out.flush()
    }

    fun u64Bytes(v: Long): ByteArray = ByteBuffer.allocate(8).putLong(v).array()
}
