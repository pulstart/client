package io.kubemaxx.st

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class MediaBufferingTest {
    @Test
    fun accessUnitWithoutParameterSetsReusesOriginalArray() {
        val accessUnit = byteArrayOf(0, 0, 0, 1, 0x65, 1, 2, 3)

        assertSame(accessUnit, stripAvcParameterSets(accessUnit, scanAvcNals(accessUnit)))
    }

    @Test
    fun parameterSetRemovalRetainsOnlyPictureNals() {
        val accessUnit = byteArrayOf(
            0, 0, 0, 1, 0x67, 1,
            0, 0, 0, 1, 0x68, 2,
            0, 0, 0, 1, 0x65, 3, 4,
        )

        assertArrayEquals(
            byteArrayOf(0, 0, 0, 1, 0x65, 3, 4),
            stripAvcParameterSets(accessUnit, scanAvcNals(accessUnit)),
        )
    }

    @Test
    fun audioPrimesTwentyFiveMsAndDetectsUnderrunAdvance() {
        assertTrue(audioPrebufferTargetBytes(48_000, 2, 2) == 4_800)
        assertFalse(audioUnderrunAdvanced(3, 3))
        assertTrue(audioUnderrunAdvanced(3, 4))
    }

    @Test
    fun underrunRebuffersWithoutDiscardingCompressedPackets() {
        val plan = planAudioUnderrun(7, 8)

        assertTrue(plan.rebuffer)
        assertTrue(plan.retainCompressedPackets)
        assertFalse(planAudioUnderrun(8, 8).rebuffer)
    }

    @Test
    fun adaptiveAudioPrebufferStaysWithinTwentyFiveToSixtyMs() {
        val prebuffer = AdaptiveAudioPrebuffer()
        repeat(20) { prebuffer.onUnderrun() }
        assertEquals(60, prebuffer.targetMs)

        prebuffer.onStablePlayback(35_000)
        assertEquals(25, prebuffer.targetMs)
        prebuffer.onStablePlayback(100_000)
        assertEquals(25, prebuffer.targetMs)
    }

    @Test
    fun packedAudioCountersExposeOccupancyAndLocalDrops() {
        val packed = (17L shl 32) or 3L

        assertEquals(3L, audioQueueOccupancy(packed))
        assertEquals(17L, audioLocalDropCount(packed))
    }
}
