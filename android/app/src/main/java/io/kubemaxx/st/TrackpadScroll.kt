package io.kubemaxx.st

import kotlin.math.hypot

internal data class WheelDelta(val x: Int, val y: Int)

internal class WheelAccumulator(private val unitsPerDp: Float = 5f) {
    private var remainderX = 0f
    private var remainderY = 0f

    fun addPixels(deltaX: Float, deltaY: Float, density: Float): WheelDelta? {
        if (density <= 0f) return null
        remainderX += deltaX / density * unitsPerDp
        remainderY += deltaY / density * unitsPerDp
        val emittedX = remainderX.toInt().coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt())
        val emittedY = remainderY.toInt().coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt())
        if (emittedX == 0 && emittedY == 0) return null
        remainderX -= emittedX
        remainderY -= emittedY
        return WheelDelta(emittedX, emittedY)
    }

    fun reset() {
        remainderX = 0f
        remainderY = 0f
    }
}

internal class PointerTravelTracker {
    private val origins = mutableMapOf<Int, Pair<Float, Float>>()
    private var maximumTravel = 0f

    fun reset() {
        origins.clear()
        maximumTravel = 0f
    }

    fun pointerDown(id: Int, x: Float, y: Float) {
        if (id !in origins) origins[id] = x to y
    }

    fun pointerMoved(id: Int, x: Float, y: Float): Float {
        val origin = origins[id] ?: (x to y).also { origins[id] = it }
        maximumTravel = maxOf(maximumTravel, hypot(x - origin.first, y - origin.second))
        return maximumTravel
    }

    fun pointerUp(id: Int, x: Float, y: Float): Float {
        val travel = pointerMoved(id, x, y)
        origins.remove(id)
        return travel
    }
}
