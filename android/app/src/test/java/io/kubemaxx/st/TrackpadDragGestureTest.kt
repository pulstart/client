package io.kubemaxx.st

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class TrackpadDragGestureTest {
    @Test
    fun secondTapWithinTimeoutAndSlopStartsDrag() {
        val gesture = TrackpadDragGesture(doubleTapTimeoutMs = 300)

        assertFalse(gesture.pointerDown(0, 10f, 20f, 30f))
        assertFalse(gesture.tapUp(50, 10f, 20f))
        assertTrue(gesture.pointerDown(250, 20f, 20f, 30f))
        assertTrue(gesture.dragging)
        assertTrue(gesture.tapUp(500, 40f, 20f))
        assertFalse(gesture.dragging)
    }

    @Test
    fun timeoutOrDistanceStartsANewTapSequence() {
        val gesture = TrackpadDragGesture(doubleTapTimeoutMs = 300)

        gesture.pointerDown(0, 0f, 0f, 20f)
        gesture.tapUp(10, 0f, 0f)
        assertFalse(gesture.pointerDown(311, 0f, 0f, 20f))
        gesture.tapUp(320, 0f, 0f)
        assertFalse(gesture.pointerDown(400, 21f, 0f, 20f))
    }

    @Test
    fun cancellationEndsDragAndClearsTapHistory() {
        val gesture = TrackpadDragGesture(doubleTapTimeoutMs = 300)
        gesture.pointerDown(0, 0f, 0f, 20f)
        gesture.tapUp(10, 0f, 0f)
        gesture.pointerDown(20, 0f, 0f, 20f)

        assertTrue(gesture.cancel())
        assertFalse(gesture.dragging)
        assertFalse(gesture.pointerDown(30, 0f, 0f, 20f))
    }
}
