package io.kubemaxx.st

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.max

internal class AvcSurfaceDecoder(
    private val nativeHandle: Long,
    private val sessionEpoch: Long,
    private val stream: StreamDescription,
    private val surface: Surface,
    private val onStatus: (String) -> Unit,
) {
    private val running = AtomicBoolean(true)
    private val worker = Thread(::decodeLoop, "st-mediacodec")

    fun start() {
        worker.start()
    }

    @Synchronized
    fun stop(): Boolean {
        running.set(false)
        worker.join(500)
        return !worker.isAlive
    }

    private fun decodeLoop() {
        var codec: MediaCodec? = null
        var pending: ByteArray? = null
        var lastPresentationUs = 0L
        var configuredSps: ByteArray? = null
        var configuredPps: ByteArray? = null
        var cachedSps: ByteArray? = null
        var cachedPps: ByteArray? = null
        var renderedFrame = false
        onStatus("waiting for H.264 keyframe")

        try {
            while (running.get()) {
                try {
                    val accessUnit = toAvcAnnexB(
                        pending
                        ?: NativeBridge.nativePollAccessUnit(nativeHandle, sessionEpoch, 20)
                        ?: run {
                            if (codec?.let(::drainOutput) == true && !renderedFrame) {
                                renderedFrame = true
                                onStatus("video active")
                            }
                            continue
                        },
                    )

                    val nals = scanAvcNals(accessUnit)
                    val nextSps = nals.firstOrNull { it.type == NAL_SPS }
                        ?.let { normalizedAvcNal(accessUnit, it) }
                    val nextPps = nals.firstOrNull { it.type == NAL_PPS }
                        ?.let { normalizedAvcNal(accessUnit, it) }
                    if (nextSps != null) cachedSps = nextSps
                    if (nextPps != null) cachedPps = nextPps
                    if (codec != null &&
                        ((nextSps != null && !nextSps.contentEquals(configuredSps)) ||
                            (nextPps != null && !nextPps.contentEquals(configuredPps)))
                    ) {
                        releaseCodec(codec)
                        codec = null
                    }

                    if (codec == null) {
                        if (nals.none { it.type == NAL_IDR }) {
                            NativeBridge.nativeRequestKeyframe(nativeHandle, sessionEpoch)
                            pending = null
                            continue
                        }
                        if (cachedSps == null || cachedPps == null) {
                            onStatus("waiting for H.264 SPS/PPS")
                            NativeBridge.nativeRequestKeyframe(nativeHandle, sessionEpoch)
                            pending = null
                            continue
                        }
                        codec = createCodec(
                            cachedSps,
                            cachedPps,
                            accessUnit.size,
                        )
                        configuredSps = cachedSps
                        configuredPps = cachedPps
                        renderedFrame = false
                        onStatus("decoder configured ${stream.width}x${stream.height}")
                    }

                    val payload = stripAvcParameterSets(accessUnit, nals)
                    if (payload.isEmpty()) {
                        pending = null
                        continue
                    }
                    val inputIndex = codec.dequeueInputBuffer(5_000)
                    if (inputIndex < 0) {
                        pending = accessUnit
                        if (drainOutput(codec) && !renderedFrame) {
                            renderedFrame = true
                            onStatus("video active")
                        }
                        continue
                    }
                    val input = codec.getInputBuffer(inputIndex)
                        ?: error("MediaCodec returned no input buffer")
                    check(input.capacity() >= payload.size) {
                        "H.264 access unit ${payload.size} exceeds MediaCodec input capacity ${input.capacity()}"
                    }
                    input.clear()
                    input.put(payload)
                    val presentationUs = max(lastPresentationUs + 1, System.nanoTime() / 1_000)
                    lastPresentationUs = presentationUs
                    codec.queueInputBuffer(inputIndex, 0, payload.size, presentationUs, 0)
                    pending = null
                    if (drainOutput(codec) && !renderedFrame) {
                        renderedFrame = true
                        onStatus("video active")
                    }
                } catch (error: Exception) {
                    Log.w(TAG, "decoder recovery", error)
                    onStatus("decoder recovery: ${error.message ?: error.javaClass.simpleName}")
                    releaseCodec(codec)
                    codec = null
                    configuredSps = null
                    configuredPps = null
                    pending = null
                    NativeBridge.nativeRequestKeyframe(nativeHandle, sessionEpoch)
                }
            }
        } finally {
            releaseCodec(codec)
        }
    }

    private fun createCodec(sps: ByteArray, pps: ByteArray, firstUnitSize: Int): MediaCodec {
        val format = MediaFormat.createVideoFormat(
            MediaFormat.MIMETYPE_VIDEO_AVC,
            stream.width,
            stream.height,
        )
        format.setInteger(MediaFormat.KEY_FRAME_RATE, stream.framerate)
        format.setInteger(MediaFormat.KEY_PRIORITY, 0)
        format.setFloat(MediaFormat.KEY_OPERATING_RATE, stream.framerate.toFloat())
        format.setInteger(
            MediaFormat.KEY_MAX_INPUT_SIZE,
            max(512 * 1024, max(firstUnitSize * 2, stream.width * stream.height / 2)),
        )
        format.setByteBuffer("csd-0", ByteBuffer.wrap(sps))
        format.setByteBuffer("csd-1", ByteBuffer.wrap(pps))
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }

        return createConfiguredCodec(
            create = { MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC) },
            configure = { it.configure(format, surface, null, 0) },
            start = MediaCodec::start,
            stop = MediaCodec::stop,
            release = MediaCodec::release,
        )
    }

    private fun drainOutput(codec: MediaCodec): Boolean {
        val info = MediaCodec.BufferInfo()
        var newestPicture = -1
        while (true) {
            when (val index = codec.dequeueOutputBuffer(info, 0)) {
                MediaCodec.INFO_TRY_AGAIN_LATER -> {
                    if (newestPicture >= 0) {
                        codec.releaseOutputBuffer(newestPicture, true)
                    }
                    return newestPicture >= 0
                }
                MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> Unit
                else -> if (index >= 0) {
                    val isPicture = info.size > 0 &&
                        info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0 &&
                        info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM == 0
                    if (isPicture) {
                        if (newestPicture >= 0) {
                            codec.releaseOutputBuffer(newestPicture, false)
                        }
                        newestPicture = index
                    } else {
                        codec.releaseOutputBuffer(index, false)
                    }
                }
            }
        }
    }

    private fun releaseCodec(codec: MediaCodec?) {
        releaseCodecResource(
            codec = codec,
            started = true,
            stop = MediaCodec::stop,
            release = MediaCodec::release,
        )
    }

    private companion object {
        const val TAG = "st-decoder"
        const val NAL_IDR = 5
        const val NAL_SPS = 7
        const val NAL_PPS = 8
    }
}

internal data class AvcNal(val start: Int, val payload: Int, val end: Int, val type: Int)

internal fun scanAvcNals(data: ByteArray): List<AvcNal> {
    val starts = ArrayList<Pair<Int, Int>>()
    var index = 0
    while (index + 3 < data.size) {
        val prefix = when {
            data[index] == 0.toByte() && data[index + 1] == 0.toByte() &&
                data[index + 2] == 0.toByte() && data[index + 3] == 1.toByte() -> 4

            data[index] == 0.toByte() && data[index + 1] == 0.toByte() &&
                data[index + 2] == 1.toByte() -> 3

            else -> 0
        }
        if (prefix == 0) {
            index++
        } else {
            starts += index to prefix
            index += prefix
        }
    }

    return starts.mapIndexedNotNull { position, (start, prefix) ->
        val payload = start + prefix
        if (payload >= data.size) {
            null
        } else {
            val end = starts.getOrNull(position + 1)?.first ?: data.size
            AvcNal(start, payload, end, data[payload].toInt() and 0x1f)
        }
    }
}

internal fun normalizedAvcNal(data: ByteArray, nal: AvcNal): ByteArray {
    val result = ByteArray(4 + nal.end - nal.payload)
    result[3] = 1
    data.copyInto(result, 4, nal.payload, nal.end)
    return result
}

internal fun toAvcAnnexB(data: ByteArray): ByteArray {
    if (scanAvcNals(data).isNotEmpty()) {
        return data
    }
    val output = ByteArrayOutputStream(data.size + 32)
    var offset = 0
    while (offset + 4 <= data.size) {
        val length = ((data[offset].toInt() and 0xff) shl 24) or
            ((data[offset + 1].toInt() and 0xff) shl 16) or
            ((data[offset + 2].toInt() and 0xff) shl 8) or
            (data[offset + 3].toInt() and 0xff)
        offset += 4
        if (length <= 0 || offset + length > data.size) {
            return data
        }
        output.write(byteArrayOf(0, 0, 0, 1))
        output.write(data, offset, length)
        offset += length
    }
    return if (offset == data.size && output.size() > 0) output.toByteArray() else data
}

internal fun stripAvcParameterSets(data: ByteArray, nals: List<AvcNal>): ByteArray {
    if (nals.none { it.type == 7 || it.type == 8 }) {
        return data
    }
    val output = ByteArrayOutputStream(data.size)
    for (nal in nals) {
        if (nal.type == 7 || nal.type == 8) continue
        output.write(byteArrayOf(0, 0, 0, 1))
        output.write(data, nal.payload, nal.end - nal.payload)
    }
    return output.toByteArray()
}

internal inline fun <T> createConfiguredCodec(
    create: () -> T,
    configure: (T) -> Unit,
    start: (T) -> Unit,
    stop: (T) -> Unit,
    release: (T) -> Unit,
): T {
    val codec = create()
    var started = false
    try {
        configure(codec)
        start(codec)
        started = true
        return codec
    } catch (error: Throwable) {
        releaseCodecResource(codec, started, stop, release)
        throw error
    }
}

internal inline fun <T> releaseCodecResource(
    codec: T?,
    started: Boolean,
    stop: (T) -> Unit,
    release: (T) -> Unit,
) {
    if (codec == null) return
    if (started) runCatching { stop(codec) }
    runCatching { release(codec) }
}
