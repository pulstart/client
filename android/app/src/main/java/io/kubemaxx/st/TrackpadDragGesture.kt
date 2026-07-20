package io.kubemaxx.st

import kotlin.math.hypot

internal class TrackpadDragGesture(private val doubleTapTimeoutMs: Long) {
    private data class Tap(val timeMs: Long, val x: Float, val y: Float)

    private var previousTap: Tap? = null
    var dragging = false
        private set

    fun pointerDown(timeMs: Long, x: Float, y: Float, doubleTapSlopPx: Float): Boolean {
        val tap = previousTap
        previousTap = null
        dragging = tap != null &&
            timeMs >= tap.timeMs &&
            timeMs - tap.timeMs <= doubleTapTimeoutMs &&
            hypot(x - tap.x, y - tap.y) <= doubleTapSlopPx.coerceAtLeast(0f)
        return dragging
    }

    fun tapUp(timeMs: Long, x: Float, y: Float): Boolean {
        val wasDragging = dragging
        dragging = false
        if (!wasDragging) previousTap = Tap(timeMs, x, y)
        return wasDragging
    }

    fun cancel(): Boolean {
        val wasDragging = dragging
        dragging = false
        previousTap = null
        return wasDragging
    }
}
