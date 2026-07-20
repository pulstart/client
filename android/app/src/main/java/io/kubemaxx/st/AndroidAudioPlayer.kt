package io.kubemaxx.st

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.os.Build
import android.os.SystemClock
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.max
import kotlin.math.min

internal class AndroidAudioPlayer(
    private val nativeHandle: Long,
    private val sessionEpoch: Long,
    private val stream: StreamDescription,
    private val onStatus: (String) -> Unit,
) {
    private val running = AtomicBoolean(false)
    private val resetRequested = AtomicBoolean(false)
    private var worker: Thread? = null

    @Synchronized
    fun start(): String? {
        if (running.get()) return null
        val configureError = NativeBridge.nativeConfigureAudio(
            nativeHandle,
            sessionEpoch,
            stream.audioSampleRate,
            stream.audioChannels,
            stream.packetDurationMs,
        )
        if (configureError != null) return configureError
        val track = try {
            createAudioTrack()
        } catch (error: Exception) {
            return "audio output error: ${error.message ?: error.javaClass.simpleName}"
        }
        val enableError = NativeBridge.nativeSetAudioEnabled(nativeHandle, sessionEpoch, true)
        if (enableError != null) {
            releaseTrack(track)
            return enableError
        }

        running.set(true)
        val nextWorker = Thread({ playbackLoop(track) }, "st-audiotrack")
        worker = nextWorker
        try {
            nextWorker.start()
        } catch (error: RuntimeException) {
            worker = null
            running.set(false)
            NativeBridge.nativeSetAudioEnabled(nativeHandle, sessionEpoch, false)
            releaseTrack(track)
            return "audio worker error: ${error.message ?: error.javaClass.simpleName}"
        }
        return null
    }

    @Synchronized
    fun stop() {
        running.set(false)
        NativeBridge.nativeSetAudioEnabled(nativeHandle, sessionEpoch, false)
        val activeWorker = worker
        var interrupted = false
        while (activeWorker?.isAlive == true) {
            try {
                activeWorker.join()
            } catch (_: InterruptedException) {
                interrupted = true
            }
        }
        worker = null
        if (interrupted) Thread.currentThread().interrupt()
    }

    fun resetForTransport() {
        if (running.get()) {
            resetRequested.set(true)
        }
    }

    private fun playbackLoop(initialTrack: AudioTrack) {
        val packetBytes = SAMPLE_RATE * CHANNELS * PCM_BYTES_PER_SAMPLE * stream.packetDurationMs / 1_000
        val recoveryPackets = MAX_RECOVERY_MS / stream.packetDurationMs + 1
        val prebufferTargetBytes = audioPrebufferTargetBytes(
            SAMPLE_RATE,
            CHANNELS,
            PCM_BYTES_PER_SAMPLE,
        )
        var track: AudioTrack? = initialTrack
        val buffer = ByteBuffer.allocateDirect(packetBytes * recoveryPackets)
            .order(ByteOrder.nativeOrder())
        var reportedActive = false
        var playing = false
        var primedBytes = 0
        var observedUnderruns = track?.let(::underrunCount) ?: 0

        try {
            while (running.get()) {
                if (resetRequested.getAndSet(false)) {
                    NativeBridge.nativeResetAudio(nativeHandle, sessionEpoch)
                    track?.let(::pauseAndFlush)
                    playing = false
                    primedBytes = 0
                    reportedActive = false
                    observedUnderruns = track?.let(::underrunCount) ?: 0
                    continue
                }
                if (track == null) {
                    track = try {
                        createAudioTrack()
                    } catch (error: Exception) {
                        onStatus("audio output error: ${error.message ?: error.javaClass.simpleName}")
                        SystemClock.sleep(REBUILD_DELAY_MS)
                        continue
                    }
                    reportedActive = false
                    playing = false
                    primedBytes = 0
                    observedUnderruns = underrunCount(track)
                }

                val currentUnderruns = underrunCount(track)
                if (playing && audioUnderrunAdvanced(observedUnderruns, currentUnderruns)) {
                    onStatus("audio underrun; rebuffering")
                    NativeBridge.nativeResetAudio(nativeHandle, sessionEpoch)
                    pauseAndFlush(track)
                    playing = false
                    primedBytes = 0
                    reportedActive = false
                    observedUnderruns = currentUnderruns
                    continue
                }
                observedUnderruns = max(observedUnderruns, currentUnderruns)

                buffer.clear()
                val fillResult = NativeBridge.nativeFillAudio(
                    nativeHandle,
                    sessionEpoch,
                    buffer,
                    POLL_TIMEOUT_MS,
                )
                val hardResync: Boolean
                val bytes: Int
                when (fillResult) {
                    0 -> continue
                    FILL_BUFFER_ERROR -> {
                        onStatus("audio buffer configuration error")
                        break
                    }
                    FILL_DECODE_ERROR -> {
                        onStatus("Opus decode recovery")
                        NativeBridge.nativeResetAudio(nativeHandle, sessionEpoch)
                        pauseAndFlush(track)
                        playing = false
                        primedBytes = 0
                        reportedActive = false
                        continue
                    }
                    else -> if (fillResult < 0) {
                        onStatus("audio decoder error $fillResult")
                        NativeBridge.nativeResetAudio(nativeHandle, sessionEpoch)
                        pauseAndFlush(track)
                        playing = false
                        primedBytes = 0
                        reportedActive = false
                        continue
                    } else {
                        hardResync = fillResult and FILL_RESYNC_FLAG != 0
                        bytes = fillResult and FILL_RESYNC_FLAG.inv()
                        if (bytes == 0) continue
                        buffer.position(0)
                        buffer.limit(bytes)
                    }
                }
                if (hardResync) {
                    pauseAndFlush(track)
                    playing = false
                    primedBytes = 0
                    reportedActive = false
                }

                var rebuild = false
                while (running.get() && buffer.hasRemaining()) {
                    val requestedBytes = if (playing) {
                        buffer.remaining()
                    } else {
                        min(buffer.remaining(), (prebufferTargetBytes - primedBytes).coerceAtLeast(0))
                    }
                    if (requestedBytes == 0) {
                        track.play()
                        playing = true
                        continue
                    }
                    val written = track.write(buffer, requestedBytes, AudioTrack.WRITE_NON_BLOCKING)
                    when {
                        written > 0 -> {
                            if (!playing) {
                                primedBytes += written
                                if (primedBytes >= prebufferTargetBytes) {
                                    track.play()
                                    playing = true
                                }
                            }
                            if (playing && !reportedActive) {
                                reportedActive = true
                                onStatus("audio active")
                            }
                        }
                        written == 0 -> {
                            SystemClock.sleep(1)
                        }
                        written == AudioTrack.ERROR_DEAD_OBJECT -> {
                            onStatus("audio output restarting")
                            rebuild = true
                            break
                        }
                        else -> {
                            onStatus("audio write error $written")
                            rebuild = true
                            break
                        }
                    }
                }
                if (rebuild) {
                    releaseTrack(track)
                    track = null
                    NativeBridge.nativeResetAudio(nativeHandle, sessionEpoch)
                    SystemClock.sleep(REBUILD_DELAY_MS)
                }
            }
        } catch (error: Exception) {
            onStatus("audio error: ${error.message ?: error.javaClass.simpleName}")
        } finally {
            releaseTrack(track)
            NativeBridge.nativeSetAudioEnabled(nativeHandle, sessionEpoch, false)
        }
    }

    private fun createAudioTrack(): AudioTrack {
        val minBufferBytes = AudioTrack.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        check(minBufferBytes > 0) { "AudioTrack rejected the stereo PCM format ($minBufferBytes)" }
        val packetBytes = SAMPLE_RATE * CHANNELS * PCM_BYTES_PER_SAMPLE * stream.packetDurationMs / 1_000
        val prebufferBytes = audioPrebufferTargetBytes(
            SAMPLE_RATE,
            CHANNELS,
            PCM_BYTES_PER_SAMPLE,
        )
        val builder = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_GAME)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                    .build(),
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setSampleRate(SAMPLE_RATE)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .build(),
            )
            .setTransferMode(AudioTrack.MODE_STREAM)
            .setBufferSizeInBytes(max(minBufferBytes, prebufferBytes + packetBytes))
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            builder.setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
        }
        return buildValidatedAudioTrack(
            build = builder::build,
            isInitialized = { it.state == AudioTrack.STATE_INITIALIZED },
            release = ::releaseTrack,
        )
    }

    private fun releaseTrack(track: AudioTrack?) {
        if (track == null) return
        runCatching { track.pause() }
        runCatching { track.flush() }
        runCatching { track.stop() }
        runCatching { track.release() }
    }

    private fun pauseAndFlush(track: AudioTrack) {
        runCatching { track.pause() }
        runCatching { track.flush() }
    }

    private fun underrunCount(track: AudioTrack): Int = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
        runCatching { track.underrunCount }.getOrDefault(0)
    } else {
        0
    }

    private companion object {
        const val SAMPLE_RATE = 48_000
        const val CHANNELS = 2
        const val PCM_BYTES_PER_SAMPLE = 2
        const val MAX_RECOVERY_MS = 60
        const val POLL_TIMEOUT_MS = 10
        const val REBUILD_DELAY_MS = 50L
        const val FILL_BUFFER_ERROR = -1
        const val FILL_DECODE_ERROR = -2
        const val FILL_RESYNC_FLAG = 1 shl 30
    }
}

internal fun audioPrebufferTargetBytes(
    sampleRate: Int,
    channels: Int,
    bytesPerSample: Int,
): Int = sampleRate * channels * bytesPerSample * 25 / 1_000

internal fun audioUnderrunAdvanced(previous: Int, current: Int): Boolean = current > previous

internal inline fun <T> buildValidatedAudioTrack(
    build: () -> T,
    isInitialized: (T) -> Boolean,
    release: (T) -> Unit,
): T {
    val track = build()
    try {
        check(isInitialized(track)) { "AudioTrack failed to initialize" }
    } catch (error: Throwable) {
        runCatching { release(track) }
        throw error
    }
    return track
}
