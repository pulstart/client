package io.kubemaxx.st

import org.junit.Assert.assertArrayEquals
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
}
