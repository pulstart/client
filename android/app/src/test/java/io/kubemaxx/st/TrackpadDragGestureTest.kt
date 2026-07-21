package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class TrackpadDragGestureTest {
    @Test
    fun longPressActivatesOnceAndUpReleasesWithoutAnotherClick() {
        val gesture = TrackpadDragGesture(doubleTapTimeoutMs = 300)
        val down = gesture.pointerDown(0, 10f, 20f, 30f)
        val token = requireNotNull(down.longPressToken)

        assertEquals(DragTransition.NONE, down.transition)
        assertEquals(DragTransition.PRESS, gesture.activateLongPress(token))
        assertTrue(gesture.dragging)
        assertEquals(DragTransition.NONE, gesture.activateLongPress(token))
        assertEquals(DragTransition.RELEASE, gesture.pointerUp(600, 10f, 20f, rememberTap = false))
        assertFalse(gesture.dragging)
        assertEquals(DragTransition.NONE, gesture.pointerUp(601, 10f, 20f, rememberTap = true))
    }

    @Test
    fun movementPastSystemSlopCancelsPendingLongPress() {
        val gesture = TrackpadDragGesture(doubleTapTimeoutMs = 300)
        val down = gesture.pointerDown(0, 0f, 0f, 20f)
        val token = requireNotNull(down.longPressToken)

        assertFalse(gesture.pointerMoved(5f, 0f, 10f))
        assertTrue(gesture.pointerMoved(11f, 0f, 10f))
        assertEquals(DragTransition.NONE, gesture.activateLongPress(token))
        assertEquals(DragTransition.NONE, gesture.pointerUp(100, 11f, 0f, rememberTap = false))
    }

    @Test
    fun upAndCancelInvalidateDelayedActivationAndReleaseActiveDrag() {
        val gesture = TrackpadDragGesture(doubleTapTimeoutMs = 300)
        val upToken = requireNotNull(gesture.pointerDown(0, 0f, 0f, 20f).longPressToken)
        assertEquals(DragTransition.NONE, gesture.pointerUp(10, 0f, 0f, rememberTap = true))
        assertEquals(DragTransition.NONE, gesture.activateLongPress(upToken))

        val cancelToken = requireNotNull(gesture.pointerDown(1_000, 0f, 0f, 20f).longPressToken)
        assertEquals(DragTransition.PRESS, gesture.activateLongPress(cancelToken))
        assertEquals(DragTransition.RELEASE, gesture.cancel())
        assertEquals(DragTransition.NONE, gesture.cancel())
        assertEquals(DragTransition.NONE, gesture.activateLongPress(cancelToken))
    }

    @Test
    fun doubleTapDragCoexistsWithLongPressAndHasNoPendingToken() {
        val gesture = TrackpadDragGesture(doubleTapTimeoutMs = 300)
        gesture.pointerDown(0, 10f, 20f, 30f)
        assertEquals(
            DragTransition.NONE,
            gesture.pointerUp(50, 10f, 20f, rememberTap = true),
        )

        val secondDown = gesture.pointerDown(250, 20f, 20f, 30f)
        assertEquals(DragTransition.PRESS, secondDown.transition)
        assertNull(secondDown.longPressToken)
        assertEquals(
            DragTransition.RELEASE,
            gesture.pointerUp(500, 40f, 20f, rememberTap = false),
        )
    }

    @Test
    fun timeoutOrDistanceStartsANewLongPressCandidate() {
        val gesture = TrackpadDragGesture(doubleTapTimeoutMs = 300)
        gesture.pointerDown(0, 0f, 0f, 20f)
        gesture.pointerUp(10, 0f, 0f, rememberTap = true)

        assertEquals(
            DragTransition.NONE,
            gesture.pointerDown(311, 0f, 0f, 20f).transition,
        )
        gesture.pointerUp(320, 0f, 0f, rememberTap = true)
        assertEquals(
            DragTransition.NONE,
            gesture.pointerDown(400, 21f, 0f, 20f).transition,
        )
    }
}
