package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

class ResourceLifecycleTest {
    @Test
    fun audioStopSignalsWithoutWaitingForBlockedWorker() {
        val running = AtomicBoolean(true)
        val entered = CountDownLatch(1)
        val release = CountDownLatch(1)
        val worker = Thread {
            entered.countDown()
            while (release.count > 0) {
                try {
                    release.await()
                } catch (_: InterruptedException) {
                    // Simulate a platform write that does not unblock on interrupt.
                }
            }
        }.apply { start() }
        try {
            assertTrue(entered.await(1, TimeUnit.SECONDS))

            val startedAt = System.nanoTime()
            requestAudioWorkerStop(running, worker)
            val elapsedMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - startedAt)

            assertTrue("stop took ${elapsedMs}ms", elapsedMs < 100)
            assertFalse(running.get())
            assertTrue(worker.isAlive)
        } finally {
            release.countDown()
            worker.join(1_000)
        }
        assertFalse(worker.isAlive)
    }

    @Test
    fun audioRuntimeTerminationClearsSlotForOneRetryAndOneRelease() {
        val slot = AudioPlayerSlot<Any>()
        val player = Any()
        slot.started(player, 7)

        assertTrue(slot.terminated(player))
        assertFalse(slot.terminated(player))
        assertTrue(slot.canStart(7))

        val replacement = Any()
        slot.started(replacement, 7)
        assertSame(replacement, slot.retireActive())
        assertFalse(slot.canStart(8))
        assertTrue(slot.terminated(replacement))
        assertTrue(slot.canStart(8))
    }

    @Test
    fun invalidAudioTrackIsReleasedBeforeValidationFailureEscapes() {
        val track = Any()
        var releases = 0

        val error = assertThrows(IllegalStateException::class.java) {
            buildValidatedAudioTrack(
                build = { track },
                isInitialized = { false },
                release = { releases++ },
            )
        }

        assertEquals("AudioTrack failed to initialize", error.message)
        assertEquals(1, releases)
    }

    @Test
    fun audioTrackIsReleasedWhenStateInspectionThrows() {
        val failure = IllegalArgumentException("state unavailable")
        var releases = 0

        val thrown = assertThrows(IllegalArgumentException::class.java) {
            buildValidatedAudioTrack(
                build = { Any() },
                isInitialized = { throw failure },
                release = { releases++ },
            )
        }

        assertSame(failure, thrown)
        assertEquals(1, releases)
    }

    @Test
    fun codecConfigurationFailureReleasesWithoutStopping() {
        val events = mutableListOf<String>()
        val failure = IllegalStateException("configure failed")

        val thrown = assertThrows(IllegalStateException::class.java) {
            createConfiguredCodec(
                create = {
                    events += "create"
                    Any()
                },
                configure = {
                    events += "configure"
                    throw failure
                },
                start = { events += "start" },
                stop = { events += "stop" },
                release = { events += "release" },
            )
        }

        assertSame(failure, thrown)
        assertEquals(listOf("create", "configure", "release"), events)
    }

    @Test
    fun codecStartFailureReleasesWithoutDoubleCleanup() {
        val events = mutableListOf<String>()
        val failure = IllegalStateException("start failed")

        val thrown = assertThrows(IllegalStateException::class.java) {
            createConfiguredCodec(
                create = { Any() },
                configure = { events += "configure" },
                start = {
                    events += "start"
                    throw failure
                },
                stop = { events += "stop" },
                release = { events += "release" },
            )
        }

        assertSame(failure, thrown)
        assertEquals(listOf("configure", "start", "release"), events)
    }

    @Test
    fun successfulCodecTransfersCleanupToWorkerOwner() {
        val events = mutableListOf<String>()
        val codec = Any()

        val created = createConfiguredCodec(
            create = { codec },
            configure = { events += "configure" },
            start = { events += "start" },
            stop = { events += "unexpected stop" },
            release = { events += "unexpected release" },
        )
        assertSame(codec, created)
        assertEquals(listOf("configure", "start"), events)

        releaseCodecResource(
            codec = created,
            started = true,
            stop = { events += "stop" },
            release = { events += "release" },
        )
        assertEquals(listOf("configure", "start", "stop", "release"), events)
    }
}
