package io.kubemaxx.st

import kotlin.math.hypot

internal enum class DragTransition {
    NONE,
    PRESS,
    RELEASE,
}

internal data class DragPointerDown(
    val transition: DragTransition,
    val longPressToken: Long?,
)

internal class TrackpadDragGesture(private val doubleTapTimeoutMs: Long) {
    private data class Tap(val timeMs: Long, val x: Float, val y: Float)

    private var previousTap: Tap? = null
    private var pointerDown: Tap? = null
    private var nextLongPressToken = 0L
    private var longPressToken: Long? = null
    var dragging = false
        private set

    fun pointerDown(timeMs: Long, x: Float, y: Float, doubleTapSlopPx: Float): DragPointerDown {
        val tap = previousTap
        previousTap = null
        pointerDown = Tap(timeMs, x, y)
        dragging = tap != null &&
            timeMs >= tap.timeMs &&
            timeMs - tap.timeMs <= doubleTapTimeoutMs &&
            hypot(x - tap.x, y - tap.y) <= doubleTapSlopPx.coerceAtLeast(0f)
        longPressToken = if (dragging) null else ++nextLongPressToken
        return DragPointerDown(
            transition = if (dragging) DragTransition.PRESS else DragTransition.NONE,
            longPressToken = longPressToken,
        )
    }

    fun pointerMoved(x: Float, y: Float, longPressSlopPx: Float): Boolean {
        val down = pointerDown ?: return false
        if (longPressToken == null ||
            hypot(x - down.x, y - down.y) <= longPressSlopPx.coerceAtLeast(0f)
        ) {
            return false
        }
        longPressToken = null
        return true
    }

    fun activateLongPress(token: Long): DragTransition {
        if (pointerDown == null || longPressToken != token || dragging) return DragTransition.NONE
        longPressToken = null
        previousTap = null
        dragging = true
        return DragTransition.PRESS
    }

    fun pointerUp(timeMs: Long, x: Float, y: Float, rememberTap: Boolean): DragTransition {
        if (pointerDown == null) return DragTransition.NONE
        val transition = if (dragging) DragTransition.RELEASE else DragTransition.NONE
        dragging = false
        longPressToken = null
        pointerDown = null
        previousTap = if (transition == DragTransition.NONE && rememberTap) {
            Tap(timeMs, x, y)
        } else {
            null
        }
        return transition
    }

    fun cancel(): DragTransition {
        val transition = if (dragging) DragTransition.RELEASE else DragTransition.NONE
        dragging = false
        longPressToken = null
        pointerDown = null
        previousTap = null
        return transition
    }
}
