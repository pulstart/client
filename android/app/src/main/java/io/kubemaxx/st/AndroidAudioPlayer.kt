package io.kubemaxx.st

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.os.Build
import android.os.Process
import android.os.SystemClock
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import kotlin.math.max
import kotlin.math.min

internal class AndroidAudioPlayer(
    private val nativeHandle: Long,
    private val sessionEpoch: Long,
    private val stream: StreamDescription,
    private val onStatus: (String) -> Unit,
    private val onTerminated: (String?) -> Unit,
) {
    private val started = AtomicBoolean(false)
    private val stopRequested = AtomicBoolean(false)
    private val running = AtomicBoolean(false)
    private val resetRequested = AtomicBoolean(false)
    private val underrunEvents = AtomicLong(0)
    private val activePrebufferMs = AtomicInteger(MIN_PREBUFFER_MS)
    @Volatile
    private var worker: Thread? = null

    fun start(): String? {
        if (!started.compareAndSet(false, true)) return null
        val configureError = NativeBridge.nativeConfigureAudio(
            nativeHandle,
            sessionEpoch,
            stream.audioSampleRate,
            stream.audioChannels,
            stream.packetDurationMs,
        )
        if (configureError != null) return configureError
        if (stopRequested.get()) return "audio start cancelled"
        val track = try {
            createAudioTrack()
        } catch (error: Exception) {
            return "audio output error: ${error.message ?: error.javaClass.simpleName}"
        }
        if (stopRequested.get()) {
            releaseTrack(track)
            return "audio start cancelled"
        }
        val enableError = NativeBridge.nativeSetAudioEnabled(nativeHandle, sessionEpoch, true)
        if (enableError != null) {
            releaseTrack(track)
            return enableError
        }

        running.set(true)
        val nextWorker = Thread({ playbackLoop(track) }, "st-audiotrack")
        worker = nextWorker
        if (stopRequested.get()) running.set(false)
        try {
            nextWorker.start()
        } catch (error: RuntimeException) {
            worker = null
            running.set(false)
            runCatching { NativeBridge.nativeSetAudioEnabled(nativeHandle, sessionEpoch, false) }
            releaseTrack(track)
            return "audio worker error: ${error.message ?: error.javaClass.simpleName}"
        }
        return null
    }

    fun stop() {
        stopRequested.set(true)
        requestAudioWorkerStop(running, worker)
    }

    fun resetForTransport() {
        if (running.get()) {
            resetRequested.set(true)
        }
    }

    fun debugStatus(): String {
        val queueStats = NativeBridge.nativeGetAudioQueueStats(nativeHandle, sessionEpoch)
        return "queue=${audioQueueOccupancy(queueStats)}, underruns=${underrunEvents.get()}, " +
            "localDrops=${audioLocalDropCount(queueStats)}, prebuffer=${activePrebufferMs.get()}ms"
    }

    private fun playbackLoop(initialTrack: AudioTrack) {
        runCatching { Process.setThreadPriority(Process.THREAD_PRIORITY_AUDIO) }
        val packetBytes = SAMPLE_RATE * CHANNELS * PCM_BYTES_PER_SAMPLE * stream.packetDurationMs / 1_000
        val recoveryPackets = MAX_RECOVERY_MS / stream.packetDurationMs + 1
        val maxWriteBytes = audioPrebufferTargetBytes(
            SAMPLE_RATE,
            CHANNELS,
            PCM_BYTES_PER_SAMPLE,
            MAX_NONBLOCKING_WRITE_MS,
        )
        val prebuffer = AdaptiveAudioPrebuffer()
        var track: AudioTrack? = initialTrack
        val buffer = ByteBuffer.allocateDirect(packetBytes * recoveryPackets)
            .order(ByteOrder.nativeOrder())
        var reportedActive = false
        var playing = false
        var primedBytes = 0
        var observedUnderruns = track?.let(::underrunCount) ?: 0
        var adaptationClockMs = SystemClock.elapsedRealtime()
        var terminalFailure: String? = null

        try {
            while (running.get()) {
                if (resetRequested.getAndSet(false)) {
                    NativeBridge.nativeResetAudio(nativeHandle, sessionEpoch)
                    track?.let(::pauseAndFlush)
                    playing = false
                    primedBytes = 0
                    reportedActive = false
                    observedUnderruns = track?.let(::underrunCount) ?: 0
                    prebuffer.reset()
                    activePrebufferMs.set(prebuffer.targetMs)
                    adaptationClockMs = SystemClock.elapsedRealtime()
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
                    adaptationClockMs = SystemClock.elapsedRealtime()
                }

                val adaptationNowMs = SystemClock.elapsedRealtime()
                val stableElapsedMs = (adaptationNowMs - adaptationClockMs).coerceAtLeast(0)
                adaptationClockMs = adaptationNowMs
                val currentUnderruns = underrunCount(track)
                val underrunPlan = planAudioUnderrun(observedUnderruns, currentUnderruns)
                if (playing && underrunPlan.rebuffer) {
                    // The device buffer drained (network jitter). Count it and
                    // deepen the cushion for future primes, but keep the track
                    // PLAYING: a drained MODE_STREAM track resumes cleanly on the
                    // next write. Pausing+flushing+re-priming here instead would
                    // discard already-buffered audio and inject a prebuffer-length
                    // silence gap on every jitter spike — a rapid gap+fade train
                    // that sounds robotic/metallic. The desktop client never
                    // flushes on underrun either; it keeps streaming and conceals.
                    val addedUnderruns = (currentUnderruns - observedUnderruns).coerceAtLeast(1)
                    underrunEvents.addAndGet(addedUnderruns.toLong())
                    prebuffer.onUnderrun(addedUnderruns)
                    activePrebufferMs.set(prebuffer.targetMs)
                    observedUnderruns = currentUnderruns
                } else if (playing) {
                    prebuffer.onStablePlayback(stableElapsedMs)
                    activePrebufferMs.set(prebuffer.targetMs)
                }
                observedUnderruns = max(observedUnderruns, currentUnderruns)

                buffer.clear()
                val fillResult = NativeBridge.nativeFillAudio(
                    nativeHandle,
                    sessionEpoch,
                    buffer,
                    POLL_TIMEOUT_MS,
                )
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
                        // Strip the resync flag but take no special action: the
                        // Rust decoder already reset and fade-in'd this frame, so
                        // playing it directly smooths the discontinuity. Flushing
                        // the track here is what produced the robotic artifact.
                        bytes = fillResult and FILL_RESYNC_FLAG.inv()
                        if (bytes == 0) continue
                        buffer.position(0)
                        buffer.limit(bytes)
                    }
                }

                var rebuild = false
                while (running.get() && buffer.hasRemaining()) {
                    val prebufferTargetBytes = audioPrebufferTargetBytes(
                        SAMPLE_RATE,
                        CHANNELS,
                        PCM_BYTES_PER_SAMPLE,
                        prebuffer.targetMs,
                    )
                    val requestedBytes = if (playing) {
                        min(buffer.remaining(), maxWriteBytes)
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
                            SystemClock.sleep(WRITE_RETRY_DELAY_MS)
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
            terminalFailure = "audio error: ${error.message ?: error.javaClass.simpleName}"
            onStatus(terminalFailure)
        } finally {
            running.set(false)
            releaseTrack(track)
            runCatching { NativeBridge.nativeSetAudioEnabled(nativeHandle, sessionEpoch, false) }
            worker = null
            onTerminated(terminalFailure)
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
            MAX_PREBUFFER_MS,
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
        const val MIN_PREBUFFER_MS = 25
        const val MAX_PREBUFFER_MS = 60
        const val MAX_NONBLOCKING_WRITE_MS = 10
        const val POLL_TIMEOUT_MS = 10
        const val REBUILD_DELAY_MS = 50L
        const val WRITE_RETRY_DELAY_MS = 2L
        const val FILL_BUFFER_ERROR = -1
        const val FILL_DECODE_ERROR = -2
        const val FILL_RESYNC_FLAG = 1 shl 30
    }
}

internal fun requestAudioWorkerStop(running: AtomicBoolean, worker: Thread?) {
    running.set(false)
    runCatching { worker?.interrupt() }
}

internal class AudioPlayerSlot<T : Any> {
    var active: T? = null
        private set
    private var retiring: T? = null
    private var attemptGeneration = 0

    fun canStart(generation: Int): Boolean =
        active == null && retiring == null && attemptGeneration != generation

    fun started(player: T, generation: Int) {
        check(active == null && retiring == null)
        active = player
        attemptGeneration = generation
    }

    fun failedToStart(player: T) {
        if (active === player) active = null
    }

    fun retireActive(): T? {
        if (retiring != null) return null
        return active?.also {
            active = null
            retiring = it
        }
    }

    fun terminated(player: T): Boolean {
        val owned = active === player || retiring === player
        if (active === player) active = null
        if (retiring === player) retiring = null
        if (owned) attemptGeneration = 0
        return owned
    }

    fun resetAttempt() {
        attemptGeneration = 0
    }
}

internal fun audioPrebufferTargetBytes(
    sampleRate: Int,
    channels: Int,
    bytesPerSample: Int,
    targetMs: Int = 25,
): Int = sampleRate * channels * bytesPerSample * targetMs / 1_000

internal fun audioUnderrunAdvanced(previous: Int, current: Int): Boolean = current > previous

internal data class AudioUnderrunPlan(
    val rebuffer: Boolean,
    val retainCompressedPackets: Boolean,
)

internal fun planAudioUnderrun(previous: Int, current: Int): AudioUnderrunPlan = AudioUnderrunPlan(
    rebuffer = audioUnderrunAdvanced(previous, current),
    retainCompressedPackets = true,
)

internal class AdaptiveAudioPrebuffer(
    private val minimumMs: Int = 25,
    private val maximumMs: Int = 60,
    private val stepMs: Int = 5,
    private val stableDecayMs: Long = 5_000,
) {
    var targetMs: Int = minimumMs
        private set
    private var stablePlaybackMs = 0L

    fun onUnderrun(count: Int = 1) {
        targetMs = (targetMs + stepMs * count.coerceAtLeast(1)).coerceAtMost(maximumMs)
        stablePlaybackMs = 0
    }

    fun onStablePlayback(elapsedMs: Long) {
        if (elapsedMs <= 0 || targetMs <= minimumMs) return
        stablePlaybackMs += elapsedMs
        while (stablePlaybackMs >= stableDecayMs && targetMs > minimumMs) {
            targetMs = (targetMs - stepMs).coerceAtLeast(minimumMs)
            stablePlaybackMs -= stableDecayMs
        }
    }

    fun reset() {
        targetMs = minimumMs
        stablePlaybackMs = 0
    }
}

internal fun audioQueueOccupancy(packed: Long): Long = packed and 0xffff_ffffL

internal fun audioLocalDropCount(packed: Long): Long = packed ushr 32

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
