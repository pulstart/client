package io.kubemaxx.st

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class StreamStabilityTest {
    private fun stream(
        transportGeneration: Int = 1,
        generation: Int = 1,
        width: Int = 1920,
        height: Int = 1080,
        framerate: Int = 60,
        sampleRate: Int = 48_000,
        channels: Int = 2,
        packetDurationMs: Int = 20,
    ) = StreamDescription(
        transportGeneration,
        generation,
        width,
        height,
        width,
        height,
        framerate,
        sampleRate,
        channels,
        packetDurationMs,
    )

    @Test
    fun fpsOnlyUpdateKeepsDecoderAndAudio() {
        val plan = planStreamUpdate(stream(), stream(framerate = 90), decoderReady = true)

        assertFalse(plan.restartDecoder)
        assertFalse(plan.restartAudio)
        assertFalse(plan.resetAudioDecoder)
    }

    @Test
    fun videoAndAudioCompatibilityAreIndependent() {
        val initial = stream()
        val audioChange = planStreamUpdate(
            initial,
            stream(packetDurationMs = 10),
            decoderReady = true,
        )
        val resize = planStreamUpdate(
            initial,
            stream(generation = 2, width = 1280, height = 720),
            decoderReady = true,
        )

        assertFalse(audioChange.restartDecoder)
        assertTrue(audioChange.restartAudio)
        assertTrue(resize.restartDecoder)
        assertFalse(resize.restartAudio)
        assertFalse(resize.resetAudioDecoder)
        assertTrue(planStreamUpdate(initial, initial, decoderReady = false).restartDecoder)
    }

    @Test
    fun transportChangeResetsAudioDecoderWithoutRestartingCompatibleTrack() {
        val plan = planStreamUpdate(
            stream(),
            stream(transportGeneration = 2, generation = 2),
            decoderReady = true,
        )

        assertFalse(plan.restartAudio)
        assertTrue(plan.resetAudioDecoder)
    }
}
