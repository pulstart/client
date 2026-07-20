package io.kubemaxx.st

import java.nio.ByteBuffer
import java.nio.ByteOrder

internal object NativeBridge {
    init {
        System.loadLibrary("st_client_android")
    }

    /*
     * Rust catches every panic before it can cross JNI. Failure sentinels are:
     * handle/epoch/packed route/fill count 0 (fill uses -2 for an internal panic),
     * false for input/acceptance, null for optional arrays, non-null error strings
     * for operations returning String?, "error: ..." for status, and "[]" for
     * discovery. Void calls become no-ops.
     */
    @JvmStatic external fun nativeCreate(): Long

    @JvmStatic external fun nativeStart(
        handle: Long,
        server: String,
        token: String,
        refreshMilliHz: Int,
        apiUrl: String,
        clientPeerId: String,
        hostPeerId: String,
        requestNonce: Long,
    ): String?

    @JvmStatic external fun nativeGetSessionEpoch(handle: Long): Long
    @JvmStatic external fun nativeSetDiscoveryEnabled(handle: Long, enabled: Boolean)
    @JvmStatic external fun nativeStop(handle: Long)
    @JvmStatic external fun nativeDestroy(handle: Long)
    @JvmStatic external fun nativeGetStatus(handle: Long): String
    @JvmStatic external fun nativeTakeStreamConfig(handle: Long, epoch: Long): IntArray?
    @JvmStatic external fun nativePollCursorSnapshot(handle: Long, epoch: Long): ByteArray?
    @JvmStatic external fun nativePollAccessUnit(handle: Long, epoch: Long, timeoutMs: Int): ByteArray?
    @JvmStatic external fun nativeConfigureAudio(
        handle: Long,
        epoch: Long,
        sampleRate: Int,
        channels: Int,
        packetDurationMs: Int,
    ): String?

    @JvmStatic external fun nativeResetAudio(handle: Long, epoch: Long)
    @JvmStatic external fun nativeSetAudioEnabled(handle: Long, epoch: Long, enabled: Boolean): String?
    @JvmStatic external fun nativeFillAudio(handle: Long, epoch: Long, buffer: ByteBuffer, timeoutMs: Int): Int
    @JvmStatic external fun nativeSendAbsolute(
        handle: Long,
        epoch: Long,
        x: Int,
        y: Int,
        buttons: Int,
    ): Boolean

    @JvmStatic external fun nativeSendTrackpadDelta(
        handle: Long,
        epoch: Long,
        dx: Int,
        dy: Int,
        buttons: Int,
    ): Long
    @JvmStatic external fun nativeSendMouseButtons(handle: Long, epoch: Long, buttons: Int): Boolean
    @JvmStatic external fun nativeSendMouseWheel(
        handle: Long,
        epoch: Long,
        deltaX: Int,
        deltaY: Int,
        buttons: Int,
    ): Boolean
    @JvmStatic external fun nativeSendKeyboardState(
        handle: Long,
        epoch: Long,
        pressed: ByteArray,
    ): Boolean
    @JvmStatic external fun nativeSendTextInput(handle: Long, epoch: Long, text: String): Boolean

    @JvmStatic external fun nativeRequestKeyframe(handle: Long, epoch: Long)
    @JvmStatic external fun nativeAcceptStreamGeneration(
        handle: Long,
        epoch: Long,
        generation: Int,
    ): Boolean
    @JvmStatic external fun nativeGetDiscoveredServers(handle: Long, token: String): String
}

internal object PackedTrackpadRoute {
    /*
     * JNI result layout: mode in bits 0..7, normalized absolute x in bits 8..23,
     * and normalized absolute y in bits 24..39. Relative/rejected coordinates are zero.
     */
    const val REJECTED = 0
    const val RELATIVE = 1
    const val ABSOLUTE = 2

    private const val MODE_MASK = 0xffL
    private const val COORD_MASK = 0xffffL
    private const val X_SHIFT = 8
    private const val Y_SHIFT = 24

    fun mode(packed: Long): Int = (packed and MODE_MASK).toInt()
    fun absoluteX(packed: Long): Int = ((packed ushr X_SHIFT) and COORD_MASK).toInt()
    fun absoluteY(packed: Long): Int = ((packed ushr Y_SHIFT) and COORD_MASK).toInt()
}

internal data class StreamDescription(
    val transportGeneration: Int,
    val generation: Int,
    val width: Int,
    val height: Int,
    val cursorWidth: Int,
    val cursorHeight: Int,
    val framerate: Int,
    val audioSampleRate: Int,
    val audioChannels: Int,
    val packetDurationMs: Int,
) {
    fun decoderCompatibleWith(other: StreamDescription): Boolean =
        width == other.width && height == other.height

    fun audioCompatibleWith(other: StreamDescription): Boolean =
        audioSampleRate == other.audioSampleRate &&
            audioChannels == other.audioChannels &&
            packetDurationMs == other.packetDurationMs

        companion object {
            fun from(values: IntArray): StreamDescription? {
            if (values.size != 10 || values[0] <= 0 || values[2] <= 0 || values[3] <= 0 ||
                values[4] <= 0 || values[5] <= 0
            ) {
                return null
            }
            return StreamDescription(
                transportGeneration = values[0],
                generation = values[1],
                width = values[2],
                height = values[3],
                cursorWidth = values[4],
                cursorHeight = values[5],
                framerate = values[6].coerceAtLeast(1),
                audioSampleRate = values[7],
                audioChannels = values[8],
                packetDurationMs = values[9],
            )
        }
    }
}

internal data class StreamUpdatePlan(
    val restartDecoder: Boolean,
    val restartAudio: Boolean,
    val resetAudioDecoder: Boolean,
)

internal fun planStreamUpdate(
    previous: StreamDescription?,
    next: StreamDescription,
    decoderReady: Boolean,
): StreamUpdatePlan = StreamUpdatePlan(
    restartDecoder = previous == null || !previous.decoderCompatibleWith(next) || !decoderReady,
    restartAudio = previous == null || !previous.audioCompatibleWith(next),
    resetAudioDecoder = previous != null && previous.transportGeneration != next.transportGeneration,
)

internal data class RevisedValue<T>(val revision: Long, val value: T)

internal data class CursorCapabilities(
    val mouseAbsolute: Boolean,
    val mouseRelative: Boolean,
    val keyboard: Boolean,
    val separateCursor: Boolean,
    val hoverCapture: Boolean,
    val cursorPositionReliable: Boolean,
    val textInput: Boolean,
)

internal enum class ControllerOwnership(val code: Int) {
    UNAVAILABLE(0),
    AVAILABLE(1),
    OWNED_BY_YOU(2),
    OWNED_BY_OTHER(3),
    ;

    companion object {
        fun fromCode(code: Int): ControllerOwnership? = entries.firstOrNull { it.code == code }
    }
}

internal data class CursorShapeData(
    val serial: Long,
    val width: Int,
    val height: Int,
    val hotspotX: Int,
    val hotspotY: Int,
    val premultipliedRgba: ByteArray,
)

internal data class CursorStateData(
    val serial: Long,
    val x: Int,
    val y: Int,
    val visible: Boolean,
    val appGrab: Boolean,
)

internal data class CursorSnapshotUpdate(
    val transportGeneration: Int,
    val capabilities: RevisedValue<CursorCapabilities>?,
    val controllerState: RevisedValue<ControllerOwnership>?,
    val shape: RevisedValue<CursorShapeData>?,
    val state: RevisedValue<CursorStateData>?,
) {
    companion object {
        /*
         * JNI snapshot v2, all integers little-endian:
         * [version:u8][present:u8][transport_generation:u32], followed in flag-bit order by
         * bit 0 capabilities: [revision:u64][flags:u8], where flag bit 6 is
         *                     reliable committed Unicode text input
         * bit 1 controller:   [revision:u64][state:u8]
         * bit 2 shape:        [revision:u64][serial:u64][width:u16][height:u16]
         *                     [hotspot_x:u16][hotspot_y:u16][rgba_len:u32][RGBA8...]
         * bit 3 state:        [revision:u64][serial:u64][x:i32][y:i32][flags:u8]
         * State flag bit 0 is visible and bit 1 is app_grab. RGBA8 is already
         * premultiplied by the producer.
         */
        fun from(bytes: ByteArray): CursorSnapshotUpdate? = runCatching {
            val buffer = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
            buffer.requireRemaining(6)
            require(buffer.get().toInt() and 0xff == SNAPSHOT_VERSION)
            val present = buffer.get().toInt() and 0xff
            require(present and KNOWN_PRESENT_FLAGS.inv() == 0)
            val transportGeneration = buffer.int
            require(transportGeneration > 0)

            val capabilities = if (present and CAPABILITIES_PRESENT != 0) {
                buffer.requireRemaining(9)
                val revision = buffer.long
                val flags = buffer.get().toInt() and 0xff
                RevisedValue(
                    revision,
                    CursorCapabilities(
                        mouseAbsolute = flags and (1 shl 0) != 0,
                        mouseRelative = flags and (1 shl 1) != 0,
                        keyboard = flags and (1 shl 2) != 0,
                        separateCursor = flags and (1 shl 3) != 0,
                        hoverCapture = flags and (1 shl 4) != 0,
                        cursorPositionReliable = flags and (1 shl 5) != 0,
                        textInput = flags and (1 shl 6) != 0,
                    ),
                )
            } else {
                null
            }
            val controller = if (present and CONTROLLER_PRESENT != 0) {
                buffer.requireRemaining(9)
                val revision = buffer.long
                val ownership = requireNotNull(ControllerOwnership.fromCode(buffer.get().toInt() and 0xff))
                RevisedValue(revision, ownership)
            } else {
                null
            }
            val shape = if (present and SHAPE_PRESENT != 0) {
                buffer.requireRemaining(28)
                val revision = buffer.long
                val serial = buffer.long
                val width = buffer.short.toInt() and 0xffff
                val height = buffer.short.toInt() and 0xffff
                val hotspotX = buffer.short.toInt() and 0xffff
                val hotspotY = buffer.short.toInt() and 0xffff
                val rgbaLength = buffer.int
                val expectedLength = width.toLong() * height.toLong() * 4L
                require(rgbaLength >= 0 && rgbaLength.toLong() == expectedLength)
                buffer.requireRemaining(rgbaLength)
                val rgba = ByteArray(rgbaLength)
                buffer.get(rgba)
                RevisedValue(
                    revision,
                    CursorShapeData(serial, width, height, hotspotX, hotspotY, rgba),
                )
            } else {
                null
            }
            val state = if (present and STATE_PRESENT != 0) {
                buffer.requireRemaining(25)
                val revision = buffer.long
                val serial = buffer.long
                val x = buffer.int
                val y = buffer.int
                val flags = buffer.get().toInt() and 0xff
                RevisedValue(
                    revision,
                    CursorStateData(
                        serial = serial,
                        x = x,
                        y = y,
                        visible = flags and (1 shl 0) != 0,
                        appGrab = flags and (1 shl 1) != 0,
                    ),
                )
            } else {
                null
            }
            require(!buffer.hasRemaining())
            CursorSnapshotUpdate(transportGeneration, capabilities, controller, shape, state)
        }.getOrNull()

        private fun ByteBuffer.requireRemaining(count: Int) {
            require(count >= 0 && remaining() >= count)
        }

        private const val SNAPSHOT_VERSION = 2
        private const val CAPABILITIES_PRESENT = 1 shl 0
        private const val CONTROLLER_PRESENT = 1 shl 1
        private const val SHAPE_PRESENT = 1 shl 2
        private const val STATE_PRESENT = 1 shl 3
        private const val KNOWN_PRESENT_FLAGS = CAPABILITIES_PRESENT or CONTROLLER_PRESENT or
            SHAPE_PRESENT or STATE_PRESENT
    }
}
