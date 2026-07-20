package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Test

class ResourceLifecycleTest {
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
