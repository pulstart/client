package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class OutputChoiceTest {
    private fun output(
        id: Long,
        name: String = "DP-$id",
        width: Int = 2560,
        height: Int = 1440,
        primary: Boolean = false,
        virtual: Boolean = false,
    ) = ServerOutput(
        id = id,
        name = name,
        width = width,
        height = height,
        x = 0,
        y = 0,
        refreshMilliHz = 60_000,
        rotation = 0,
        primary = primary,
        virtual = virtual,
    )

    private fun outputs(vararg list: ServerOutput, virtualSupported: Boolean = false) =
        ServerOutputs(list.toList(), virtualSupported)

    private fun stream(direct: Boolean = true, outputId: Long = 1) = StreamDescription(
        1, 1, 1, 1920, 1080, 1920, 1080, 60, 48_000, 2, 20,
        outputId = outputId,
        directTransport = direct,
    )

    @Test
    fun pickerShowsNameSizeAndMarkers() {
        assertEquals("DP-1 · 2560×1440 · primary", output(1, primary = true).label())
        assertEquals(
            "Headless · 1280×720 · virtual",
            output(2, "Headless", 1280, 720, virtual = true).label(),
        )
        assertEquals("Output 7 · 2560×1440", output(7, name = " ").label())
    }

    @Test
    fun pickerHidesWithOneOrFewerOutputs() {
        assertFalse(shouldShowOutputPicker(emptyList()))
        assertFalse(shouldShowOutputPicker(listOf(output(1))))
        assertTrue(shouldShowOutputPicker(listOf(output(1), output(2))))
    }

    @Test
    fun highlightedEntryFollowsTheStreamedOutput() {
        val list = listOf(output(10), output(20), output(30))
        assertEquals(1, outputPickerIndex(list, 20))
        assertEquals(-1, outputPickerIndex(list, 99))
        assertEquals(-1, outputPickerIndex(list, null))
        assertEquals(2, outputPickerIndex(list, 30))
    }

    @Test
    fun rememberedOutputsRoundTripPerServer() {
        val choices = mapOf(
            "peer-a" to RememberedOutput(0xA1B2C3D4L, "DP-1"),
            "192.168.1.5:28480" to RememberedOutput(2, "HDMI-A-1"),
        )

        assertEquals(choices, parseOutputChoices(serializeOutputChoices(choices)))
    }

    @Test
    fun unreadableOrReservedChoicesAreDropped() {
        assertEquals(emptyMap<String, RememberedOutput>(), parseOutputChoices(null))
        assertEquals(emptyMap<String, RememberedOutput>(), parseOutputChoices("not json"))
        assertEquals(
            mapOf("good" to RememberedOutput(5, "x")),
            parseOutputChoices(
                """{"good":{"id":5,"name":"x"},"zero":{"id":0,"name":"d"},"big":{"id":4294967296},"str":"x"}""",
            ),
        )
    }

    @Test
    fun noStoredChoiceConnectsToTheServerDefault() {
        assertEquals(OUTPUT_DEFAULT, connectOutputId(null))
        assertEquals(77L, connectOutputId(RememberedOutput(77, "DP-2")))
    }

    @Test
    fun onlyAServerConfirmedPickIsRemembered() {
        val list = listOf(output(1, "DP-1"), output(2, "HDMI-A-1"))

        assertEquals(RememberedOutput(2, "HDMI-A-1"), outputChoiceToRemember(2, 2, list))
        assertNull("refused switch: server kept streaming the old output", outputChoiceToRemember(2, 1, list))
        assertNull("server fallback without a user pick", outputChoiceToRemember(null, 1, list))
        assertEquals(RememberedOutput(9, ""), outputChoiceToRemember(9, 9, list))
    }

    @Test
    fun auxPrefersTheRememberedOutputUnlessMainShowsItOrItIsGone() {
        val list = outputs(output(1), output(2), output(3))

        assertEquals(AuxTarget.Existing(3), chooseAuxTarget(list, 1, RememberedOutput(3, "")))
        assertEquals(AuxTarget.Existing(2), chooseAuxTarget(list, 1, RememberedOutput(1, "main shows it")))
        assertEquals(AuxTarget.Existing(2), chooseAuxTarget(list, 1, RememberedOutput(42, "unplugged")))
        assertEquals(AuxTarget.Existing(2), chooseAuxTarget(list, 1, null))
        assertEquals(AuxTarget.Existing(1), chooseAuxTarget(list, 2, null))
    }

    @Test
    fun auxFallsBackToAVirtualOutputOnlyWhenNothingElseExists() {
        val single = outputs(output(1), virtualSupported = true)
        assertEquals(AuxTarget.Virtual, chooseAuxTarget(single, 1, null))
        assertEquals(AuxTarget.Virtual, chooseAuxTarget(outputs(virtualSupported = true), 0, null))
        assertNull(chooseAuxTarget(outputs(output(1)), 1, null))
        assertNull(chooseAuxTarget(ServerOutputs.NONE, 0, null))

        val two = outputs(output(1), output(2), virtualSupported = true)
        assertEquals(AuxTarget.Existing(2), chooseAuxTarget(two, 1, null))
        assertEquals(AuxTarget.Virtual, chooseAuxTarget(two, 1, RememberedOutput(OUTPUT_VIRTUAL, "virtual")))
        assertEquals(
            "virtual choice is ignored when the server cannot create one",
            AuxTarget.Existing(2),
            chooseAuxTarget(outputs(output(1), output(2)), 1, RememberedOutput(OUTPUT_VIRTUAL, "virtual")),
        )
    }

    @Test
    fun auxTargetsMapToConnectOutputIds() {
        assertEquals(5L, auxTargetOutputId(AuxTarget.Existing(5)))
        assertEquals(OUTPUT_VIRTUAL, auxTargetOutputId(AuxTarget.Virtual))
        assertEquals(0xFFFF_FFFFL, OUTPUT_VIRTUAL)
    }

    @Test
    fun auxNeedsDisplayConnectedMainAndADirectTransport() {
        assertTrue(auxSessionWanted(true, true, stream()))
        assertFalse(auxSessionWanted(false, true, stream()))
        assertFalse(auxSessionWanted(true, false, stream()))
        assertFalse(auxSessionWanted(true, true, null))
        assertFalse("punched, relayed and TCP-tunnelled mains get no aux", auxSessionWanted(true, true, stream(direct = false)))
    }

    @Test
    fun auxPickerOffersVirtualUnlessAlreadyOnOne() {
        val list = outputs(output(1), output(2), virtualSupported = true)
        assertEquals(
            listOf(AuxTarget.Existing(1), AuxTarget.Existing(2), AuxTarget.Virtual),
            auxPickerEntries(list, 1).map(AuxPickerEntry::target),
        )

        val onVirtual = outputs(output(1), output(8, virtual = true), virtualSupported = true)
        assertEquals(
            listOf(AuxTarget.Existing(1), AuxTarget.Existing(8)),
            auxPickerEntries(onVirtual, 8).map(AuxPickerEntry::target),
        )
        assertEquals(2, auxPickerEntries(outputs(output(1), output(2)), 1).size)
    }

    @Test
    fun auxVirtualPickIsRememberedOnceTheServerStreamsAVirtualOutput() {
        val list = listOf(output(1), output(8, virtual = true))

        assertEquals(RememberedOutput(OUTPUT_VIRTUAL, "virtual"), auxChoiceToRemember(OUTPUT_VIRTUAL, 8, list))
        assertNull(auxChoiceToRemember(OUTPUT_VIRTUAL, 1, list))
        assertNull(auxChoiceToRemember(null, 8, list))
        assertEquals(RememberedOutput(1, "DP-1"), auxChoiceToRemember(1, 1, list))
    }
}
