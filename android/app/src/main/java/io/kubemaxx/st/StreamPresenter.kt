package io.kubemaxx.st

import android.content.Context
import android.os.Handler
import android.os.SystemClock
import android.view.SurfaceHolder
import android.view.SurfaceView
import java.util.concurrent.Executors

internal interface StreamPresenterListener {
    /** Anything the status UI shows changed. */
    fun onStateChanged(presenter: StreamPresenter) = Unit

    /** After the native status was polled while a session is started. */
    fun onConnectionPolled(presenter: StreamPresenter, wasConnected: Boolean, pending: Boolean) = Unit

    /** A stream event arrived, before the presenter adopts it. */
    fun onStreamPlanned(presenter: StreamPresenter, plan: StreamUpdatePlan) = Unit

    /** A stream event was adopted and its decoder/acceptance transition queued. */
    fun onStreamApplied(presenter: StreamPresenter, stream: StreamDescription, plan: StreamUpdatePlan) = Unit

    fun onControlSnapshot(presenter: StreamPresenter, update: CursorSnapshotUpdate) = Unit

    fun onOutputsChanged(presenter: StreamPresenter) = Unit

    fun onSurfaceReady(presenter: StreamPresenter) = Unit

    fun onSurfaceDestroyed(presenter: StreamPresenter) = Unit
}

internal data class StreamStart(
    val server: String,
    val token: String,
    val refreshMilliHz: Int,
    val apiUrl: String = "",
    val clientPeerId: String = "",
    val hostPeerId: String = "",
    val requestNonce: Long = 0L,
    val outputId: Long = OUTPUT_DEFAULT,
    val aux: Boolean = false,
    val displayWidth: Int = 0,
    val displayHeight: Int = 0,
)

/**
 * One native session shown on one surface: the decoder, the stream
 * generation/epoch handshake with the core, status polling and the cursor
 * overlay. The main view and every external-display view each own one.
 */
internal class StreamPresenter(
    context: Context,
    private val handler: Handler,
    private val listener: StreamPresenterListener,
    scaleMode: VideoScaleMode,
    private val label: String,
) : SurfaceHolder.Callback {
    private data class StreamTransitionRequest(
        val sessionEpoch: Long,
        val stream: StreamDescription,
        val restartDecoder: Boolean,
        val previousDecoder: AvcSurfaceDecoder?,
    )

    private enum class StreamTransitionResult { ACCEPTED, RETRY, SUPERSEDED }

    val nativeHandle: Long = NativeBridge.nativeCreate()
    val videoView = AspectSurfaceView(context)
    val cursorOverlay = CursorOverlayView(context, videoView)

    var sessionStarted = false
        private set
    var currentSessionEpoch = 0L
        private set
    var surfaceReady = false
        private set
    var connected = false
        private set
    var connectionStatus = "disconnected"
    var decoderStatus = ""
        private set
    var hasRenderedVideo = false
        private set
    var activeStream: StreamDescription? = null
        private set
    var outputs: ServerOutputs = ServerOutputs.NONE
        private set

    private var decoder: AvcSurfaceDecoder? = null
    private val retiredDecoders = mutableListOf<AvcSurfaceDecoder>()
    private var pendingDecoderStream: StreamDescription? = null
    private var pendingStreamAcceptance: StreamDescription? = null
    private var decoderTransitionStartedAt = 0L
    private var streamGeneration = 0
    private var acceptedVideoEpoch = 0L
    private var outputsTransportGeneration = 0
    private var released = false

    private val transitionExecutor = Executors.newSingleThreadExecutor { runnable ->
        Thread(runnable, "st-stream-transition-$label")
    }
    private val streamTransitions = LatestAsyncTask(
        executor = transitionExecutor,
        deliver = { action -> handler.post { action() } },
        work = ::performStreamTransition,
        complete = ::completeStreamTransition,
    )

    init {
        videoView.setScaleMode(scaleMode)
        videoView.holder.addCallback(this)
        videoView.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> cursorOverlay.invalidate() }
    }

    fun setScaleMode(mode: VideoScaleMode) {
        videoView.setScaleMode(mode)
        cursorOverlay.invalidate()
    }

    /** Starts the native session; returns an error message, or null once it is running. */
    fun start(params: StreamStart): String? {
        cursorOverlay.clearSession()
        outputs = ServerOutputs.NONE
        outputsTransportGeneration = 0
        currentSessionEpoch = 0L
        sessionStarted = false
        val error = NativeBridge.nativeStart(
            nativeHandle,
            params.server,
            params.token,
            params.refreshMilliHz,
            params.apiUrl,
            params.clientPeerId,
            params.hostPeerId,
            params.requestNonce,
            params.outputId,
            params.aux,
            params.displayWidth,
            params.displayHeight,
        )
        if (error != null) {
            connectionStatus = error
            listener.onStateChanged(this)
            return error
        }
        val sessionEpoch = NativeBridge.nativeGetSessionEpoch(nativeHandle)
        if (sessionEpoch == 0L) {
            NativeBridge.nativeStop(nativeHandle)
            connectionStatus = "native session did not start"
            listener.onStateChanged(this)
            return connectionStatus
        }
        currentSessionEpoch = sessionEpoch
        sessionStarted = true
        connectionStatus = "connecting"
        listener.onStateChanged(this)
        return null
    }

    fun stopSession() {
        streamTransitions.cancel()
        val wasStarted = sessionStarted
        currentSessionEpoch = 0L
        sessionStarted = false
        decoder?.let {
            it.stop()
            retiredDecoders += it
        }
        decoder = null
        decoderStatus = ""
        hasRenderedVideo = false
        activeStream = null
        pendingDecoderStream = null
        pendingStreamAcceptance = null
        decoderTransitionStartedAt = 0L
        streamGeneration = 0
        acceptedVideoEpoch = 0L
        connected = false
        connectionStatus = "disconnected"
        outputs = ServerOutputs.NONE
        outputsTransportGeneration = 0
        cursorOverlay.clearSession()
        if (wasStarted) {
            NativeBridge.nativeStop(nativeHandle)
        }
    }

    /** Stops the session and frees the native handle once the decoders have wound down. */
    fun release() {
        if (released) return
        released = true
        streamTransitions.cancel()
        transitionExecutor.shutdownNow()
        currentSessionEpoch = 0L
        sessionStarted = false
        val decodersToStop = retiredDecoders.toMutableList().apply {
            decoder?.let(::add)
        }
        retiredDecoders.clear()
        decoder = null
        Thread({
            decodersToStop.forEach { it.stopAndWait(500) }
            NativeBridge.nativeDestroy(nativeHandle)
        }, "st-native-destroy").start()
    }

    fun selectOutput(outputId: Long): Boolean {
        val epoch = currentSessionEpoch
        if (!connected || epoch == 0L) return false
        return NativeBridge.nativeSelectOutput(nativeHandle, epoch, outputId)
    }

    fun pollStatus() {
        val sessionEpoch = currentSessionEpoch
        if (sessionStarted && sessionEpoch != 0L) {
            val status = NativeBridge.nativeGetStatus(nativeHandle)
            val wasConnected = connected
            connected = status == "connected"
            val connectionPending = isConnectionPending(status)
            connectionStatus = status
            listener.onConnectionPolled(this, wasConnected, connectionPending)
            if (!connectionPending && !connected) {
                cursorOverlay.clearSession()
            }
        } else {
            connected = false
        }
        if (sessionStarted && sessionEpoch != 0L && surfaceReady) {
            NativeBridge.nativeTakeStreamConfig(nativeHandle, sessionEpoch)
                ?.let(StreamDescription::from)
                ?.takeIf { currentSessionEpoch == sessionEpoch }
                ?.let(::applyStreamConfig)
        }
        retiredDecoders.removeAll { it.stop() }
    }

    fun pollControl() {
        val sessionEpoch = currentSessionEpoch
        if (!sessionStarted || sessionEpoch == 0L) return
        val update = NativeBridge.nativePollCursorSnapshot(nativeHandle, sessionEpoch)
            ?.let(CursorSnapshotUpdate::from)
            ?.takeIf { currentSessionEpoch == sessionEpoch }
            ?: return
        var outputsChanged = false
        if (outputsTransportGeneration != update.transportGeneration) {
            outputsTransportGeneration = update.transportGeneration
            outputsChanged = outputs != ServerOutputs.NONE
            outputs = ServerOutputs.NONE
        }
        update.outputs?.let {
            outputsChanged = outputsChanged || outputs != it.value
            outputs = it.value
        }
        cursorOverlay.applyUpdate(update)
        listener.onControlSnapshot(this, update)
        if (outputsChanged) listener.onOutputsChanged(this)
    }

    private fun applyStreamConfig(stream: StreamDescription) {
        val sessionEpoch = currentSessionEpoch
        if (!sessionStarted || sessionEpoch == 0L) return
        val previous = activeStream
        val plan = planStreamUpdate(
            previous,
            stream,
            decoderReady = decoder != null && pendingDecoderStream == null,
        )
        val decoderChanged = plan.restartDecoder
        listener.onStreamPlanned(this, plan)
        activeStream = stream
        val (uprightWidth, uprightHeight) = stream.uprightWidth to stream.uprightHeight
        videoView.setVideoSize(uprightWidth, uprightHeight)
        cursorOverlay.updateStreamConfig(
            stream.generation,
            uprightWidth,
            uprightHeight,
            stream.cursorWidth,
            stream.cursorHeight,
        )
        if (decoderChanged) {
            pendingStreamAcceptance = null
            if (pendingDecoderStream?.generation != stream.generation ||
                pendingDecoderStream?.videoEpoch != stream.videoEpoch
            ) {
                decoderTransitionStartedAt = SystemClock.uptimeMillis()
            }
            pendingDecoderStream = stream
            decoderStatus = "starting decoder"
            queueStreamTransition(stream, restartDecoder = true)
        } else if (streamGeneration != stream.generation || acceptedVideoEpoch != stream.videoEpoch) {
            if (pendingStreamAcceptance?.generation != stream.generation ||
                pendingStreamAcceptance?.videoEpoch != stream.videoEpoch
            ) {
                decoderTransitionStartedAt = SystemClock.uptimeMillis()
            }
            pendingStreamAcceptance = stream
            queueStreamTransition(stream, restartDecoder = false)
        }
        listener.onStreamApplied(this, stream, plan)
    }

    private fun queueStreamTransition(stream: StreamDescription, restartDecoder: Boolean) {
        val sessionEpoch = currentSessionEpoch
        if (!sessionStarted || sessionEpoch == 0L) return
        streamTransitions.submit(
            StreamTransitionRequest(sessionEpoch, stream, restartDecoder, decoder),
        )
    }

    private fun performStreamTransition(
        request: StreamTransitionRequest,
        isCurrent: () -> Boolean,
    ): StreamTransitionResult {
        if (request.restartDecoder) {
            val stopped = runCatching {
                request.previousDecoder?.stopAndWait(DECODER_STOP_WAIT_MS) != false
            }.getOrDefault(false)
            if (!isCurrent()) return StreamTransitionResult.SUPERSEDED
            if (!stopped) return StreamTransitionResult.RETRY
        }
        if (!isCurrent()) return StreamTransitionResult.SUPERSEDED
        val accepted = runCatching {
            NativeBridge.nativeAcceptStreamGeneration(
                nativeHandle,
                request.sessionEpoch,
                request.stream.generation,
                request.stream.videoEpoch,
            )
        }.getOrDefault(false)
        return if (accepted) {
            StreamTransitionResult.ACCEPTED
        } else {
            StreamTransitionResult.RETRY
        }
    }

    private fun completeStreamTransition(
        request: StreamTransitionRequest,
        result: StreamTransitionResult,
    ) {
        val stream = request.stream
        if (!sessionStarted || currentSessionEpoch != request.sessionEpoch ||
            activeStream?.generation != stream.generation || activeStream?.videoEpoch != stream.videoEpoch
        ) {
            return
        }
        if (request.restartDecoder && request.previousDecoder?.stop() != false &&
            decoder === request.previousDecoder
        ) {
            decoder = null
        }
        if (result != StreamTransitionResult.ACCEPTED) {
            if (result == StreamTransitionResult.RETRY &&
                SystemClock.uptimeMillis() - decoderTransitionStartedAt < DECODER_TRANSITION_TIMEOUT_MS
            ) {
                decoderStatus = if (request.restartDecoder) {
                    "waiting for previous decoder to stop"
                } else {
                    "waiting to accept video configuration"
                }
                handler.postDelayed(
                    {
                        if (sessionStarted && currentSessionEpoch == request.sessionEpoch &&
                            activeStream?.generation == stream.generation &&
                            activeStream?.videoEpoch == stream.videoEpoch
                        ) {
                            queueStreamTransition(stream, request.restartDecoder)
                        }
                    },
                    STREAM_STATUS_POLL_MS,
                )
            } else if (result == StreamTransitionResult.RETRY) {
                pendingDecoderStream = null
                pendingStreamAcceptance = null
                decoderStatus = "decoder restart timed out; reconnect required"
            }
            listener.onStateChanged(this)
            return
        }
        streamGeneration = stream.generation
        acceptedVideoEpoch = stream.videoEpoch
        if (!request.restartDecoder) {
            pendingStreamAcceptance = null
            if (hasRenderedVideo) decoderStatus = VIDEO_ACTIVE_STATUS
            listener.onStateChanged(this)
            return
        }
        startDecoder(stream, request.sessionEpoch)
    }

    private fun startDecoder(stream: StreamDescription, sessionEpoch: Long) {
        val next = AvcSurfaceDecoder(nativeHandle, sessionEpoch, stream, videoView.holder.surface) { status ->
            handler.post {
                if (currentSessionEpoch == sessionEpoch && streamGeneration == stream.generation &&
                    acceptedVideoEpoch == stream.videoEpoch
                ) {
                    decoderStatus = status
                    if (status == VIDEO_ACTIVE_STATUS) hasRenderedVideo = true
                    listener.onStateChanged(this)
                }
            }
        }
        try {
            next.start()
            decoder = next
            pendingDecoderStream = null
            decoderTransitionStartedAt = 0L
        } catch (error: RuntimeException) {
            next.stop()
            NativeBridge.nativeRequestKeyframe(nativeHandle, sessionEpoch)
            val timedOut = SystemClock.uptimeMillis() - decoderTransitionStartedAt >=
                DECODER_TRANSITION_TIMEOUT_MS
            decoderStatus = if (timedOut) {
                pendingDecoderStream = null
                "decoder start failed; reconnect required: ${error.message ?: error.javaClass.simpleName}"
            } else {
                "decoder start failed; retrying: ${error.message ?: error.javaClass.simpleName}"
            }
            listener.onStateChanged(this)
            if (!timedOut) {
                handler.postDelayed(
                    {
                        if (sessionStarted && currentSessionEpoch == sessionEpoch &&
                            activeStream?.generation == stream.generation &&
                            activeStream?.videoEpoch == stream.videoEpoch
                        ) {
                            queueStreamTransition(stream, restartDecoder = true)
                        }
                    },
                    STREAM_STATUS_POLL_MS,
                )
            }
        }
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        surfaceReady = true
        listener.onSurfaceReady(this)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) = Unit

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        surfaceReady = false
        listener.onSurfaceDestroyed(this)
    }

    private companion object {
        const val DECODER_TRANSITION_TIMEOUT_MS = 3_000L
        const val DECODER_STOP_WAIT_MS = 50L
        const val VIDEO_ACTIVE_STATUS = "video active"
    }
}

internal class AspectSurfaceView(context: Context) : SurfaceView(context) {
    private var videoWidth = 0
    private var videoHeight = 0
    private var scaleMode = VideoScaleMode.FIT

    fun setScaleMode(mode: VideoScaleMode) {
        if (scaleMode == mode) return
        scaleMode = mode
        requestLayout()
    }

    /** Takes the upright picture size; the decoder rotates the encoded frame to match. */
    fun setVideoSize(width: Int, height: Int) {
        videoWidth = width
        videoHeight = height
        holder.setFixedSize(width, height)
        requestLayout()
    }

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val (width, height) = measureVideoView(
            MeasureSpec.getSize(widthMeasureSpec),
            MeasureSpec.getSize(heightMeasureSpec),
            videoWidth,
            videoHeight,
            scaleMode,
        )
        setMeasuredDimension(width, height)
    }
}
