package io.kubemaxx.st

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import java.util.ArrayDeque

internal enum class RemoteKey(val wireId: Int) {
    Escape(0),
    Tab(1),
    Backspace(2),
    Enter(3),
    Space(4),
    Insert(5),
    Delete(6),
    Home(7),
    End(8),
    PageUp(9),
    PageDown(10),
    ArrowUp(11),
    ArrowDown(12),
    ArrowLeft(13),
    ArrowRight(14),
    Minus(15),
    Equals(16),
    OpenBracket(17),
    CloseBracket(18),
    Backslash(19),
    Semicolon(20),
    Quote(21),
    Backtick(22),
    Comma(23),
    Period(24),
    Slash(25),
    Num0(26),
    Num1(27),
    Num2(28),
    Num3(29),
    Num4(30),
    Num5(31),
    Num6(32),
    Num7(33),
    Num8(34),
    Num9(35),
    A(36),
    B(37),
    C(38),
    D(39),
    E(40),
    F(41),
    G(42),
    H(43),
    I(44),
    J(45),
    K(46),
    L(47),
    M(48),
    N(49),
    O(50),
    P(51),
    Q(52),
    R(53),
    S(54),
    T(55),
    U(56),
    V(57),
    W(58),
    X(59),
    Y(60),
    Z(61),
    F1(62),
    F2(63),
    F3(64),
    F4(65),
    F5(66),
    F6(67),
    F7(68),
    F8(69),
    F9(70),
    F10(71),
    F11(72),
    F12(73),
    LeftShift(74),
    LeftCtrl(75),
    LeftAlt(76),
    LeftMeta(77),
    RightShift(78),
    RightCtrl(79),
    RightAlt(80),
    RightMeta(81),
    CapsLock(82),
    NumLock(83),
    ScrollLock(84),
    PrintScreen(85),
    Pause(86),
    Application(87),
    Numpad0(88),
    Numpad1(89),
    Numpad2(90),
    Numpad3(91),
    Numpad4(92),
    Numpad5(93),
    Numpad6(94),
    Numpad7(95),
    Numpad8(96),
    Numpad9(97),
    NumpadDecimal(98),
    NumpadDivide(99),
    NumpadMultiply(100),
    NumpadSubtract(101),
    NumpadAdd(102),
    NumpadEnter(103),
    NumpadEquals(104),
    NumpadComma(105),
    F13(106),
    F14(107),
    F15(108),
    F16(109),
    F17(110),
    F18(111),
    F19(112),
    F20(113),
    F21(114),
    F22(115),
    F23(116),
    F24(117),
    VolumeMute(118),
    VolumeDown(119),
    VolumeUp(120),
    MediaPrevious(121),
    MediaNext(122),
    MediaPlayPause(123),
    MediaStop(124),
    IntlBackslash(125),
}

internal data class RemoteKeyStroke(val key: RemoteKey, val shift: Boolean = false)

internal object RemoteKeyMap {
    private val letters = arrayOf(
        RemoteKey.A, RemoteKey.B, RemoteKey.C, RemoteKey.D, RemoteKey.E, RemoteKey.F,
        RemoteKey.G, RemoteKey.H, RemoteKey.I, RemoteKey.J, RemoteKey.K, RemoteKey.L,
        RemoteKey.M, RemoteKey.N, RemoteKey.O, RemoteKey.P, RemoteKey.Q, RemoteKey.R,
        RemoteKey.S, RemoteKey.T, RemoteKey.U, RemoteKey.V, RemoteKey.W, RemoteKey.X,
        RemoteKey.Y, RemoteKey.Z,
    )
    private val topRowDigits = arrayOf(
        RemoteKey.Num0, RemoteKey.Num1, RemoteKey.Num2, RemoteKey.Num3, RemoteKey.Num4,
        RemoteKey.Num5, RemoteKey.Num6, RemoteKey.Num7, RemoteKey.Num8, RemoteKey.Num9,
    )
    private val numpadDigits = arrayOf(
        RemoteKey.Numpad0, RemoteKey.Numpad1, RemoteKey.Numpad2, RemoteKey.Numpad3,
        RemoteKey.Numpad4, RemoteKey.Numpad5, RemoteKey.Numpad6, RemoteKey.Numpad7,
        RemoteKey.Numpad8, RemoteKey.Numpad9,
    )
    private val functionKeys = arrayOf(
        RemoteKey.F1, RemoteKey.F2, RemoteKey.F3, RemoteKey.F4, RemoteKey.F5, RemoteKey.F6,
        RemoteKey.F7, RemoteKey.F8, RemoteKey.F9, RemoteKey.F10, RemoteKey.F11, RemoteKey.F12,
    )
    private val extendedFunctionKeys = arrayOf(
        RemoteKey.F13, RemoteKey.F14, RemoteKey.F15, RemoteKey.F16, RemoteKey.F17, RemoteKey.F18,
        RemoteKey.F19, RemoteKey.F20, RemoteKey.F21, RemoteKey.F22, RemoteKey.F23, RemoteKey.F24,
    )

    fun fromCharacter(character: Char): RemoteKeyStroke? {
        if (character in 'a'..'z') {
            return RemoteKeyStroke(letters[character - 'a'])
        }
        if (character in 'A'..'Z') {
            return RemoteKeyStroke(letters[character - 'A'], shift = true)
        }
        if (character in '0'..'9') {
            return RemoteKeyStroke(topRowDigits[character - '0'])
        }
        return when (character) {
            ' ' -> RemoteKeyStroke(RemoteKey.Space)
            '\t' -> RemoteKeyStroke(RemoteKey.Tab)
            '\r', '\n' -> RemoteKeyStroke(RemoteKey.Enter)
            '-' -> RemoteKeyStroke(RemoteKey.Minus)
            '_' -> RemoteKeyStroke(RemoteKey.Minus, true)
            '=' -> RemoteKeyStroke(RemoteKey.Equals)
            '+' -> RemoteKeyStroke(RemoteKey.Equals, true)
            '[' -> RemoteKeyStroke(RemoteKey.OpenBracket)
            '{' -> RemoteKeyStroke(RemoteKey.OpenBracket, true)
            ']' -> RemoteKeyStroke(RemoteKey.CloseBracket)
            '}' -> RemoteKeyStroke(RemoteKey.CloseBracket, true)
            '\\' -> RemoteKeyStroke(RemoteKey.Backslash)
            '|' -> RemoteKeyStroke(RemoteKey.Backslash, true)
            ';' -> RemoteKeyStroke(RemoteKey.Semicolon)
            ':' -> RemoteKeyStroke(RemoteKey.Semicolon, true)
            '\'' -> RemoteKeyStroke(RemoteKey.Quote)
            '"' -> RemoteKeyStroke(RemoteKey.Quote, true)
            '`' -> RemoteKeyStroke(RemoteKey.Backtick)
            '~' -> RemoteKeyStroke(RemoteKey.Backtick, true)
            ',' -> RemoteKeyStroke(RemoteKey.Comma)
            '<' -> RemoteKeyStroke(RemoteKey.Comma, true)
            '.' -> RemoteKeyStroke(RemoteKey.Period)
            '>' -> RemoteKeyStroke(RemoteKey.Period, true)
            '/' -> RemoteKeyStroke(RemoteKey.Slash)
            '?' -> RemoteKeyStroke(RemoteKey.Slash, true)
            '!' -> RemoteKeyStroke(RemoteKey.Num1, true)
            '@' -> RemoteKeyStroke(RemoteKey.Num2, true)
            '#' -> RemoteKeyStroke(RemoteKey.Num3, true)
            '$' -> RemoteKeyStroke(RemoteKey.Num4, true)
            '%' -> RemoteKeyStroke(RemoteKey.Num5, true)
            '^' -> RemoteKeyStroke(RemoteKey.Num6, true)
            '&' -> RemoteKeyStroke(RemoteKey.Num7, true)
            '*' -> RemoteKeyStroke(RemoteKey.Num8, true)
            '(' -> RemoteKeyStroke(RemoteKey.Num9, true)
            ')' -> RemoteKeyStroke(RemoteKey.Num0, true)
            else -> null
        }
    }

    fun fromAndroidKeyCode(keyCode: Int, scanCode: Int = 0): RemoteKey? {
        fromLinuxScanCode(scanCode)?.let { return it }
        return when (keyCode) {
        KeyEvent.KEYCODE_ESCAPE -> RemoteKey.Escape
        KeyEvent.KEYCODE_TAB -> RemoteKey.Tab
        KeyEvent.KEYCODE_DEL -> RemoteKey.Backspace
        KeyEvent.KEYCODE_ENTER -> RemoteKey.Enter
        KeyEvent.KEYCODE_NUMPAD_ENTER -> RemoteKey.NumpadEnter
        KeyEvent.KEYCODE_SPACE -> RemoteKey.Space
        KeyEvent.KEYCODE_INSERT -> RemoteKey.Insert
        KeyEvent.KEYCODE_FORWARD_DEL -> RemoteKey.Delete
        KeyEvent.KEYCODE_MOVE_HOME -> RemoteKey.Home
        KeyEvent.KEYCODE_MOVE_END -> RemoteKey.End
        KeyEvent.KEYCODE_PAGE_UP -> RemoteKey.PageUp
        KeyEvent.KEYCODE_PAGE_DOWN -> RemoteKey.PageDown
        KeyEvent.KEYCODE_DPAD_UP -> RemoteKey.ArrowUp
        KeyEvent.KEYCODE_DPAD_DOWN -> RemoteKey.ArrowDown
        KeyEvent.KEYCODE_DPAD_LEFT -> RemoteKey.ArrowLeft
        KeyEvent.KEYCODE_DPAD_RIGHT -> RemoteKey.ArrowRight
        KeyEvent.KEYCODE_MINUS -> RemoteKey.Minus
        KeyEvent.KEYCODE_EQUALS -> RemoteKey.Equals
        KeyEvent.KEYCODE_LEFT_BRACKET -> RemoteKey.OpenBracket
        KeyEvent.KEYCODE_RIGHT_BRACKET -> RemoteKey.CloseBracket
        KeyEvent.KEYCODE_BACKSLASH -> RemoteKey.Backslash
        KeyEvent.KEYCODE_SEMICOLON -> RemoteKey.Semicolon
        KeyEvent.KEYCODE_APOSTROPHE -> RemoteKey.Quote
        KeyEvent.KEYCODE_GRAVE -> RemoteKey.Backtick
        KeyEvent.KEYCODE_COMMA -> RemoteKey.Comma
        KeyEvent.KEYCODE_PERIOD -> RemoteKey.Period
        KeyEvent.KEYCODE_SLASH -> RemoteKey.Slash
        in KeyEvent.KEYCODE_0..KeyEvent.KEYCODE_9 -> topRowDigits[keyCode - KeyEvent.KEYCODE_0]
        in KeyEvent.KEYCODE_NUMPAD_0..KeyEvent.KEYCODE_NUMPAD_9 ->
            numpadDigits[keyCode - KeyEvent.KEYCODE_NUMPAD_0]
        KeyEvent.KEYCODE_NUMPAD_DIVIDE -> RemoteKey.NumpadDivide
        KeyEvent.KEYCODE_NUMPAD_MULTIPLY -> RemoteKey.NumpadMultiply
        KeyEvent.KEYCODE_NUMPAD_SUBTRACT -> RemoteKey.NumpadSubtract
        KeyEvent.KEYCODE_NUMPAD_ADD -> RemoteKey.NumpadAdd
        KeyEvent.KEYCODE_NUMPAD_DOT -> RemoteKey.NumpadDecimal
        KeyEvent.KEYCODE_NUMPAD_COMMA -> RemoteKey.NumpadComma
        KeyEvent.KEYCODE_NUMPAD_EQUALS -> RemoteKey.NumpadEquals
        in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z -> letters[keyCode - KeyEvent.KEYCODE_A]
        in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 -> functionKeys[keyCode - KeyEvent.KEYCODE_F1]
        KeyEvent.KEYCODE_SHIFT_LEFT -> RemoteKey.LeftShift
        KeyEvent.KEYCODE_CTRL_LEFT -> RemoteKey.LeftCtrl
        KeyEvent.KEYCODE_ALT_LEFT -> RemoteKey.LeftAlt
        KeyEvent.KEYCODE_META_LEFT -> RemoteKey.LeftMeta
        KeyEvent.KEYCODE_SHIFT_RIGHT -> RemoteKey.RightShift
        KeyEvent.KEYCODE_CTRL_RIGHT -> RemoteKey.RightCtrl
        KeyEvent.KEYCODE_ALT_RIGHT -> RemoteKey.RightAlt
        KeyEvent.KEYCODE_META_RIGHT -> RemoteKey.RightMeta
        KeyEvent.KEYCODE_CAPS_LOCK -> RemoteKey.CapsLock
        KeyEvent.KEYCODE_NUM_LOCK -> RemoteKey.NumLock
        KeyEvent.KEYCODE_SCROLL_LOCK -> RemoteKey.ScrollLock
        KeyEvent.KEYCODE_SYSRQ -> RemoteKey.PrintScreen
        KeyEvent.KEYCODE_BREAK -> RemoteKey.Pause
        KeyEvent.KEYCODE_MENU -> RemoteKey.Application
        KeyEvent.KEYCODE_VOLUME_MUTE -> RemoteKey.VolumeMute
        KeyEvent.KEYCODE_VOLUME_DOWN -> RemoteKey.VolumeDown
        KeyEvent.KEYCODE_VOLUME_UP -> RemoteKey.VolumeUp
        KeyEvent.KEYCODE_MEDIA_PREVIOUS -> RemoteKey.MediaPrevious
        KeyEvent.KEYCODE_MEDIA_NEXT -> RemoteKey.MediaNext
        KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE -> RemoteKey.MediaPlayPause
        KeyEvent.KEYCODE_MEDIA_STOP -> RemoteKey.MediaStop
        KeyEvent.KEYCODE_RO, KeyEvent.KEYCODE_YEN -> RemoteKey.IntlBackslash
        else -> null
        }
    }

    fun fromAndroidKeyStroke(keyCode: Int, scanCode: Int = 0): RemoteKeyStroke? =
        fromAndroidKeyCode(keyCode, scanCode)?.let(::RemoteKeyStroke)

    private fun fromLinuxScanCode(scanCode: Int): RemoteKey? = when (scanCode) {
        in 183..194 -> extendedFunctionKeys[scanCode - 183]
        86 -> RemoteKey.IntlBackslash
        else -> null
    }
}

internal class RemoteKeyboardState(private val publish: (ByteArray) -> Boolean) {
    private val hardware = BooleanArray(KEYBOARD_STATE_BITS)
    private val synthetic = IntArray(KEYBOARD_STATE_BITS)
    private var lastPublished = ByteArray(KEYBOARD_STATE_BYTES)

    fun setHardware(key: RemoteKey, pressed: Boolean) {
        if (hardware[key.wireId] == pressed) {
            publishIfChanged()
            return
        }
        hardware[key.wireId] = pressed
        publishIfChanged()
    }

    fun setSynthetic(key: RemoteKey, pressed: Boolean) {
        val index = key.wireId
        synthetic[index] = (synthetic[index] + if (pressed) 1 else -1).coerceAtLeast(0)
        publishIfChanged()
    }

    fun isHardwarePressed(key: RemoteKey): Boolean = hardware[key.wireId]

    fun releaseAll() {
        hardware.fill(false)
        synthetic.fill(0)
        publishIfChanged(force = true)
    }

    private fun publishIfChanged(force: Boolean = false) {
        val next = ByteArray(KEYBOARD_STATE_BYTES)
        RemoteKey.entries.forEach { key ->
            if (hardware[key.wireId] || synthetic[key.wireId] > 0) {
                val byte = key.wireId / 8
                next[byte] = (next[byte].toInt() or (1 shl (key.wireId % 8))).toByte()
            }
        }
        if (!force && next.contentEquals(lastPublished)) return
        if (publish(next.copyOf())) {
            lastPublished = next
        }
    }

    private companion object {
        const val KEYBOARD_STATE_BYTES = 16
        const val KEYBOARD_STATE_BITS = KEYBOARD_STATE_BYTES * 8
    }
}

internal class RemoteKeyboardView(
    context: Context,
    publish: (ByteArray) -> Boolean,
    publishText: (String) -> Boolean,
    private val dismiss: () -> Unit,
    private val unsupportedText: (Int) -> Unit,
) : View(context) {
    private val handler = Handler(Looper.getMainLooper())
    private val state = RemoteKeyboardState(publish)
    private val pendingStrokes = ArrayDeque<RemoteKeyStroke>()
    private val eventModifiers = mutableMapOf<Int, List<RemoteKey>>()
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val releaseStroke = Runnable(::releaseCurrentStroke)
    private val textRouter = RemoteTextRouter(publishText, unsupportedText, ::enqueueLayoutDependentAsciiFallback)
    private var currentStroke: RemoteKeyStroke? = null
    private var composingText = ""
    private var composingCursor = 0
    private var available = false
    private var open = false

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        isClickable = true
        contentDescription = "Remote keyboard"
    }

    fun setAvailable(value: Boolean) {
        available = value
        isEnabled = value
        alpha = if (value) 1f else 0.45f
        invalidate()
    }

    fun setTextInputAvailable(value: Boolean) {
        textRouter.textInputAvailable = value
    }

    fun setOpen(value: Boolean) {
        open = value
        invalidate()
    }

    @Suppress("DEPRECATION")
    fun handleKeyEvent(event: KeyEvent): Boolean {
        if (!available) return false
        if (event.action == KeyEvent.ACTION_MULTIPLE && !event.characters.isNullOrEmpty()) {
            commitText(event.characters.orEmpty())
            return true
        }
        val stroke = RemoteKeyMap.fromAndroidKeyStroke(event.keyCode, event.scanCode) ?: return false
        val key = stroke.key
        val eventId = if (event.keyCode == KeyEvent.KEYCODE_UNKNOWN) -event.scanCode else event.keyCode
        when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                if (!eventModifiers.containsKey(eventId)) {
                    val modifiers = event.remoteModifiers(key, stroke.shift)
                    eventModifiers[eventId] = modifiers
                    modifiers.forEach { state.setSynthetic(it, true) }
                }
                state.setHardware(key, true)
            }
            KeyEvent.ACTION_UP -> {
                state.setHardware(key, false)
                eventModifiers.remove(eventId)?.asReversed()?.forEach {
                    state.setSynthetic(it, false)
                }
            }
            else -> return false
        }
        return true
    }

    fun commitText(text: CharSequence) {
        textRouter.commit(text.toString())
    }

    private fun enqueueLayoutDependentAsciiFallback(text: String) {
        val fallback = layoutDependentAsciiStrokes(text)
        pendingStrokes.addAll(fallback.strokes)
        if (fallback.unsupportedCodePoints > 0) unsupportedText(fallback.unsupportedCodePoints)
        startNextStroke()
    }

    fun enqueueTaps(key: RemoteKey, count: Int = 1) {
        repeat(count.coerceIn(0, MAX_QUEUED_DELETES)) { pendingStrokes.addLast(RemoteKeyStroke(key)) }
        startNextStroke()
    }

    fun closeKeyboardSession() {
        handler.removeCallbacks(releaseStroke)
        pendingStrokes.clear()
        eventModifiers.clear()
        currentStroke = null
        composingText = ""
        composingCursor = 0
        state.releaseAll()
    }

    override fun onCheckIsTextEditor(): Boolean = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE or
            InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_ACTION_NONE or EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        outAttrs.initialSelStart = 0
        outAttrs.initialSelEnd = 0
        return object : BaseInputConnection(this, true) {
            override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
                composingText = text?.toString().orEmpty()
                composingCursor = if (newCursorPosition > 0) {
                    composingText.length + newCursorPosition - 1
                } else {
                    newCursorPosition
                }.coerceIn(0, composingText.length)
                return true
            }

            override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
                composingText = ""
                composingCursor = 0
                text?.let(::commitText)
                return true
            }

            override fun finishComposingText(): Boolean {
                composingText = ""
                composingCursor = 0
                return true
            }

            override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
                deleteAroundComposition(beforeLength, afterLength, codePoints = false)
                return true
            }

            override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean {
                deleteAroundComposition(beforeLength, afterLength, codePoints = true)
                return true
            }

            override fun getTextBeforeCursor(length: Int, flags: Int): CharSequence =
                composingText.substring(0, composingCursor).takeLast(length.coerceAtLeast(0))

            override fun getTextAfterCursor(length: Int, flags: Int): CharSequence =
                composingText.substring(composingCursor).take(length.coerceAtLeast(0))

            override fun sendKeyEvent(event: KeyEvent): Boolean = handleKeyEvent(event)

            override fun performEditorAction(actionCode: Int): Boolean {
                enqueueTaps(RemoteKey.Enter)
                return true
            }
        }
    }

    override fun onKeyPreIme(keyCode: Int, event: KeyEvent): Boolean {
        if (keyCode == KeyEvent.KEYCODE_BACK && event.action == KeyEvent.ACTION_UP) {
            dismiss()
            return true
        }
        return super.onKeyPreIme(keyCode, event)
    }

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        val radius = minOf(width, height) * 0.42f
        paint.color = when {
            !available -> Color.argb(180, 60, 64, 72)
            open -> Color.rgb(45, 135, 180)
            else -> Color.argb(220, 31, 36, 45)
        }
        canvas.drawCircle(width / 2f, height / 2f, radius, paint)
        paint.color = Color.WHITE
        paint.textAlign = Paint.Align.CENTER
        paint.textSize = resources.displayMetrics.density * resources.configuration.fontScale * 18f
        paint.typeface = android.graphics.Typeface.DEFAULT_BOLD
        val baseline = height / 2f - (paint.ascent() + paint.descent()) / 2f
        canvas.drawText("K", width / 2f, baseline, paint)
    }

    override fun performClick(): Boolean {
        super.performClick()
        return true
    }

    private fun startNextStroke() {
        if (!available || currentStroke != null) return
        val stroke = pendingStrokes.pollFirst() ?: return
        currentStroke = stroke
        if (stroke.shift) state.setSynthetic(RemoteKey.LeftShift, true)
        state.setSynthetic(stroke.key, true)
        handler.postDelayed(releaseStroke, KEY_HOLD_MS)
    }

    private fun releaseCurrentStroke() {
        val stroke = currentStroke ?: return
        state.setSynthetic(stroke.key, false)
        if (stroke.shift) state.setSynthetic(RemoteKey.LeftShift, false)
        currentStroke = null
        handler.postDelayed(::startNextStroke, KEY_GAP_MS)
    }

    private fun deleteAroundComposition(before: Int, after: Int, codePoints: Boolean) {
        var remainingBefore = before.coerceAtLeast(0)
        var remainingAfter = after.coerceAtLeast(0)
        if (composingText.isNotEmpty()) {
            val start = if (codePoints) {
                val available = composingText.codePointCount(0, composingCursor)
                val consumed = remainingBefore.coerceAtMost(available)
                remainingBefore -= consumed
                composingText.offsetByCodePoints(composingCursor, -consumed)
            } else {
                val consumed = remainingBefore.coerceAtMost(composingCursor)
                remainingBefore -= consumed
                composingCursor - consumed
            }
            val end = if (codePoints) {
                val available = composingText.codePointCount(composingCursor, composingText.length)
                val consumed = remainingAfter.coerceAtMost(available)
                remainingAfter -= consumed
                composingText.offsetByCodePoints(composingCursor, consumed)
            } else {
                val consumed = remainingAfter.coerceAtMost(composingText.length - composingCursor)
                remainingAfter -= consumed
                composingCursor + consumed
            }
            composingText = composingText.removeRange(start, end)
            composingCursor = start
        }
        enqueueTaps(RemoteKey.Backspace, remainingBefore)
        enqueueTaps(RemoteKey.Delete, remainingAfter)
    }

    private fun KeyEvent.remoteModifiers(key: RemoteKey, forceShift: Boolean): List<RemoteKey> {
        if (key.isModifier()) return emptyList()
        return buildList {
            modifierForEvent(
                forceShift || isShiftPressed,
                metaState and KeyEvent.META_SHIFT_LEFT_ON != 0,
                metaState and KeyEvent.META_SHIFT_RIGHT_ON != 0,
                RemoteKey.LeftShift,
                RemoteKey.RightShift,
            )?.let(::add)
            modifierForEvent(
                isCtrlPressed,
                metaState and KeyEvent.META_CTRL_LEFT_ON != 0,
                metaState and KeyEvent.META_CTRL_RIGHT_ON != 0,
                RemoteKey.LeftCtrl,
                RemoteKey.RightCtrl,
            )?.let(::add)
            modifierForEvent(
                isAltPressed,
                metaState and KeyEvent.META_ALT_LEFT_ON != 0,
                metaState and KeyEvent.META_ALT_RIGHT_ON != 0,
                RemoteKey.LeftAlt,
                RemoteKey.RightAlt,
            )?.let(::add)
            modifierForEvent(
                isMetaPressed,
                metaState and KeyEvent.META_META_LEFT_ON != 0,
                metaState and KeyEvent.META_META_RIGHT_ON != 0,
                RemoteKey.LeftMeta,
                RemoteKey.RightMeta,
            )?.let(::add)
        }
    }

    private fun modifierForEvent(
        active: Boolean,
        leftRequested: Boolean,
        rightRequested: Boolean,
        left: RemoteKey,
        right: RemoteKey,
    ): RemoteKey? = when {
        !active -> null
        rightRequested && !state.isHardwarePressed(right) -> right
        leftRequested && !state.isHardwarePressed(left) -> left
        state.isHardwarePressed(left) || state.isHardwarePressed(right) -> null
        else -> left
    }

    private companion object {
        const val KEY_HOLD_MS = 60L
        const val KEY_GAP_MS = 8L
        const val MAX_QUEUED_DELETES = 128
    }
}

internal class RemoteTextRouter(
    private val publishText: (String) -> Boolean,
    private val unsupportedText: (Int) -> Unit,
    private val layoutDependentAsciiFallback: (String) -> Unit,
) {
    var textInputAvailable = false

    fun commit(text: String) {
        if (text.isEmpty()) return
        if (!textInputAvailable) {
            layoutDependentAsciiFallback(text)
            return
        }
        val chunks = committedTextChunks(text)
        if (chunks == null) {
            unsupportedText(text.countCodePoints { it == 0 })
            return
        }
        for (chunk in chunks) {
            if (!publishText(chunk)) break
        }
    }
}

internal data class AsciiFallback(
    val strokes: List<RemoteKeyStroke>,
    val unsupportedCodePoints: Int,
)

internal fun layoutDependentAsciiStrokes(text: String): AsciiFallback {
    val strokes = mutableListOf<RemoteKeyStroke>()
    var unsupported = 0
    var previousWasCarriageReturn = false
    var index = 0
    while (index < text.length) {
        val codePoint = Character.codePointAt(text, index)
        index += Character.charCount(codePoint)
        val character = codePoint.takeIf { it in 1..0x7f }?.toChar()
        if (character == '\n' && previousWasCarriageReturn) {
            previousWasCarriageReturn = false
            continue
        }
        val stroke = character?.let(RemoteKeyMap::fromCharacter)
        if (stroke == null) unsupported += 1 else strokes += stroke
        previousWasCarriageReturn = character == '\r'
    }
    return AsciiFallback(strokes, unsupported)
}

internal fun committedTextChunks(text: String, maxBytes: Int = MAX_TEXT_INPUT_BYTES): List<String>? {
    require(maxBytes >= 4)
    if (text.isEmpty()) return emptyList()
    if ('\u0000' in text) return null

    val chunks = mutableListOf<String>()
    val chunk = StringBuilder()
    var chunkBytes = 0
    var index = 0
    while (index < text.length) {
        val codePoint = Character.codePointAt(text, index)
        val charCount = Character.charCount(codePoint)
        val bytes = when {
            codePoint <= 0x7f -> 1
            codePoint <= 0x7ff -> 2
            codePoint <= 0xffff -> 3
            else -> 4
        }
        if (chunkBytes + bytes > maxBytes) {
            chunks += chunk.toString()
            chunk.clear()
            chunkBytes = 0
        }
        chunk.append(text, index, index + charCount)
        chunkBytes += bytes
        index += charCount
    }
    if (chunk.isNotEmpty()) chunks += chunk.toString()
    return chunks
}

private inline fun String.countCodePoints(predicate: (Int) -> Boolean): Int {
    var count = 0
    var index = 0
    while (index < length) {
        val codePoint = Character.codePointAt(this, index)
        if (predicate(codePoint)) count += 1
        index += Character.charCount(codePoint)
    }
    return count
}

private fun RemoteKey.isModifier(): Boolean = when (this) {
    RemoteKey.LeftShift,
    RemoteKey.LeftCtrl,
    RemoteKey.LeftAlt,
    RemoteKey.LeftMeta,
    RemoteKey.RightShift,
    RemoteKey.RightCtrl,
    RemoteKey.RightAlt,
    RemoteKey.RightMeta,
    -> true
    else -> false
}

private const val MAX_TEXT_INPUT_BYTES = 4096
