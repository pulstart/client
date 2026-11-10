package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class VideoGeometryTest {
    @Test
    fun quarterTurnsSwapTheUprightAxes() {
        assertEquals(1920 to 1080, uprightSize(1920, 1080, 0))
        assertEquals(1080 to 1920, uprightSize(1920, 1080, 1))
        assertEquals(1920 to 1080, uprightSize(1920, 1080, 2))
        assertEquals(1080 to 1920, uprightSize(1920, 1080, 3))
    }

    @Test
    fun rotationMapsToMediaCodecDegrees() {
        assertEquals(listOf(0, 90, 180, 270), (0..3).map(::rotationDegrees))
        assertEquals(90, rotationDegrees(5))
    }

    @Test
    fun rotatedStreamIsFittedByItsUprightAspect() {
        val (w, h) = uprightSize(1920, 1080, 1)

        assertEquals(608 to 1080, measureVideoView(1920, 1080, w, h, VideoScaleMode.FIT))
        assertEquals(1920 to 3413, measureVideoView(1920, 1080, w, h, VideoScaleMode.COVER))
        assertEquals(1920 to 1080, measureVideoView(1920, 1080, w, h, VideoScaleMode.STRETCH))
        assertEquals(
            "unrotated stream still fills the landscape area",
            1920 to 1080,
            measureVideoView(1920, 1080, 1920, 1080, VideoScaleMode.FIT),
        )
    }

    @Test
    fun measuringWithoutAPictureFillsTheArea() {
        assertEquals(800 to 600, measureVideoView(800, 600, 0, 0, VideoScaleMode.FIT))
        assertEquals(0 to 0, measureVideoView(0, 0, 1920, 1080, VideoScaleMode.FIT))
    }

    @Test
    fun cursorPositionsMapThroughTheUprightSpace() {
        val (w, h) = uprightSize(1920, 1080, 1)

        assertEquals(0f to 0f, mapUprightToView(0f, 0f, w, h, 608f, 1080f))
        assertEquals(304f to 540f, mapUprightToView(540f, 960f, w, h, 608f, 1080f))
        val (x, y) = mapUprightToView(1080f, 1920f, w, h, 608f, 1080f)
        assertEquals(608f, x, 0.001f)
        assertEquals(1080f, y, 0.001f)
    }

    @Test
    fun uprightCursorSpaceMatchesTheVideoSpaceForEveryRotation() {
        for (rotation in 0..3) {
            val stream = StreamDescription(
                1, 1, 1, 1920, 1080,
                uprightSize(1920, 1080, rotation).first,
                uprightSize(1920, 1080, rotation).second,
                60, 48_000, 2, 20,
                rotation = rotation,
            )
            assertEquals(stream.uprightWidth, stream.cursorWidth)
            assertEquals(stream.uprightHeight, stream.cursorHeight)
            assertTrue(stream.uprightWidth * stream.uprightHeight == 1920 * 1080)
        }
    }
}
