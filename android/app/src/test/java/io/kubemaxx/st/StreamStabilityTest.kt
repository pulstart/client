package io.kubemaxx.st

import org.junit.Assert.assertFalse
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

class StreamStabilityTest {
    private fun stream(
        transportGeneration: Int = 1,
        generation: Int = 1,
        videoEpoch: Long = 1,
        width: Int = 1920,
        height: Int = 1080,
        framerate: Int = 60,
        sampleRate: Int = 48_000,
        channels: Int = 2,
        packetDurationMs: Int = 20,
    ) = StreamDescription(
        transportGeneration,
        generation,
        videoEpoch,
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

    @Test
    fun liveTransitionKeepsLastSurfaceFrameUnobscured() {
        assertFalse(
            shouldShowStartupStatus(
                streaming = true,
                hasRenderedVideo = true,
                connected = true,
                decoderStatus = "starting decoder",
            ),
        )
        assertTrue(
            shouldShowStartupStatus(
                streaming = true,
                hasRenderedVideo = false,
                connected = false,
                decoderStatus = "starting decoder",
            ),
        )
        assertTrue(
            shouldShowStartupStatus(
                streaming = true,
                hasRenderedVideo = true,
                connected = false,
                decoderStatus = "video active",
            ),
        )
        assertTrue(
            shouldShowStartupStatus(
                streaming = true,
                hasRenderedVideo = true,
                connected = true,
                decoderStatus = "decoder restart timed out; reconnect required",
            ),
        )
    }

    @Test
    fun decoderTransitionPollFitsOneDisplayFrame() {
        assertTrue(STREAM_STATUS_POLL_MS <= 16L)
    }

    @Test
    fun asyncTransitionsNeverBlockSubmitAndOnlyDeliverLatest() {
        val executor = Executors.newSingleThreadExecutor()
        val firstStarted = CountDownLatch(1)
        val releaseFirst = CountDownLatch(1)
        val completed = CountDownLatch(1)
        val active = AtomicInteger()
        val maximumActive = AtomicInteger()
        val delivered = mutableListOf<Int>()
        val transitions = LatestAsyncTask<Int, Int>(
            executor = executor,
            deliver = { it() },
            work = { value, _ ->
                val current = active.incrementAndGet()
                maximumActive.accumulateAndGet(current, ::maxOf)
                if (value == 1) {
                    firstStarted.countDown()
                    releaseFirst.await(1, TimeUnit.SECONDS)
                }
                active.decrementAndGet()
                value
            },
            complete = { _, result ->
                delivered += result
                completed.countDown()
            },
        )

        try {
            transitions.submit(1)
            assertTrue(firstStarted.await(1, TimeUnit.SECONDS))
            val startedAt = System.nanoTime()
            transitions.submit(2)
            val submitMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - startedAt)
            assertTrue("submit waited ${submitMs}ms", submitMs < 100)
            releaseFirst.countDown()
            assertTrue(completed.await(1, TimeUnit.SECONDS))
            assertEquals(listOf(2), delivered)
            assertEquals(1, maximumActive.get())
        } finally {
            releaseFirst.countDown()
            transitions.cancel()
            executor.shutdownNow()
        }
    }
}
