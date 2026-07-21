package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import android.view.KeyEvent
import java.nio.charset.StandardCharsets
import java.nio.ByteBuffer
import java.nio.ByteOrder

class RemoteKeyboardTest {
    @Test
    fun mapsAsciiToPhysicalUsKeyboardChords() {
        assertEquals(126, RemoteKey.entries.size)
        assertEquals((0..125).toList(), RemoteKey.entries.map(RemoteKey::wireId))
        assertEquals(81, RemoteKey.RightMeta.wireId)
        assertEquals(125, RemoteKey.IntlBackslash.wireId)
        assertEquals(RemoteKeyStroke(RemoteKey.A), RemoteKeyMap.fromCharacter('a'))
        assertEquals(RemoteKeyStroke(RemoteKey.A, true), RemoteKeyMap.fromCharacter('A'))
        assertEquals(RemoteKeyStroke(RemoteKey.Num1, true), RemoteKeyMap.fromCharacter('!'))
        assertEquals(RemoteKeyStroke(RemoteKey.Slash, true), RemoteKeyMap.fromCharacter('?'))
        assertEquals(RemoteKeyStroke(RemoteKey.Enter), RemoteKeyMap.fromCharacter('\n'))
        assertNull(RemoteKeyMap.fromCharacter('é'))
        assertEquals(RemoteKey.LeftCtrl, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_CTRL_LEFT))
        assertEquals(RemoteKey.ArrowUp, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_DPAD_UP))
        assertEquals(RemoteKey.Numpad4, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_NUMPAD_4))
        assertEquals(RemoteKey.NumpadAdd, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_NUMPAD_ADD))
        assertEquals(RemoteKey.NumpadEnter, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_NUMPAD_ENTER))
        assertEquals(RemoteKey.CapsLock, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_CAPS_LOCK))
        assertEquals(RemoteKey.PrintScreen, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_SYSRQ))
        assertEquals(RemoteKey.Application, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_MENU))
        assertEquals(RemoteKey.MediaNext, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_MEDIA_NEXT))
        assertEquals(RemoteKey.IntlBackslash, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_RO))
        assertEquals(RemoteKey.F13, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_UNKNOWN, 183))
        assertEquals(RemoteKey.F24, RemoteKeyMap.fromAndroidKeyCode(KeyEvent.KEYCODE_UNKNOWN, 194))
        assertNull(RemoteKeyMap.fromAndroidKeyStroke(KeyEvent.KEYCODE_NUMPAD_LEFT_PAREN))
    }

    @Test
    fun publishesCompleteOrderedSnapshotsAndRelease() {
        val snapshots = mutableListOf<ByteArray>()
        val state = RemoteKeyboardState { snapshot ->
            snapshots += snapshot
            true
        }

        state.setSynthetic(RemoteKey.LeftShift, true)
        state.setSynthetic(RemoteKey.A, true)
        state.setSynthetic(RemoteKey.A, false)
        state.setSynthetic(RemoteKey.LeftShift, false)

        assertEquals(4, snapshots.size)
        assertTrue(snapshots[0].isPressed(RemoteKey.LeftShift))
        assertFalse(snapshots[0].isPressed(RemoteKey.A))
        assertTrue(snapshots[1].isPressed(RemoteKey.LeftShift))
        assertTrue(snapshots[1].isPressed(RemoteKey.A))
        assertFalse(snapshots[2].isPressed(RemoteKey.A))
        assertTrue(snapshots[2].isPressed(RemoteKey.LeftShift))
        assertTrue(snapshots[3].all { it == 0.toByte() })
    }

    @Test
    fun syntheticReleaseDoesNotReleaseHeldHardwareModifier() {
        val snapshots = mutableListOf<ByteArray>()
        val state = RemoteKeyboardState { snapshot ->
            snapshots += snapshot
            true
        }

        state.setHardware(RemoteKey.LeftShift, true)
        state.setSynthetic(RemoteKey.LeftShift, true)
        state.setSynthetic(RemoteKey.LeftShift, false)

        assertEquals(1, snapshots.size)
        assertTrue(snapshots.single().isPressed(RemoteKey.LeftShift))
    }

    @Test
    fun expandedKeysUseExplicitWireBits() {
        val snapshots = mutableListOf<ByteArray>()
        val state = RemoteKeyboardState { snapshot ->
            snapshots += snapshot
            true
        }

        state.setHardware(RemoteKey.Numpad0, true)
        state.setHardware(RemoteKey.F24, true)
        state.setHardware(RemoteKey.IntlBackslash, true)

        assertTrue(snapshots.last().isPressed(RemoteKey.Numpad0))
        assertTrue(snapshots.last().isPressed(RemoteKey.F24))
        assertTrue(snapshots.last().isPressed(RemoteKey.IntlBackslash))
        assertEquals(0, snapshots.last()[15].toInt() and 0xc0)
    }

    @Test
    fun virtualNumpadUsesDifferentWireKeysFromTopRow() {
        val snapshots = mutableListOf<ByteArray>()
        val controller = VirtualKeyboardController(RemoteKeyboardState { snapshot ->
            snapshots += snapshot
            true
        })

        controller.press(RemoteKey.Num1)
        controller.release(RemoteKey.Num1)
        controller.press(RemoteKey.Numpad1)

        assertTrue(snapshots[0].isPressed(RemoteKey.Num1))
        assertFalse(snapshots[0].isPressed(RemoteKey.Numpad1))
        assertTrue(snapshots.last().isPressed(RemoteKey.Numpad1))
        assertFalse(snapshots.last().isPressed(RemoteKey.Num1))
        assertFalse(RemoteKey.Num1.wireId == RemoteKey.Numpad1.wireId)
    }

    @Test
    fun latchedModifiersPublishCtrlAltDeleteChord() {
        val snapshots = mutableListOf<ByteArray>()
        val controller = VirtualKeyboardController(RemoteKeyboardState { snapshot ->
            snapshots += snapshot
            true
        })

        controller.toggleLatch(RemoteKey.LeftCtrl)
        controller.toggleLatch(RemoteKey.LeftAlt)
        controller.press(RemoteKey.Delete)

        assertTrue(controller.isLatched(RemoteKey.LeftCtrl))
        assertTrue(controller.isLatched(RemoteKey.LeftAlt))
        assertTrue(snapshots.last().isPressed(RemoteKey.LeftCtrl))
        assertTrue(snapshots.last().isPressed(RemoteKey.LeftAlt))
        assertTrue(snapshots.last().isPressed(RemoteKey.Delete))

        controller.release(RemoteKey.Delete)
        assertTrue(snapshots.last().isPressed(RemoteKey.LeftCtrl))
        assertTrue(snapshots.last().isPressed(RemoteKey.LeftAlt))
        assertFalse(snapshots.last().isPressed(RemoteKey.Delete))
    }

    @Test
    fun failedVirtualKeyPublicationRemainsRetryable() {
        val attempts = mutableListOf<ByteArray>()
        var accept = false
        val controller = VirtualKeyboardController(RemoteKeyboardState { snapshot ->
            attempts += snapshot
            accept
        })

        controller.toggleLatch(RemoteKey.LeftCtrl)
        accept = true
        controller.retryPublication()

        assertEquals(2, attempts.size)
        assertTrue(attempts.all { it.isPressed(RemoteKey.LeftCtrl) })
    }

    @Test
    fun closingVirtualKeyboardReleasesHeldKeysAndLatches() {
        val snapshots = mutableListOf<ByteArray>()
        val controller = VirtualKeyboardController(RemoteKeyboardState { snapshot ->
            snapshots += snapshot
            true
        })
        controller.toggleLatch(RemoteKey.LeftCtrl)
        controller.press(RemoteKey.Delete)

        controller.releaseAll()

        assertFalse(controller.isLatched(RemoteKey.LeftCtrl))
        assertTrue(snapshots.last().all { it == 0.toByte() })
    }

    @Test
    fun accessibleVirtualClickHoldsThroughRepairWindowThenReleases() {
        val snapshots = mutableListOf<ByteArray>()
        val controller = VirtualKeyboardController(RemoteKeyboardState { snapshot ->
            snapshots += snapshot
            true
        })
        var release: (() -> Unit)? = null
        var delayMs = 0L

        performVirtualKeyClick(controller, RemoteKey.Delete) { delay, action ->
            delayMs = delay
            release = action
        }

        assertTrue(delayMs >= KEYBOARD_REPAIR_WINDOW_MS)
        assertTrue(snapshots.single().isPressed(RemoteKey.Delete))
        requireNotNull(release).invoke()
        assertFalse(snapshots.last().isPressed(RemoteKey.Delete))
    }

    @Test
    fun heldRepeatableVirtualKeyEmitsBalancedEdgesWithoutCountDrift() {
        val snapshots = mutableListOf<ByteArray>()
        val controller = VirtualKeyboardController(RemoteKeyboardState { snapshot ->
            snapshots += snapshot
            true
        })

        controller.press(RemoteKey.Backspace)
        controller.repeat(RemoteKey.Backspace)
        controller.repeat(RemoteKey.Backspace)
        controller.release(RemoteKey.Backspace)

        assertTrue(RemoteKey.Backspace.isRepeatable())
        assertTrue(RemoteKey.ArrowLeft.isRepeatable())
        assertTrue(RemoteKey.PageDown.isRepeatable())
        assertFalse(RemoteKey.CapsLock.isRepeatable())
        assertEquals(6, snapshots.size)
        assertEquals(3, snapshots.count { it.isPressed(RemoteKey.Backspace) })
        assertTrue(snapshots.last().all { it == 0.toByte() })
    }

    @Test
    fun failedPublicationIsRetriedAndReleaseCanStillPublish() {
        val attempts = mutableListOf<ByteArray>()
        var accept = false
        val state = RemoteKeyboardState { snapshot ->
            attempts += snapshot
            accept
        }

        state.setHardware(RemoteKey.A, true)
        assertEquals(1, attempts.size)
        accept = true
        state.setHardware(RemoteKey.A, true)
        state.setHardware(RemoteKey.A, false)

        assertEquals(3, attempts.size)
        assertTrue(attempts[1].isPressed(RemoteKey.A))
        assertTrue(attempts[2].all { it == 0.toByte() })
    }

    @Test
    fun failedReleaseRemainsRetryable() {
        val attempts = mutableListOf<ByteArray>()
        var failRelease = false
        val state = RemoteKeyboardState { snapshot ->
            attempts += snapshot
            !(failRelease && snapshot.all { it == 0.toByte() })
        }

        state.setHardware(RemoteKey.A, true)
        failRelease = true
        state.setHardware(RemoteKey.A, false)
        failRelease = false
        state.releaseAll()

        assertEquals(3, attempts.size)
        assertTrue(attempts.takeLast(2).all { snapshot -> snapshot.all { it == 0.toByte() } })
    }

    @Test
    fun committedUnicodeIsChunkedWithoutChangingCodePoints() {
        val text = "e\u0301 中文 مرحبا 😀" + "界".repeat(2000)
        val chunks = requireNotNull(committedTextChunks(text))

        assertEquals(text, chunks.joinToString(separator = ""))
        assertTrue(chunks.all { it.toByteArray(StandardCharsets.UTF_8).size <= 4096 })
        assertTrue(chunks.none { it.last().isHighSurrogate() })
        assertNull(committedTextChunks("a\u0000b"))
    }

    @Test
    fun routerUsesReliableTextOnlyWhenCapabilityAllows() {
        val sent = mutableListOf<String>()
        val fallback = mutableListOf<String>()
        val unsupported = mutableListOf<Int>()
        val router = RemoteTextRouter(
            publishText = { sent += it; true },
            unsupportedText = unsupported::add,
            layoutDependentAsciiFallback = fallback::add,
        )
        val text = "e\u0301 中文 مرحبا 😀"

        router.textInputAvailable = true
        router.commit(text)
        assertEquals(text, sent.joinToString(separator = ""))
        assertTrue(fallback.isEmpty())

        router.textInputAvailable = false
        router.commit(text)
        assertEquals(listOf(text), fallback)
        assertTrue(unsupported.isEmpty())
    }

    @Test
    fun asciiFallbackIsExplicitlyLayoutDependentAndReportsUnicode() {
        val fallback = layoutDependentAsciiStrokes("Az!\r\né😀")
        assertEquals(
            listOf(
                RemoteKeyStroke(RemoteKey.A, shift = true),
                RemoteKeyStroke(RemoteKey.Z),
                RemoteKeyStroke(RemoteKey.Num1, shift = true),
                RemoteKeyStroke(RemoteKey.Enter),
            ),
            fallback.strokes,
        )
        assertEquals(2, fallback.unsupportedCodePoints)
    }

    @Test
    fun cursorSnapshotExposesTextInputCapabilityBit() {
        val bytes = ByteBuffer.allocate(15).order(ByteOrder.LITTLE_ENDIAN)
            .put(2.toByte())
            .put(1.toByte())
            .putInt(3)
            .putLong(7)
            .put(0x40.toByte())
            .array()
        val update = requireNotNull(CursorSnapshotUpdate.from(bytes))
        val capabilities = requireNotNull(update.capabilities)
        assertEquals(3, update.transportGeneration)
        assertEquals(7, capabilities.revision)
        assertTrue(capabilities.value.textInput)
        assertFalse(capabilities.value.keyboard)
    }

    private fun ByteArray.isPressed(key: RemoteKey): Boolean =
        this[key.wireId / 8].toInt() and (1 shl (key.wireId % 8)) != 0
}
