package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

class OutputSnapshotTest {
    private fun le(size: Int, write: ByteBuffer.() -> Unit): ByteArray =
        ByteBuffer.allocate(size).order(ByteOrder.LITTLE_ENDIAN).apply(write).array()

    private fun outputEntry(
        id: Long,
        name: String,
        width: Int,
        height: Int,
        x: Int,
        y: Int,
        refresh: Int,
        rotation: Int,
        flags: Int,
    ): ByteArray {
        val nameBytes = name.toByteArray(Charsets.UTF_8)
        return le(28 + nameBytes.size) {
            putInt(id.toInt())
            putInt(width)
            putInt(height)
            putInt(x)
            putInt(y)
            putInt(refresh)
            put(rotation.toByte())
            put(flags.toByte())
            putShort(nameBytes.size.toShort())
            put(nameBytes)
        }
    }

    private fun snapshot(vararg tail: ByteArray, present: Int = 1 shl 4): ByteArray {
        val out = ByteArrayOutputStream()
        out.write(le(6) {
            put(3)
            put(present.toByte())
            putInt(4)
        })
        tail.forEach(out::write)
        return out.toByteArray()
    }

    private fun outputsSection(count: Int, virtualSupported: Boolean, revision: Long = 7): ByteArray =
        le(11) {
            putLong(revision)
            put(if (virtualSupported) 1 else 0)
            putShort(count.toShort())
        }

    @Test
    fun parsesOutputsSectionWithUtf8NamesAndFlags() {
        val bytes = snapshot(
            outputsSection(2, virtualSupported = true),
            outputEntry(0xA1B2C3D4L, "DP-1", 2560, 1440, -1920, 12, 143_856, 1, 0b01),
            outputEntry(2, "Virtuell ü", 1280, 720, 0, 0, 0, 0, 0b10),
        )

        val update = CursorSnapshotUpdate.from(bytes)

        assertNotNull(update)
        assertEquals(4, update!!.transportGeneration)
        val outputs = update.outputs!!
        assertEquals(7L, outputs.revision)
        assertTrue(outputs.value.virtualSupported)
        assertEquals(
            listOf(
                ServerOutput(0xA1B2C3D4L, "DP-1", 2560, 1440, -1920, 12, 143_856, 1, primary = true, virtual = false),
                ServerOutput(2, "Virtuell ü", 1280, 720, 0, 0, 0, 0, primary = false, virtual = true),
            ),
            outputs.value.outputs,
        )
    }

    @Test
    fun emptyOutputListIsStillAnAnnouncement() {
        val update = CursorSnapshotUpdate.from(snapshot(outputsSection(0, virtualSupported = false)))

        assertEquals(ServerOutputs.NONE, update!!.outputs!!.value)
    }

    @Test
    fun snapshotWithoutOutputsLeavesThemUntouched() {
        val update = CursorSnapshotUpdate.from(snapshot(present = 0))

        assertNull(update!!.outputs)
    }

    @Test
    fun truncatedOrOverlongOutputSectionIsRejected() {
        val entry = outputEntry(1, "DP-1", 1920, 1080, 0, 0, 60_000, 0, 0)
        assertNull(CursorSnapshotUpdate.from(snapshot(outputsSection(2, false), entry)))
        assertNull(CursorSnapshotUpdate.from(snapshot(outputsSection(1, false), entry.copyOf(entry.size - 1))))
        assertNull(CursorSnapshotUpdate.from(snapshot(outputsSection(1, false), entry, byteArrayOf(0))))
        assertNull(CursorSnapshotUpdate.from(snapshot(present = 1 shl 5)))
    }

    @Test
    fun olderSnapshotVersionIsRejected() {
        val bytes = snapshot(outputsSection(0, false))
        bytes[0] = 2
        assertNull(CursorSnapshotUpdate.from(bytes))
    }

    private val streamValues = intArrayOf(4, 5, 1, 2, 1920, 1080, 1080, 1920, 60, 48_000, 2, 20, -2, 3, 1)

    @Test
    fun streamDescriptionCarriesOutputRotationAndTransport() {
        val stream = StreamDescription.from(streamValues)!!

        assertEquals(0xFFFF_FFFEL, stream.outputId)
        assertEquals(3, stream.rotation)
        assertTrue(stream.directTransport)
        assertEquals(1080 to 1920, stream.uprightWidth to stream.uprightHeight)
        assertEquals(1920 to 1080, stream.width to stream.height)
        assertEquals(1L shl 32 or 2L, stream.videoEpoch)
    }

    @Test
    fun onlyDirectUdpCountsAsDirect() {
        for (code in 2..4) {
            assertFalse(StreamDescription.from(streamValues.copyOf().also { it[14] = code })!!.directTransport)
        }
    }

    @Test
    fun malformedStreamDescriptionsAreRejected() {
        assertNull(StreamDescription.from(streamValues.copyOf(12)))
        assertNull(StreamDescription.from(streamValues.copyOf().also { it[13] = 4 }))
        assertNull(StreamDescription.from(streamValues.copyOf().also { it[13] = -1 }))
        assertNull(StreamDescription.from(streamValues.copyOf().also { it[6] = 0 }))
    }

    private fun stream(
        generation: Int = 1,
        videoEpoch: Long = 1,
        outputId: Long = 1,
        rotation: Int = 0,
        width: Int = 1920,
        height: Int = 1080,
    ) = StreamDescription(
        1, generation, videoEpoch, width, height,
        uprightSize(width, height, rotation).first,
        uprightSize(width, height, rotation).second,
        60, 48_000, 2, 20,
        outputId = outputId,
        rotation = rotation,
    )

    @Test
    fun rotationChangeRestartsTheDecoderAtTheSameEncodedSize() {
        val plan = planStreamUpdate(stream(), stream(generation = 2, rotation = 1), decoderReady = true)

        assertTrue(plan.restartDecoder)
        assertFalse(plan.restartAudio)
    }

    @Test
    fun outputSwitchAtTheSameGeometryKeepsTheDecoder() {
        val plan = planStreamUpdate(
            stream(),
            stream(videoEpoch = 2, outputId = 9),
            decoderReady = true,
        )

        assertFalse(plan.restartDecoder)
        assertFalse(plan.restartAudio)
    }

    @Test
    fun outputSwitchToAnotherSizeRestartsTheDecoder() {
        val plan = planStreamUpdate(
            stream(),
            stream(generation = 2, videoEpoch = 2, outputId = 9, width = 2560, height = 1440),
            decoderReady = true,
        )

        assertTrue(plan.restartDecoder)
    }
}
