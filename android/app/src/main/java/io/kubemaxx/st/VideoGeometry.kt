package io.kubemaxx.st

import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

internal enum class VideoScaleMode(val label: String) {
    FIT("Fit"),
    COVER("Fill / Cover"),
    STRETCH("Stretch"),
}

internal fun uprightSize(width: Int, height: Int, rotation: Int): Pair<Int, Int> =
    if (rotation and 1 == 1) height to width else width to height

internal fun rotationDegrees(rotation: Int): Int = (rotation and 3) * 90

/** Size of the video view inside `availableWidth x availableHeight` for an upright picture. */
internal fun measureVideoView(
    availableWidth: Int,
    availableHeight: Int,
    videoWidth: Int,
    videoHeight: Int,
    mode: VideoScaleMode,
): Pair<Int, Int> {
    if (videoWidth <= 0 || videoHeight <= 0 || availableWidth <= 0 || availableHeight <= 0 ||
        mode == VideoScaleMode.STRETCH
    ) {
        return availableWidth to availableHeight
    }
    val widthScale = availableWidth.toDouble() / videoWidth
    val heightScale = availableHeight.toDouble() / videoHeight
    val scale = if (mode == VideoScaleMode.COVER) max(widthScale, heightScale) else min(widthScale, heightScale)
    return (videoWidth * scale).roundToInt().coerceAtLeast(1) to
        (videoHeight * scale).roundToInt().coerceAtLeast(1)
}

/** Maps a point in upright cursor space onto the displayed video rectangle. */
internal fun mapUprightToView(
    x: Float,
    y: Float,
    uprightWidth: Int,
    uprightHeight: Int,
    viewWidth: Float,
    viewHeight: Float,
): Pair<Float, Float> = x * viewWidth / uprightWidth to y * viewHeight / uprightHeight
