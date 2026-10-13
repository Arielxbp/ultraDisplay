package com.ultradisplay.app

import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.util.Log
import android.view.Surface
import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong

/**
 * Hardware decoder rendering straight to [surface]. Input is fed from the network thread;
 * a dedicated thread drains output and releases every frame for display immediately
 * (no client-side pacing: the host already sends at the panel's rate).
 */
class VideoDecoder(private val surface: Surface) {
    private var codec: MediaCodec? = null
    private var outputThread: Thread? = null
    @Volatile private var running = false

    private var mime = MIME_HEVC
    private var width = 0
    private var height = 0
    private var fps = 120

    // pts -> System.nanoTime() at queueInputBuffer, for decode latency.
    private val queuedAt = ConcurrentHashMap<Long, Long>()
    val framesRendered = AtomicInteger()
    val decodeNanosTotal = AtomicLong()
    val decodeSamples = AtomicInteger()

    /** Applies a CONFIG message; the codec is (re)created on the next keyframe. */
    fun configure(codecName: String, width: Int, height: Int, fps: Int) {
        release()
        mime = if (codecName == "h264") MIME_AVC else MIME_HEVC
        this.width = width
        this.height = height
        this.fps = fps
    }

    /** Returns false if the frame couldn't be used and the host should send an IDR. */
    fun feed(au: ByteArray, offset: Int, keyframe: Boolean, ptsUs: Long): Boolean {
        var c = codec
        if (c == null) {
            if (!keyframe || width == 0) return false
            c = start(au, offset)
        }
        while (running) {
            val index = c.dequeueInputBuffer(20_000)
            if (index < 0) continue
            val buf = c.getInputBuffer(index)!!
            buf.clear()
            val len = au.size - offset
            if (len > buf.capacity()) {
                Log.e(TAG, "access unit $len B exceeds input buffer ${buf.capacity()} B")
                c.queueInputBuffer(index, 0, 0, ptsUs, 0)
                return false
            }
            buf.put(au, offset, len)
            queuedAt[ptsUs] = System.nanoTime()
            val flags = if (keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
            c.queueInputBuffer(index, 0, len, ptsUs, flags)
            return true
        }
        return false
    }

    private fun start(keyframe: ByteArray, offset: Int): MediaCodec {
        val format = MediaFormat.createVideoFormat(mime, width, height).apply {
            val csd = parameterSets(keyframe, offset, hevc = mime == MIME_HEVC)
            setByteBuffer("csd-0", ByteBuffer.wrap(csd))
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, width * height)
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_PRIORITY, 0) // realtime
            setInteger(MediaFormat.KEY_OPERATING_RATE, Short.MAX_VALUE.toInt())
            // Qualcomm: low-latency mode and output in decode order.
            setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
            setInteger("vendor.qti-ext-dec-picture-order.enable", 1)
        }
        val name = pickDecoder(format)
        val c = MediaCodec.createByCodecName(name)
        c.configure(format, surface, null, 0)
        c.start()
        Log.i(TAG, "decoder $name started: $mime ${width}x$height@$fps")
        codec = c
        running = true
        outputThread = Thread({ drain(c) }, "decoder-output").apply {
            priority = Thread.MAX_PRIORITY
            start()
        }
        return c
    }

    private fun drain(c: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (running) {
            val index = try {
                c.dequeueOutputBuffer(info, 20_000)
            } catch (e: IllegalStateException) {
                break
            }
            if (index < 0) continue
            queuedAt.remove(info.presentationTimeUs)?.let {
                decodeNanosTotal.addAndGet(System.nanoTime() - it)
                decodeSamples.incrementAndGet()
            }
            c.releaseOutputBuffer(index, true)
            framesRendered.incrementAndGet()
        }
    }

    fun release() {
        running = false
        outputThread?.join(500)
        outputThread = null
        codec?.let {
            try {
                it.stop()
            } catch (_: IllegalStateException) {
            }
            it.release()
        }
        codec = null
        queuedAt.clear()
    }

    companion object {
        private const val TAG = "UD.Decoder"
        const val MIME_HEVC = MediaFormat.MIMETYPE_VIDEO_HEVC
        const val MIME_AVC = MediaFormat.MIMETYPE_VIDEO_AVC

        /** Codec names the host may offer, for HELLO. */
        fun supportedCodecs(): List<String> {
            val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
            fun hw(mime: String) = list.codecInfos.any {
                !it.isEncoder && it.isHardwareAccelerated && it.supportedTypes.contains(mime)
            }
            return buildList {
                if (hw(MIME_HEVC)) add("hevc")
                if (hw(MIME_AVC)) add("h264")
            }
        }

        private fun pickDecoder(format: MediaFormat): String {
            val mime = format.getString(MediaFormat.KEY_MIME)!!
            val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
            return list.codecInfos.firstOrNull {
                !it.isEncoder && it.isHardwareAccelerated && !it.name.endsWith(".secure") &&
                    it.supportedTypes.contains(mime)
            }?.name ?: list.findDecoderForFormat(format)
            ?: throw IllegalStateException("no decoder for $mime")
        }

        /**
         * Extracts the parameter-set NAL units (HEVC VPS/SPS/PPS, H.264 SPS/PPS) from an
         * Annex-B keyframe, keeping their start codes, for use as csd-0.
         */
        fun parameterSets(au: ByteArray, offset: Int, hevc: Boolean): ByteArray {
            val out = ByteArrayOutputStream()
            val starts = mutableListOf<Int>() // index of first payload byte of each NAL
            var i = offset
            while (i + 3 <= au.size) {
                if (au[i].toInt() == 0 && au[i + 1].toInt() == 0 && au[i + 2].toInt() == 1) {
                    starts.add(i + 3)
                    i += 3
                } else {
                    i++
                }
            }
            for ((n, start) in starts.withIndex()) {
                if (start >= au.size) continue
                var end = if (n + 1 < starts.size) starts[n + 1] - 3 else au.size
                if (end > start && au[end - 1].toInt() == 0) end-- // 4-byte start code
                val header = au[start].toInt() and 0xff
                val isParamSet = if (hevc) {
                    ((header shr 1) and 0x3f) in 32..34
                } else {
                    (header and 0x1f) == 7 || (header and 0x1f) == 8
                }
                if (isParamSet) {
                    out.write(byteArrayOf(0, 0, 0, 1))
                    out.write(au, start, end - start)
                }
            }
            return out.toByteArray()
        }
    }
}
