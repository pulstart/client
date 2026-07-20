package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class TrackpadScrollTest {
    @Test
    fun twentyFourDpProducesOneWheelStepWithNaturalSigns() {
        val accumulator = WheelAccumulator()

        assertEquals(WheelDelta(120, 120), accumulator.addPixels(48f, 48f, 2f))
        assertEquals(WheelDelta(-120, -120), accumulator.addPixels(-48f, -48f, 2f))
    }

    @Test
    fun retainsPositiveAndNegativeFractionalUnits() {
        val accumulator = WheelAccumulator()

        assertNull(accumulator.addPixels(0.1f, -0.1f, 1f))
        assertEquals(WheelDelta(1, -1), accumulator.addPixels(0.1f, -0.1f, 1f))
    }

    @Test
    fun resetDropsFractionalMovement() {
        val accumulator = WheelAccumulator()

        assertNull(accumulator.addPixels(0.1f, 0f, 1f))
        accumulator.reset()
        assertNull(accumulator.addPixels(0.1f, 0f, 1f))
    }

    @Test
    fun opposingFingerMotionStillCrossesGestureSlop() {
        val tracker = PointerTravelTracker()
        tracker.pointerDown(1, 10f, 10f)
        tracker.pointerDown(2, 30f, 10f)

        tracker.pointerMoved(1, 4f, 10f)
        assertEquals(6f, tracker.pointerMoved(2, 36f, 10f))
    }

    @Test
    fun pointerTravelResetStartsAStationaryGestureAgain() {
        val tracker = PointerTravelTracker()
        tracker.pointerDown(1, 0f, 0f)
        assertEquals(10f, tracker.pointerMoved(1, 10f, 0f))

        tracker.reset()
        tracker.pointerDown(1, 10f, 0f)
        assertEquals(0f, tracker.pointerMoved(1, 10f, 0f))
    }

    @Test
    fun pointerUpRecordsTravelNotDeliveredAsMove() {
        val tracker = PointerTravelTracker()
        tracker.pointerDown(1, 10f, 10f)

        assertEquals(8f, tracker.pointerUp(1, 18f, 10f))
    }
}
