package io.kubemaxx.st

import android.app.Activity
import android.app.Presentation
import android.graphics.Color
import android.hardware.display.DisplayManager
import android.os.Handler
import android.view.Display
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView

internal data class AuxPickerEntry(val label: String, val target: AuxTarget)

internal fun auxPickerEntries(available: ServerOutputs, streamedOutputId: Long?): List<AuxPickerEntry> {
    val entries = available.outputs.map { AuxPickerEntry(it.label(), AuxTarget.Existing(it.id)) }
    val onVirtual = available.outputs.any { it.id == streamedOutputId && it.virtual }
    return if (available.virtualSupported && !onVirtual) {
        entries + AuxPickerEntry("New virtual display", AuxTarget.Virtual)
    } else {
        entries
    }
}

/** Like [outputChoiceToRemember], but a virtual pick is confirmed by the server streaming a virtual output. */
internal fun auxChoiceToRemember(
    pendingPick: Long?,
    streamedOutputId: Long,
    outputs: List<ServerOutput>,
): RememberedOutput? {
    if (pendingPick != OUTPUT_VIRTUAL) return outputChoiceToRemember(pendingPick, streamedOutputId, outputs)
    return RememberedOutput(OUTPUT_VIRTUAL, VIRTUAL_CHOICE_NAME)
        .takeIf { outputs.firstOrNull { it.id == streamedOutputId }?.virtual == true }
}

private const val VIRTUAL_CHOICE_NAME = "virtual"

internal interface AuxDisplayHost {
    val mainStream: StreamDescription?
    val mainConnected: Boolean
    val mainOutputs: ServerOutputs
    val serverAddress: String
    val token: String
    val maxRefreshHz: Int
    val scaleMode: VideoScaleMode
    val remembered: RememberedOutput?
    fun remember(choice: RememberedOutput)

    /** Anything the menu shows about the external display changed. */
    fun onAuxChanged()
}

/**
 * Shows a second server output on an attached presentation display (DeX,
 * HDMI, wireless display) through one extra video-only native session. It
 * never touches the main session: any failure only ends the external view.
 */
internal class AuxDisplayController(
    private val activity: Activity,
    private val handler: Handler,
    private val host: AuxDisplayHost,
) {
    private class Session(
        val display: Display,
        val presentation: AuxPresentation,
        val presenter: StreamPresenter,
        val target: AuxTarget,
    ) {
        var pendingPick: Long? = null
    }

    private data class Inputs(
        val displayId: Int?,
        val mainConnected: Boolean,
        val direct: Boolean,
        val mainOutputId: Long?,
        val outputs: ServerOutputs,
    )

    private val displayManager = activity.getSystemService(DisplayManager::class.java)
    private var session: Session? = null
    private var listening = false
    private var lastInputs: Inputs? = null
    private var externalDisplay: Display? = null

    var failure: String? = null
        private set

    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) {
            findExternalDisplay()
            reconcile()
        }

        override fun onDisplayRemoved(displayId: Int) {
            if (session?.display?.displayId == displayId) teardown()
            findExternalDisplay()
            reconcile()
        }

        override fun onDisplayChanged(displayId: Int) = Unit
    }

    fun onStart() {
        if (listening) return
        listening = true
        displayManager.registerDisplayListener(displayListener, handler)
        findExternalDisplay()
        reconcile()
    }

    fun onStop() {
        if (listening) displayManager.unregisterDisplayListener(displayListener)
        listening = false
        stop()
    }

    /** Ends the external view (main disconnected or the app is leaving the foreground). */
    fun stop() {
        failure = null
        lastInputs = null
        teardown()
    }

    fun release() {
        onStop()
    }

    fun setScaleMode(mode: VideoScaleMode) {
        session?.presenter?.setScaleMode(mode)
    }

    fun poll() {
        val current = session
        if (current != null) {
            current.presenter.pollStatus()
            val status = current.presenter.connectionStatus
            if (status.startsWith("error")) {
                fail(status)
                return
            }
            current.presentation.setStatus(current.presenter.statusLine())
        }
        if (listening) reconcile()
    }

    fun pollControl() {
        session?.presenter?.pollControl()
    }

    fun pickerVisible(): Boolean = session != null && auxPickerEntries(pickerOutputs(), streamedOutputId()).size > 1

    fun pickerEntries(): List<AuxPickerEntry> = auxPickerEntries(pickerOutputs(), streamedOutputId())

    fun pickerSelection(): Int = outputPickerIndex(pickerOutputs().outputs, streamedOutputId())

    fun pickerPending(): Int = outputPickerIndex(pickerOutputs().outputs, session?.pendingPick)

    fun pick(index: Int) {
        val current = session ?: return
        val entry = pickerEntries().getOrNull(index) ?: return
        when (val target = entry.target) {
            is AuxTarget.Existing -> {
                if (target.id == streamedOutputId()) return
                current.pendingPick = target.id
                if (!current.presenter.selectOutput(target.id)) current.pendingPick = null
            }
            AuxTarget.Virtual -> {
                val display = current.display
                teardown()
                start(display, AuxTarget.Virtual, userPick = true)
            }
        }
    }

    fun reconcile() {
        if (!listening) return
        val display = externalDisplay
        val stream = host.mainStream
        val inputs = Inputs(
            displayId = display?.displayId,
            mainConnected = host.mainConnected,
            direct = stream?.directTransport == true,
            mainOutputId = stream?.outputId,
            outputs = host.mainOutputs,
        )
        if (inputs == lastInputs) return
        lastInputs = inputs
        failure = null

        val current = session
        val wanted = auxSessionWanted(display != null, host.mainConnected, stream)
        if (display == null || !wanted) {
            teardown()
            return
        }
        if (current != null) {
            if (current.display.displayId != display.displayId) teardown() else return
        }
        val target = chooseAuxTarget(host.mainOutputs, stream?.outputId ?: OUTPUT_DEFAULT, host.remembered)
            ?: return
        start(display, target)
    }

    private fun start(display: Display, target: AuxTarget, userPick: Boolean = false) {
        val presentation = AuxPresentation(activity, display)
        lateinit var created: Session
        val presenter = StreamPresenter(
            presentation.context,
            handler,
            object : StreamPresenterListener {
                override fun onStreamApplied(
                    presenter: StreamPresenter,
                    stream: StreamDescription,
                    plan: StreamUpdatePlan,
                ) {
                    val choice = auxChoiceToRemember(
                        created.pendingPick,
                        stream.outputId,
                        presenter.outputs.outputs,
                    )
                    if (choice != null) {
                        created.pendingPick = null
                        host.remember(choice)
                    }
                    host.onAuxChanged()
                }

                override fun onOutputsChanged(presenter: StreamPresenter) = host.onAuxChanged()

                override fun onSurfaceReady(presenter: StreamPresenter) = begin(created)

                override fun onSurfaceDestroyed(presenter: StreamPresenter) {
                    if (session === created) handler.post { if (session === created) teardown() }
                }
            },
            host.scaleMode,
            "aux",
        )
        created = Session(display, presentation, presenter, target)
        if (userPick) created.pendingPick = auxTargetOutputId(target)
        presentation.attach(presenter)
        presentation.setOnDismissListener {
            handler.post { if (session === created) teardown() }
        }
        session = created
        try {
            presentation.show()
        } catch (error: WindowManager.InvalidDisplayException) {
            fail("external display unavailable: ${error.message ?: error.javaClass.simpleName}")
            return
        }
        host.onAuxChanged()
    }

    private fun begin(created: Session) {
        if (session !== created) return
        val (width, height) = displayPixelSize(created.display)
        val maxRefresh = host.maxRefreshHz * 1_000
        val refresh = displayRefreshMilliHz(created.display).takeIf { it > 0 }?.coerceAtMost(maxRefresh)
            ?: maxRefresh
        val error = created.presenter.start(
            StreamStart(
                server = host.serverAddress,
                token = host.token,
                refreshMilliHz = refresh,
                outputId = auxTargetOutputId(created.target),
                aux = true,
                displayWidth = width,
                displayHeight = height,
            ),
        )
        if (error != null) fail(error)
    }

    private fun fail(message: String) {
        failure = "External display: ${message.removePrefix("error: ")}"
        teardown()
    }

    private fun teardown() {
        val current = session ?: return
        session = null
        current.presentation.setOnDismissListener(null)
        current.presenter.stopSession()
        runCatching { current.presentation.dismiss() }
        current.presenter.release()
        host.onAuxChanged()
    }

    private fun findExternalDisplay() {
        val own = activity.currentDisplay().displayId
        externalDisplay = displayManager.getDisplays(DisplayManager.DISPLAY_CATEGORY_PRESENTATION)
            .firstOrNull { it.displayId != own && it.isValid }
    }

    private fun pickerOutputs(): ServerOutputs =
        session?.presenter?.outputs?.takeIf { it != ServerOutputs.NONE } ?: host.mainOutputs

    private fun streamedOutputId(): Long? = session?.presenter?.activeStream?.outputId
}

private fun StreamPresenter.statusLine(): String? = when {
    hasRenderedVideo && connected -> null
    decoderStatus.isNotEmpty() -> decoderStatus
    else -> connectionStatus
}

private class AuxPresentation(activity: Activity, display: Display) : Presentation(activity, display) {
    private val root = FrameLayout(context).apply { setBackgroundColor(Color.BLACK) }
    private var shown: String? = null
    private val status = TextView(context).apply {
        setTextColor(Color.WHITE)
        textSize = 16f
        gravity = Gravity.CENTER
    }

    fun attach(presenter: StreamPresenter) {
        val center = FrameLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.MATCH_PARENT,
            Gravity.CENTER,
        )
        root.addView(presenter.videoView, center)
        root.addView(presenter.cursorOverlay, FrameLayout.LayoutParams(center))
        root.addView(
            status,
            FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT,
                ViewGroup.LayoutParams.WRAP_CONTENT,
                Gravity.CENTER,
            ),
        )
        setContentView(root)
    }

    fun setStatus(text: String?) {
        if (text == shown) return
        shown = text
        status.text = text.orEmpty()
        status.visibility = if (text.isNullOrEmpty()) View.GONE else View.VISIBLE
    }
}
