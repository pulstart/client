package io.kubemaxx.st

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RectF
import android.os.SystemClock
import android.view.View
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.LinkedHashMap

internal class CursorOverlayView(
    context: Context,
    private val videoView: View,
) : View(context) {
    private data class CachedShape(
        val serial: Long,
        val width: Int,
        val height: Int,
        val hotspotX: Int,
        val hotspotY: Int,
        val bitmap: Bitmap,
    )

    private val shapes = LinkedHashMap<Long, CachedShape>(MAX_SHAPES, 0.75f, true)
    private val bitmapPaint = Paint().apply {
        isAntiAlias = false
        isDither = false
        isFilterBitmap = false
    }
    private val fallbackFill = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.WHITE
        style = Paint.Style.FILL
    }
    private val fallbackStroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.BLACK
        style = Paint.Style.STROKE
        strokeJoin = Paint.Join.ROUND
        strokeWidth = resources.displayMetrics.density * 2f
    }
    private val fallbackPath = Path()
    private val destination = RectF()

    private var capabilities: CursorCapabilities? = null
    private var controllerState: ControllerOwnership? = null
    private var cursorState: CursorStateData? = null
    private var latestShapeSerial: Long? = null
    private var capabilitiesRevision = 0L
    private var controllerRevision = 0L
    private var shapeRevision = 0L
    private var stateRevision = 0L
    private var controlTransportGeneration = 0

    private var streamGeneration = 0
    private var videoWidth = 0
    private var videoHeight = 0
    private var cursorWidth = 0
    private var cursorHeight = 0
    private var cursorStateWidth = 0
    private var cursorStateHeight = 0
    private var predictedTipX: Float? = null
    private var predictedTipY: Float? = null
    private var lastPredictionAt = 0L
    private var authoritativeAbsolutePrediction = false

    init {
        setBackgroundColor(Color.TRANSPARENT)
        isClickable = false
        isFocusable = false
        importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_NO
    }

    fun applyUpdate(update: CursorSnapshotUpdate) {
        if (controlTransportGeneration != update.transportGeneration) {
            clearControlState()
            controlTransportGeneration = update.transportGeneration
        }
        val wasInputEligible = inputEligible()
        val wasDrawable = cursorIsDrawable()
        var reseedForOwnership = false
        update.capabilities?.let {
            if (it.revision != capabilitiesRevision) {
                capabilitiesRevision = it.revision
                val reliabilityChanged = capabilities?.cursorPositionReliable != it.value.cursorPositionReliable
                capabilities = it.value
                if (reliabilityChanged && !it.value.cursorPositionReliable &&
                    !authoritativeAbsolutePrediction
                ) {
                    predictedTipX = null
                    predictedTipY = null
                    lastPredictionAt = 0L
                }
            }
        }
        update.controllerState?.let {
            if (it.revision != controllerRevision) {
                controllerRevision = it.revision
                reseedForOwnership = shouldReseedPredictionForOwnership(
                    controllerState,
                    it.value,
                    capabilities?.cursorPositionReliable == true,
                )
                controllerState = it.value
            }
        }
        update.shape?.let {
            if (it.revision != shapeRevision) {
                shapeRevision = it.revision
                cacheShape(it.value)
            }
        }
        update.state?.let {
            if (it.revision != stateRevision) {
                stateRevision = it.revision
                cursorState = it.value
                cursorStateWidth = cursorWidth
                cursorStateHeight = cursorHeight
            }
        }

        val inputEligible = inputEligible()
        val drawable = cursorIsDrawable()
        when {
            !inputEligible || !drawable -> resetPrediction()
            reseedForOwnership -> {
                resetPrediction()
                initializePrediction()
            }
            !wasInputEligible || !wasDrawable -> {
                resetPrediction()
                initializePrediction()
            }
            authoritativeAbsolutePrediction && !absolutePredictionEligible() -> {
                authoritativeAbsolutePrediction = false
            }
        }
        reanchorIdlePrediction()
        invalidate()
    }

    fun updateStreamConfig(
        generation: Int,
        videoWidth: Int,
        videoHeight: Int,
        cursorWidth: Int,
        cursorHeight: Int,
    ) {
        if (generation == streamGeneration && videoWidth == this.videoWidth &&
            videoHeight == this.videoHeight && cursorWidth == this.cursorWidth &&
            cursorHeight == this.cursorHeight
        ) {
            return
        }

        if (this.cursorWidth > 1 && this.cursorHeight > 1 && cursorWidth > 1 && cursorHeight > 1) {
            predictedTipX = predictedTipX?.let {
                it / (this.cursorWidth - 1) * (cursorWidth - 1)
            }
            predictedTipY = predictedTipY?.let {
                it / (this.cursorHeight - 1) * (cursorHeight - 1)
            }
        } else if (this.cursorWidth > 0 || this.cursorHeight > 0) {
            predictedTipX = null
            predictedTipY = null
        }
        streamGeneration = generation
        this.videoWidth = videoWidth
        this.videoHeight = videoHeight
        this.cursorWidth = cursorWidth
        this.cursorHeight = cursorHeight
        if (predictedTipX == null || predictedTipY == null) {
            initializePrediction()
        } else {
            clampPrediction()
        }
        invalidate()
    }

    fun predictRelativeTrackpadDelta(dx: Int, dy: Int) {
        authoritativeAbsolutePrediction = false
        if (cursorWidth <= 0 || cursorHeight <= 0 || !predictionEligible()) {
            return
        }
        val now = SystemClock.uptimeMillis()
        if (predictedTipX == null || predictedTipY == null ||
            capabilities?.cursorPositionReliable == true && !predictionIsRecent(now)
        ) {
            val serverTip = if (capabilities?.cursorPositionReliable == true) {
                serverTip(resolveShape(cursorState?.serial))
            } else {
                null
            }
            predictedTipX = serverTip?.first ?: predictedTipX ?: cursorWidth / 2f
            predictedTipY = serverTip?.second ?: predictedTipY ?: cursorHeight / 2f
        }
        predictedTipX = predictedTipX!! + dx
        predictedTipY = predictedTipY!! + dy
        clampPrediction()
        lastPredictionAt = now
        invalidate()
        postInvalidateDelayed(PREDICTION_TTL_MS + 1)
    }

    fun predictAbsoluteTrackpadTarget(normalizedX: Int, normalizedY: Int) {
        if (cursorWidth <= 0 || cursorHeight <= 0) {
            return
        }
        predictedTipX = normalizedX.coerceIn(0, NORMALIZED_MAX).toFloat() /
            NORMALIZED_MAX * (cursorWidth - 1).coerceAtLeast(0)
        predictedTipY = normalizedY.coerceIn(0, NORMALIZED_MAX).toFloat() /
            NORMALIZED_MAX * (cursorHeight - 1).coerceAtLeast(0)
        authoritativeAbsolutePrediction = true
        lastPredictionAt = SystemClock.uptimeMillis()
        clampPrediction()
        invalidate()
    }

    fun clearSession() {
        clearControlState()
        controlTransportGeneration = 0
        streamGeneration = 0
        videoWidth = 0
        videoHeight = 0
        cursorWidth = 0
        cursorHeight = 0
        invalidate()
    }

    private fun clearControlState() {
        shapes.values.forEach { it.bitmap.recycle() }
        shapes.clear()
        capabilities = null
        controllerState = null
        cursorState = null
        latestShapeSerial = null
        capabilitiesRevision = 0
        controllerRevision = 0
        shapeRevision = 0
        stateRevision = 0
        cursorStateWidth = 0
        cursorStateHeight = 0
        predictedTipX = null
        predictedTipY = null
        lastPredictionAt = 0
        authoritativeAbsolutePrediction = false
        invalidate()
    }

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        val capabilities = capabilities ?: return
        val state = cursorState ?: return
        if (!capabilities.separateCursor || !state.visible || state.appGrab ||
            cursorWidth <= 0 || cursorHeight <= 0
        ) {
            return
        }

        val videoRect = displayedVideoRect() ?: return
        val shape = resolveShape(state.serial)
        val (tipX, tipY) = displayedTip(shape, capabilities.cursorPositionReliable)
        val scaleX = videoRect.width() / cursorWidth
        val scaleY = videoRect.height() / cursorHeight
        val mappedTipX = videoRect.left + tipX * scaleX
        val mappedTipY = videoRect.top + tipY * scaleY

        if (shape == null) {
            drawFallbackArrow(canvas, mappedTipX, mappedTipY)
            return
        }

        val left = mappedTipX - shape.hotspotX * scaleX
        val top = mappedTipY - shape.hotspotY * scaleY
        destination.set(
            left,
            top,
            left + (shape.width * scaleX).coerceAtLeast(1f),
            top + (shape.height * scaleY).coerceAtLeast(1f),
        )
        canvas.drawBitmap(shape.bitmap, null, destination, bitmapPaint)
    }

    private fun displayedTip(shape: CachedShape?, positionReliable: Boolean): Pair<Float, Float> {
        val now = SystemClock.uptimeMillis()
        val predictedX = predictedTipX
        val predictedY = predictedTipY
        if (authoritativeAbsolutePrediction && absolutePredictionEligible() &&
            predictedX != null && predictedY != null
        ) {
            return predictedX to predictedY
        }
        if (predictionEligible() && predictedX != null && predictedY != null &&
            (predictionIsRecent(now) || !positionReliable)
        ) {
            return predictedX to predictedY
        }

        val serverTip = serverTip(shape)
        if (positionReliable && serverTip != null) {
            predictedTipX = serverTip.first
            predictedTipY = serverTip.second
            clampPrediction()
            return serverTip
        }
        if (predictionEligible() && predictedX != null && predictedY != null) {
            return predictedX to predictedY
        }
        val fallback = if (positionReliable) serverTip else null
        return (fallback ?: (cursorWidth / 2f to cursorHeight / 2f)).also {
            predictedTipX = it.first
            predictedTipY = it.second
            clampPrediction()
        }
    }

    private fun reanchorIdlePrediction() {
        if (predictedTipX == null || predictedTipY == null) {
            initializePrediction()
            return
        }
        if (authoritativeAbsolutePrediction && absolutePredictionEligible()) {
            return
        }
        if (!predictionEligible() || capabilities?.cursorPositionReliable != true ||
            predictionIsRecent(SystemClock.uptimeMillis())
        ) {
            return
        }
        serverTip(resolveShape(cursorState?.serial))?.let {
            predictedTipX = it.first
            predictedTipY = it.second
            clampPrediction()
        }
    }

    private fun initializePrediction() {
        val serverTip = if (capabilities?.cursorPositionReliable == true) {
            serverTip(resolveShape(cursorState?.serial))
        } else {
            null
        }
        if (serverTip != null) {
            predictedTipX = serverTip.first
            predictedTipY = serverTip.second
        } else if (cursorWidth > 0 && cursorHeight > 0) {
            predictedTipX = cursorWidth / 2f
            predictedTipY = cursorHeight / 2f
        }
        clampPrediction()
    }

    private fun predictionEligible(): Boolean {
        val capabilities = capabilities ?: return false
        return inputEligible() && cursorIsDrawable() &&
            (capabilities.mouseRelative || capabilities.mouseAbsolute)
    }

    private fun absolutePredictionEligible(): Boolean {
        val capabilities = capabilities ?: return false
        return inputEligible() && cursorIsDrawable() && capabilities.mouseAbsolute
    }

    private fun inputEligible(): Boolean {
        val capabilities = capabilities ?: return false
        val ownershipEligible = controllerState?.let(::controllerOwnershipAllowsInput) == true
        return ownershipEligible && (capabilities.mouseRelative || capabilities.mouseAbsolute)
    }

    private fun cursorIsDrawable(): Boolean {
        val state = cursorState ?: return false
        return capabilities?.separateCursor == true && state.visible && !state.appGrab
    }

    private fun resetPrediction() {
        predictedTipX = null
        predictedTipY = null
        lastPredictionAt = 0L
        authoritativeAbsolutePrediction = false
    }

    private fun clampPrediction() {
        if (cursorWidth <= 0 || cursorHeight <= 0) return
        predictedTipX = predictedTipX?.coerceIn(0f, (cursorWidth - 1).coerceAtLeast(0).toFloat())
        predictedTipY = predictedTipY?.coerceIn(0f, (cursorHeight - 1).coerceAtLeast(0).toFloat())
    }

    private fun serverTip(shape: CachedShape?): Pair<Float, Float>? {
        val state = cursorState ?: return null
        // resolveShape deliberately falls back to the latest bitmap when the
        // state serial is not cached; use the same bitmap's hotspot as drawing.
        val x = state.x.toFloat() + (shape?.hotspotX ?: 0)
        val y = state.y.toFloat() + (shape?.hotspotY ?: 0)
        return remapServerAxis(x, cursorStateWidth, cursorWidth) to
            remapServerAxis(y, cursorStateHeight, cursorHeight)
    }

    private fun remapServerAxis(value: Float, sourceExtent: Int, targetExtent: Int): Float {
        val targetMax = (targetExtent - 1).coerceAtLeast(0).toFloat()
        if (sourceExtent <= 1 || targetExtent <= 1) {
            return value.coerceIn(0f, targetMax)
        }
        return value.coerceIn(0f, (sourceExtent - 1).toFloat()) /
            (sourceExtent - 1) * targetMax
    }

    private fun predictionIsRecent(now: Long): Boolean =
        lastPredictionAt != 0L && now - lastPredictionAt < PREDICTION_TTL_MS

    private fun displayedVideoRect(): RectF? {
        if (videoView.width <= 0 || videoView.height <= 0) {
            return null
        }
        val left = videoView.x - x
        val top = videoView.y - y
        return RectF(left, top, left + videoView.width, top + videoView.height)
    }

    private fun resolveShape(serial: Long?): CachedShape? {
        if (serial != null) {
            return shapes[serial]
        }
        return latestShapeSerial?.let { shapes[it] }
    }

    private fun cacheShape(shape: CursorShapeData) {
        val bitmap = bitmapFromPremultipliedRgba(shape)
            ?: return
        shapes.remove(shape.serial)?.bitmap?.recycle()
        latestShapeSerial = shape.serial
        shapes[shape.serial] = CachedShape(
            serial = shape.serial,
            width = shape.width,
            height = shape.height,
            hotspotX = shape.hotspotX,
            hotspotY = shape.hotspotY,
            bitmap = bitmap,
        )
        while (shapes.size > MAX_SHAPES) {
            val oldest = shapes.entries.iterator().next()
            oldest.value.bitmap.recycle()
            shapes.remove(oldest.key)
        }
    }

    private fun bitmapFromPremultipliedRgba(shape: CursorShapeData): Bitmap? {
        if (shape.width <= 0 || shape.height <= 0 ||
            shape.premultipliedRgba.size != shape.width * shape.height * 4
        ) {
            return null
        }

        // copyPixelsFromBuffer is raw. Feed Android's native ARGB_8888 byte
        // order and mark it premultiplied, avoiding setPixels/createBitmap's
        // second premultiplication of the wire's already-premultiplied RGB.
        val pixels = ByteBuffer.allocateDirect(shape.premultipliedRgba.size)
        val rgba = shape.premultipliedRgba
        val littleEndian = ByteOrder.nativeOrder() == ByteOrder.LITTLE_ENDIAN
        var offset = 0
        while (offset < rgba.size) {
            val red = rgba[offset]
            val green = rgba[offset + 1]
            val blue = rgba[offset + 2]
            val alpha = rgba[offset + 3]
            if (littleEndian) {
                pixels.put(blue).put(green).put(red).put(alpha)
            } else {
                pixels.put(alpha).put(red).put(green).put(blue)
            }
            offset += 4
        }
        pixels.rewind()
        return Bitmap.createBitmap(shape.width, shape.height, Bitmap.Config.ARGB_8888).apply {
            setPremultiplied(true)
            copyPixelsFromBuffer(pixels)
        }
    }

    private fun drawFallbackArrow(canvas: Canvas, tipX: Float, tipY: Float) {
        val density = resources.displayMetrics.density
        fallbackPath.reset()
        fallbackPath.moveTo(tipX, tipY)
        fallbackPath.lineTo(tipX, tipY + 16f * density)
        fallbackPath.lineTo(tipX + 4.5f * density, tipY + 12f * density)
        fallbackPath.lineTo(tipX + 8f * density, tipY + 20f * density)
        fallbackPath.lineTo(tipX + 11f * density, tipY + 18.5f * density)
        fallbackPath.lineTo(tipX + 7.5f * density, tipY + 10.5f * density)
        fallbackPath.lineTo(tipX + 13f * density, tipY + 10f * density)
        fallbackPath.close()
        canvas.drawPath(fallbackPath, fallbackFill)
        canvas.drawPath(fallbackPath, fallbackStroke)
    }

    private companion object {
        const val MAX_SHAPES = 8
        const val NORMALIZED_MAX = 65_535
        const val PREDICTION_TTL_MS = 120L
    }
}
