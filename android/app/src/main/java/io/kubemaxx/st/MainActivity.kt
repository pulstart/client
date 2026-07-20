package io.kubemaxx.st

import android.content.Context
import android.graphics.Color
import android.graphics.Rect
import android.graphics.drawable.GradientDrawable
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.text.Editable
import android.text.InputType
import android.text.TextWatcher
import android.view.Gravity
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewConfiguration
import android.view.ViewGroup
import android.view.ViewTreeObserver
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.view.inputmethod.InputMethodManager
import android.view.inputmethod.EditorInfo
import android.widget.AdapterView
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.TextView
import android.widget.Toast
import android.text.method.PasswordTransformationMethod
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit
import kotlin.math.hypot
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

private enum class VideoScaleMode(val label: String) {
    FIT("Fit"),
    COVER("Fill / Cover"),
    STRETCH("Stretch"),
}

internal fun isConnectionPending(status: String): Boolean = status.startsWith("connecting")

class MainActivity : ComponentActivity(), SurfaceHolder.Callback {
    private enum class HomePage { SERVERS, SETTINGS, UPDATE, ABOUT }

    private val nativeHandle = NativeBridge.nativeCreate()
    private val tokenStore by lazy {
        TokenStore(getSharedPreferences(PREFERENCES, MODE_PRIVATE))
    }
    private val handler = Handler(Looper.getMainLooper())
    private val apiDiscoveryExecutor = Executors.newSingleThreadScheduledExecutor { runnable ->
        Thread(runnable, "st-api-discovery")
    }
    private lateinit var homeContainer: LinearLayout
    private lateinit var rootContainer: FrameLayout
    private lateinit var navigation: LinearLayout
    private lateinit var homeScroll: ScrollView
    private lateinit var serversPanel: LinearLayout
    private lateinit var settingsPanel: LinearLayout
    private lateinit var updatePanel: LinearLayout
    private lateinit var aboutPanel: LinearLayout
    private lateinit var serverCardsContainer: LinearLayout
    private lateinit var searchInput: EditText
    private lateinit var addServerInput: EditText
    private lateinit var addServerBar: LinearLayout
    private lateinit var apiStatusText: TextView
    private lateinit var tokenInput: EditText
    private lateinit var homeStatusText: TextView
    private lateinit var statusText: TextView
    private lateinit var statusPanel: LinearLayout
    private lateinit var menuStatusText: TextView
    private lateinit var menuLauncher: FrameLayout
    private lateinit var keyboardLauncher: RemoteKeyboardView
    private lateinit var floatingMenu: LinearLayout
    private lateinit var settingsAudioToggle: CheckBox
    private lateinit var menuAudioToggle: CheckBox
    private lateinit var settingsImmersiveToggle: CheckBox
    private lateinit var menuImmersiveToggle: CheckBox
    private lateinit var settingsScaleSpinner: Spinner
    private lateinit var menuScaleSpinner: Spinner
    private lateinit var streamArea: FrameLayout
    private lateinit var videoView: AspectSurfaceView
    private lateinit var cursorOverlay: CursorOverlayView
    private var currentPage = HomePage.SERVERS
    private var sessionStarted = false
    private var currentSessionEpoch = 0L
    private var pendingConnect = false
    private var surfaceReady = false
    private var connected = false
    private var touchEnabled = true
    private var touchSensitivity = 1.5f
    private var refreshRateHz = 60
    private var audioEnabled = true
    private var immersiveFullscreenEnabled = true
    private var videoScaleMode = VideoScaleMode.FIT
    private var syncingControls = false
    private var menuOpen = false
    private var keyboardOpen = false
    private var keyboardShowGeneration = 0L
    private var imeShowRequested = false
    private var keyboardCapability = false
    private var textInputCapability = false
    private var controllerOwnership = ControllerOwnership.UNAVAILABLE
    private var imeVisible = false
    private var legacyImeLayoutListener: ViewTreeObserver.OnGlobalLayoutListener? = null
    private var consumeDismissTouch = false
    private var launcherNormalizedX = Float.NaN
    private var launcherNormalizedY = Float.NaN
    private var launcherDragging = false
    private var launcherDownRawX = 0f
    private var launcherDownRawY = 0f
    private var launcherStartX = 0f
    private var launcherStartY = 0f
    private var decoder: AvcSurfaceDecoder? = null
    private var pendingDecoderStream: StreamDescription? = null
    private var decoderTransitionStartedAt = 0L
    private var audioPlayer: AndroidAudioPlayer? = null
    private var decoderStatus = ""
    private var audioStatus = ""
    private var connectionStatus = "disconnected"
    private var controlTransportGeneration = 0
    private var streamGeneration = 0
    private var audioAttemptGeneration = 0
    private var activeStream: StreamDescription? = null
    private var savedServers = mutableListOf<SavedServer>()
    private var lanDiscoveredServers = emptyList<LanDiscoveredServer>()
    private var apiHostPresence: ApiHostPresence? = null
    private var apiHostLastSeen = 0L
    private var apiDiscoveryOnline = false
    private var lastDiscoveryJson = ""
    private var nextDiscoveryRefresh = 0L
    private var discoveryLock: WifiManager.MulticastLock? = null
    private var apiDiscoveryFuture: ScheduledFuture<*>? = null
    @Volatile private var apiDiscoveryStarted = false
    @Volatile private var apiDiscoveryToken = ""
    @Volatile private var apiDiscoveryGeneration = 0L
    private var authenticationToken = ""
    private var clientPeerId = ""
    private var apiRequestNonce = 0L
    private var connectionTargetAddress = ""
    private var connectionTargetLabel = ""
    private var connectionApiPeerId: String? = null
    private var connectionSavedAddress: String? = null
    private val navigationButtons = mutableMapOf<HomePage, TextView>()
    private var lastTouchX = 0f
    private var lastTouchY = 0f
    private var pendingTouchX = 0f
    private var pendingTouchY = 0f
    private var touchTravel = 0f
    private var touchDownAt = 0L
    private var touchMoved = false
    private var maxTouchPointers = 0
    private var twoFingerScrolling = false
    private var touchButtonMask = 0
    private val wheelAccumulator = WheelAccumulator()
    private val pointerTravelTracker = PointerTravelTracker()
    private val dragGesture = TrackpadDragGesture(ViewConfiguration.getDoubleTapTimeout().toLong())

    private val pollStatus = object : Runnable {
        override fun run() {
            val sessionEpoch = currentSessionEpoch
            if (sessionStarted && sessionEpoch != 0L) {
                val status = NativeBridge.nativeGetStatus(nativeHandle)
                val wasConnected = connected
                connected = status == "connected"
                val connectionPending = isConnectionPending(status)
                connectionStatus = status
                if (connected && !wasConnected) {
                    markServerConnected()
                }
                if (!connected && audioPlayer != null) {
                    stopAudio()
                }
                if (!connected && wasConnected) {
                    cancelTouchInput()
                }
                if (!connected && !connectionPending && keyboardOpen) {
                    closeRemoteKeyboard()
                }
                if (!connectionPending && !connected) {
                    cursorOverlay.clearSession()
                }
            } else {
                connected = false
            }
            if (sessionStarted && sessionEpoch != 0L && surfaceReady) {
                NativeBridge.nativeTakeStreamConfig(nativeHandle, sessionEpoch)
                    ?.let(StreamDescription::from)
                    ?.takeIf { currentSessionEpoch == sessionEpoch }
                    ?.let(::applyStreamConfig)
            }
            if (pendingDecoderStream != null) retryDecoderTransition()
            if (connected) activeStream?.let(::startAudio)
            updateStatusText()
            if (SystemClock.uptimeMillis() >= nextDiscoveryRefresh) {
                refreshDiscoveredServers()
                nextDiscoveryRefresh = SystemClock.uptimeMillis() + 1_000
            }
            handler.postDelayed(this, 100)
        }
    }

    private val pollCursor = object : Runnable {
        override fun run() {
            val sessionEpoch = currentSessionEpoch
            if (sessionStarted && sessionEpoch != 0L) {
                NativeBridge.nativePollCursorSnapshot(nativeHandle, sessionEpoch)
                    ?.let(CursorSnapshotUpdate::from)
                    ?.takeIf { currentSessionEpoch == sessionEpoch }
                    ?.let(::applyControlSnapshot)
            }
            handler.postDelayed(this, CURSOR_POLL_MS)
        }
    }

    @Suppress("DEPRECATION")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                handleBackNavigation()
            }
        })
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            window.attributes = window.attributes.apply {
                layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
            }
        }
        window.setSoftInputMode(
            WindowManager.LayoutParams.SOFT_INPUT_STATE_ALWAYS_HIDDEN or
                WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE,
        )
        loadSettings()
        savedServers = loadSavedServers().toMutableList()
        persistSavedServers()
        buildUi()
        applyKeepAwake()
        showHome(HomePage.SERVERS)
    }

    override fun onResume() {
        super.onResume()
        if (isStreamingVisible()) {
            applyStreamingFullscreen()
        }
    }

    override fun onPause() {
        cancelTouchInput()
        if (keyboardOpen) closeRemoteKeyboard()
        super.onPause()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && isStreamingVisible()) {
            applyStreamingFullscreen()
        } else if (!hasFocus) {
            cancelTouchInput()
            if (keyboardOpen) closeRemoteKeyboard()
        }
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (keyboardOpen && ::keyboardLauncher.isInitialized && keyboardLauncher.handleKeyEvent(event)) {
            return true
        }
        return super.onKeyDown(keyCode, event)
    }

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean {
        if (keyboardOpen && ::keyboardLauncher.isInitialized && keyboardLauncher.handleKeyEvent(event)) {
            return true
        }
        return super.onKeyUp(keyCode, event)
    }

    override fun onKeyMultiple(keyCode: Int, repeatCount: Int, event: KeyEvent): Boolean {
        if (keyboardOpen && ::keyboardLauncher.isInitialized && keyboardLauncher.handleKeyEvent(event)) {
            return true
        }
        return super.onKeyMultiple(keyCode, repeatCount, event)
    }

    private fun handleBackNavigation() {
        when {
            menuOpen -> closeFloatingMenu()
            keyboardOpen -> closeRemoteKeyboard()
            isStreamingVisible() -> disconnect()
            else -> finishAfterTransition()
        }
    }

    override fun onStart() {
        super.onStart()
        acquireDiscoveryLock()
        NativeBridge.nativeSetDiscoveryEnabled(nativeHandle, true)
        startApiDiscovery()
        handler.post(pollStatus)
        handler.post(pollCursor)
    }

    override fun onStop() {
        stopApiDiscovery()
        handler.removeCallbacks(pollStatus)
        handler.removeCallbacks(pollCursor)
        NativeBridge.nativeSetDiscoveryEnabled(nativeHandle, false)
        releaseDiscoveryLock()
        disconnect()
        super.onStop()
    }

    override fun onDestroy() {
        cancelTouchInput()
        stopApiDiscovery()
        apiDiscoveryExecutor.shutdownNow()
        handler.removeCallbacks(pollStatus)
        handler.removeCallbacks(pollCursor)
        currentSessionEpoch = 0L
        sessionStarted = false
        stopAudio()
        decoder?.stop()
        decoder = null
        legacyImeLayoutListener?.let { listener ->
            if (::rootContainer.isInitialized && rootContainer.viewTreeObserver.isAlive) {
                rootContainer.viewTreeObserver.removeOnGlobalLayoutListener(listener)
            }
        }
        legacyImeLayoutListener = null
        Thread({ NativeBridge.nativeDestroy(nativeHandle) }, "st-native-destroy").start()
        super.onDestroy()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        surfaceReady = true
        if (pendingConnect) {
            startNativeConnection()
        }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) = Unit

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        surfaceReady = false
        if (sessionStarted || pendingConnect) {
            disconnect()
        }
    }

    private fun buildUi() {
        rootContainer = FrameLayout(this).apply {
            setBackgroundColor(COLOR_BACKGROUND)
        }
        navigation = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.TOP or Gravity.CENTER_HORIZONTAL
            setPadding(0, dp(12), 0, 0)
            setBackgroundColor(COLOR_SIDEBAR)
            addView(navigationButton("S", "Servers", HomePage.SERVERS))
            addView(navigationButton("G", "Settings", HomePage.SETTINGS))
            addView(navigationButton("U", "Update", HomePage.UPDATE))
            addView(navigationButton("?", "About", HomePage.ABOUT))
        }
        serversPanel = buildServersPanel()
        settingsPanel = buildSettingsPanel()
        updatePanel = buildUpdatePanel()
        aboutPanel = buildAboutPanel()
        val panelStack = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(serversPanel)
            addView(settingsPanel)
            addView(updatePanel)
            addView(aboutPanel)
        }
        homeScroll = ScrollView(this).apply {
            isFillViewport = true
            setBackgroundColor(COLOR_BACKGROUND)
            addView(panelStack)
        }
        addServerBar = buildAddServerBar()
        homeContainer = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(navigation, LinearLayout.LayoutParams(dp(56), ViewGroup.LayoutParams.MATCH_PARENT))
            addView(LinearLayout(this@MainActivity).apply {
                orientation = LinearLayout.VERTICAL
                addView(homeScroll, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
                addView(addServerBar, rowParams())
            }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.MATCH_PARENT, 1f))
        }
        videoView = AspectSurfaceView(this).apply {
            setScaleMode(videoScaleMode)
            holder.addCallback(this@MainActivity)
        }
        cursorOverlay = CursorOverlayView(this, videoView)
        videoView.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> cursorOverlay.invalidate() }
        statusPanel = buildStatusPanel()
        floatingMenu = buildFloatingMenu()
        menuLauncher = buildMenuLauncher()
        keyboardLauncher = RemoteKeyboardView(
            this,
            publish = ::publishKeyboardState,
            publishText = ::publishTextInput,
            dismiss = { closeRemoteKeyboard(hideIme = false) },
            unsupportedText = { count ->
                Toast.makeText(
                    this,
                    "$count character(s) cannot be entered remotely; Unicode requires server text-input support",
                    Toast.LENGTH_SHORT,
                ).show()
            },
        ).apply {
            visibility = View.GONE
            setAvailable(false)
            setOnClickListener { toggleRemoteKeyboard() }
        }
        streamArea = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            clipChildren = true
            clipToPadding = true
            isClickable = true
            setOnTouchListener { _, event -> handleStreamTouch(event) }
            visibility = View.GONE
            addView(
                videoView,
                FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    Gravity.CENTER,
                ),
            )
            addView(
                cursorOverlay,
                FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    Gravity.CENTER,
                ),
            )
            addView(
                statusPanel,
                FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.WRAP_CONTENT,
                    ViewGroup.LayoutParams.WRAP_CONTENT,
                    Gravity.CENTER,
                ),
            )
            addView(
                floatingMenu,
                FrameLayout.LayoutParams(dp(300), ViewGroup.LayoutParams.WRAP_CONTENT),
            )
            addView(
                menuLauncher,
                FrameLayout.LayoutParams(dp(MENU_TOUCH_DP), dp(MENU_TOUCH_DP)),
            )
            addView(
                keyboardLauncher,
                FrameLayout.LayoutParams(dp(MENU_TOUCH_DP), dp(MENU_TOUCH_DP)),
            )
            addOnLayoutChangeListener { _, left, top, right, bottom, oldLeft, oldTop, oldRight, oldBottom ->
                if (right - left != oldRight - oldLeft || bottom - top != oldBottom - oldTop) {
                    positionMenuLauncher()
                    positionKeyboardLauncher()
                    if (menuOpen) positionFloatingMenu()
                }
            }
        }

        rootContainer.addView(
            homeContainer,
            FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT),
        )
        rootContainer.addView(
            streamArea,
            FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT),
        )
        setContentView(rootContainer)
        installImeInsetsListener()
    }

    private fun buildStatusPanel() = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        gravity = Gravity.CENTER
        setPadding(dp(20), dp(16), dp(20), dp(16))
        background = roundedBackground(Color.argb(210, 25, 29, 36), 14)
        isClickable = true
        elevation = dp(8).toFloat()
        statusText = TextView(this@MainActivity).apply {
            setTextColor(Color.WHITE)
            textSize = 16f
            gravity = Gravity.CENTER
            maxWidth = dp(440)
            text = "preparing video surface"
            setPadding(dp(4), 0, dp(4), dp(10))
        }
        addView(statusText, rowParams())
        addView(Button(this@MainActivity).apply {
            text = "Cancel"
            setOnClickListener { disconnect() }
        }, LinearLayout.LayoutParams(dp(140), dp(48)))
    }

    private fun buildFloatingMenu() = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(dp(16), dp(14), dp(16), dp(14))
        background = roundedBackground(Color.argb(236, 24, 28, 35), 14)
        isClickable = true
        elevation = dp(10).toFloat()
        visibility = View.GONE
        menuStatusText = TextView(this@MainActivity).apply {
            setTextColor(Color.WHITE)
            textSize = 14f
            setPadding(dp(2), 0, dp(2), dp(8))
        }
        addView(menuStatusText, rowParams())
        addView(Button(this@MainActivity).apply {
            text = "Disconnect"
            setOnClickListener { disconnect() }
        }, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(48)))
        menuAudioToggle = CheckBox(this@MainActivity).apply {
            text = "Audio"
            setTextColor(Color.WHITE)
            isChecked = audioEnabled
            setOnCheckedChangeListener { _, enabled ->
                if (!syncingControls) setAudioEnabled(enabled)
            }
        }
        addView(menuAudioToggle, rowParams())
        menuImmersiveToggle = CheckBox(this@MainActivity).apply {
            text = "Immersive fullscreen"
            setTextColor(Color.WHITE)
            isChecked = immersiveFullscreenEnabled
            setOnCheckedChangeListener { _, enabled ->
                if (!syncingControls) setImmersiveFullscreenEnabled(enabled)
            }
        }
        addView(menuImmersiveToggle, rowParams())
        addView(label("Video scaling"))
        menuScaleSpinner = videoScaleSpinner()
        addView(menuScaleSpinner, rowParams())
    }

    private fun buildMenuLauncher() = FrameLayout(this).apply {
        visibility = View.GONE
        isClickable = true
        isFocusable = true
        contentDescription = "Streaming menu"
        elevation = dp(12).toFloat()
        addView(
            TextView(this@MainActivity).apply {
                text = "M"
                gravity = Gravity.CENTER
                setTextColor(Color.WHITE)
                textSize = 18f
                background = roundedBackground(Color.argb(220, 31, 36, 45), 20, Color.argb(180, 255, 255, 255))
            },
            FrameLayout.LayoutParams(dp(MENU_VISUAL_DP), dp(MENU_VISUAL_DP), Gravity.CENTER),
        )
        setOnClickListener { toggleFloatingMenu() }
        setOnTouchListener { _, event -> handleMenuLauncherTouch(event) }
    }

    private fun buildServersPanel(): LinearLayout {
        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(28), dp(22), dp(28), dp(24))
            val heading = LinearLayout(this@MainActivity).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                addView(LinearLayout(this@MainActivity).apply {
                    orientation = LinearLayout.VERTICAL
                    addView(title("Computers"))
                    addView(description("Connect to your computer in low latency desktop mode."))
                }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
                apiStatusText = TextView(this@MainActivity).apply {
                    textSize = 12f
                    gravity = Gravity.CENTER_VERTICAL
                }
                addView(apiStatusText, LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, dp(40)))
            }
            addView(heading, rowParams())
            val searchRow = LinearLayout(this@MainActivity).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(0, dp(18), 0, dp(14))
                searchInput = textInput("Search Hosts and Computers", "").apply {
                    addTextChangedListener(simpleTextWatcher { refreshServerCards() })
                }
                addView(searchInput, LinearLayout.LayoutParams(0, dp(52), 1f))
                addView(Button(this@MainActivity).apply {
                    text = "Reload"
                    setOnClickListener { reloadServers() }
                }, LinearLayout.LayoutParams(dp(112), dp(48)).apply { marginStart = dp(8) })
            }
            addView(searchRow, rowParams())
            serverCardsContainer = LinearLayout(this@MainActivity).apply {
                orientation = LinearLayout.VERTICAL
            }
            addView(serverCardsContainer, rowParams())
            homeStatusText = TextView(this@MainActivity).apply {
                setTextColor(Color.LTGRAY)
                setPadding(dp(4), dp(12), dp(4), dp(4))
            }
            addView(homeStatusText, rowParams())
            post(::refreshServerCards)
        }
    }

    private fun buildAddServerBar() = LinearLayout(this).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(20), dp(7), dp(20), dp(7))
        setBackgroundColor(Color.rgb(30, 34, 42))
        addView(description("Add server by address."))
        addServerInput = textInput("IP or host[:port]", "", InputType.TYPE_TEXT_VARIATION_URI)
        addView(addServerInput, LinearLayout.LayoutParams(0, dp(46), 1f).apply { marginStart = dp(12) })
        addView(Button(this@MainActivity).apply {
            text = "Add"
            setOnClickListener { addServerByAddress() }
        }, LinearLayout.LayoutParams(dp(76), dp(44)).apply { marginStart = dp(6) })
    }

    private fun buildSettingsPanel(): LinearLayout {
        val preferences = getSharedPreferences(PREFERENCES, MODE_PRIVATE)
        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
            setPadding(dp(28), dp(22), dp(28), dp(24))
            addView(title("Settings"))
            addView(description("Configure authentication, streaming, audio, and Android controls."))
            addView(label("Authentication"))
            tokenInput = textInput(
                "Shared server token",
                authenticationToken,
                InputType.TYPE_TEXT_VARIATION_PASSWORD or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS,
            ).apply {
                transformationMethod = PasswordTransformationMethod.getInstance()
                isSaveEnabled = false
                imeOptions = imeOptions or EditorInfo.IME_FLAG_NO_EXTRACT_UI
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    imeOptions = imeOptions or EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
                    importantForAutofill = View.IMPORTANT_FOR_AUTOFILL_NO_EXCLUDE_DESCENDANTS
                    setAutofillHints(null)
                }
                addTextChangedListener(simpleTextWatcher { value -> setAuthenticationToken(value.trim()) })
            }
            addView(tokenInput, rowParams())
            addView(description("The same token is used for saved servers, LAN discovery, and API presence."))
            addView(label("Streaming"))
            val rates = listOf(30, 60, 90, 120)
            val rateSpinner = Spinner(this@MainActivity).apply {
                adapter = ArrayAdapter(
                    this@MainActivity,
                    android.R.layout.simple_spinner_dropdown_item,
                    rates.map { "$it Hz" },
                )
                setSelection(rates.indexOf(refreshRateHz).coerceAtLeast(0))
                onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                    override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                        refreshRateHz = rates[position]
                        preferences.edit().putInt("refresh_hz", refreshRateHz).apply()
                    }

                    override fun onNothingSelected(parent: AdapterView<*>?) = Unit
                }
            }
            addView(label("Maximum refresh rate"))
            addView(rateSpinner, rowParams())
            addView(label("Video scaling"))
            settingsScaleSpinner = videoScaleSpinner()
            addView(settingsScaleSpinner, rowParams())
            addView(CheckBox(this@MainActivity).apply {
                text = "Keep screen awake while st is open"
                setTextColor(Color.WHITE)
                isChecked = preferences.getBoolean("keep_awake", true)
                setOnCheckedChangeListener { _, enabled ->
                    preferences.edit().putBoolean("keep_awake", enabled).apply()
                    applyKeepAwake()
                }
            })
            settingsAudioToggle = CheckBox(this@MainActivity).apply {
                text = "Enable audio playback"
                setTextColor(Color.WHITE)
                isChecked = audioEnabled
                setOnCheckedChangeListener { _, enabled ->
                    if (!syncingControls) setAudioEnabled(enabled)
                }
            }
            addView(settingsAudioToggle)
            settingsImmersiveToggle = CheckBox(this@MainActivity).apply {
                text = "Immersive fullscreen while streaming"
                setTextColor(Color.WHITE)
                isChecked = immersiveFullscreenEnabled
                setOnCheckedChangeListener { _, enabled ->
                    if (!syncingControls) setImmersiveFullscreenEnabled(enabled)
                }
            }
            addView(settingsImmersiveToggle)
            addView(CheckBox(this@MainActivity).apply {
                text = "Enable touch mouse input"
                setTextColor(Color.WHITE)
                isChecked = touchEnabled
                setOnCheckedChangeListener { _, enabled ->
                    touchEnabled = enabled
                    if (!enabled) cancelTouchInput()
                    preferences.edit().putBoolean("touch_enabled", enabled).apply()
                }
            })
            val sensitivities = listOf(0.75f, 1f, 1.5f, 2f)
            addView(label("Trackpad sensitivity"))
            addView(Spinner(this@MainActivity).apply {
                adapter = ArrayAdapter(
                    this@MainActivity,
                    android.R.layout.simple_spinner_dropdown_item,
                    sensitivities.map { "${it}x" },
                )
                setSelection(sensitivities.indexOf(touchSensitivity).coerceAtLeast(0))
                onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                    override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                        touchSensitivity = sensitivities[position]
                        preferences.edit().putFloat("touch_sensitivity", touchSensitivity).apply()
                    }

                    override fun onNothingSelected(parent: AdapterView<*>?) = Unit
                }
            }, rowParams())
            addView(TextView(this@MainActivity).apply {
                setTextColor(Color.GRAY)
                text = "Android uses direct LAN TCP/UDP when available, then API-signaled encrypted UDP punching with an encrypted TCP relay fallback."
                setPadding(dp(4), dp(16), dp(4), dp(4))
            })
        }
    }

    private fun buildUpdatePanel(): LinearLayout = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        visibility = View.GONE
        setPadding(dp(28), dp(22), dp(28), dp(24))
        addView(title("Update"))
        addView(description("Keep the Android client aligned with the server and shared protocol."))
        addView(infoCard("Current version", "0.1.0"))
        addView(infoCard("Update channel", "Manual APK installation"))
        addView(description("Automatic self-update is not available on Android. Install a newer signed APK over this app to retain settings."))
    }

    private fun buildAboutPanel(): LinearLayout = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        visibility = View.GONE
        setPadding(dp(28), dp(22), dp(28), dp(24))
        addView(title("About"))
        addView(description("st Android 0.1.0"))
        addView(infoCard("Presentation", "Android MediaCodec to SurfaceView"))
        addView(infoCard("Video", "Hardware H.264, SDR, YUV420"))
        addView(infoCard("Audio", "48 kHz stereo Opus with redundancy, FEC, and PLC"))
        addView(infoCard("Transport", "Direct IPv4 or encrypted API tunnel"))
        addView(description("Built with the shared Rust session core for low-latency desktop streaming."))
    }

    private fun navigationButton(symbol: String, name: String, page: HomePage) = TextView(this).apply {
        text = symbol
        gravity = Gravity.CENTER
        textSize = 17f
        isClickable = true
        isFocusable = true
        contentDescription = name
        setOnClickListener { showHome(page) }
        navigationButtons[page] = this
        layoutParams = LinearLayout.LayoutParams(dp(56), dp(48))
    }

    private fun showHome(page: HomePage = currentPage) {
        currentPage = page
        closeFloatingMenu()
        menuLauncher.visibility = View.GONE
        homeContainer.visibility = View.VISIBLE
        navigation.visibility = View.VISIBLE
        homeScroll.visibility = View.VISIBLE
        serversPanel.visibility = if (page == HomePage.SERVERS) View.VISIBLE else View.GONE
        settingsPanel.visibility = if (page == HomePage.SETTINGS) View.VISIBLE else View.GONE
        updatePanel.visibility = if (page == HomePage.UPDATE) View.VISIBLE else View.GONE
        aboutPanel.visibility = if (page == HomePage.ABOUT) View.VISIBLE else View.GONE
        addServerBar.visibility = if (page == HomePage.SERVERS) View.VISIBLE else View.GONE
        navigationButtons.forEach { (buttonPage, button) ->
            val selected = buttonPage == page
            button.setTextColor(if (selected) COLOR_ACCENT else COLOR_TEXT_MUTED)
            button.background = if (selected) {
                roundedBackground(Color.argb(36, 90, 200, 250), 0, COLOR_ACCENT)
            } else {
                null
            }
        }
        streamArea.visibility = View.GONE
        exitImmersiveMode()
        rootContainer.requestApplyInsets()
    }

    private fun showStreaming() {
        homeContainer.visibility = View.GONE
        navigation.visibility = View.GONE
        homeScroll.visibility = View.GONE
        serversPanel.visibility = View.GONE
        settingsPanel.visibility = View.GONE
        updatePanel.visibility = View.GONE
        aboutPanel.visibility = View.GONE
        addServerBar.visibility = View.GONE
        streamArea.visibility = View.VISIBLE
        applyStreamingFullscreen()
        rootContainer.requestApplyInsets()
        streamArea.post(::positionMenuLauncher)
        updateStatusText()
    }

    private fun connectToServer(card: ServerCard) {
        val server = card.connectAddress
        val tunnelPeerId = card.tunnelPeerId
        if (server == null && tunnelPeerId == null) {
            homeStatusText.text = "This server has no usable connection path"
            return
        }
        if (authenticationToken.isEmpty()) {
            homeStatusText.text = "Set the authentication token in Settings before connecting"
            return
        }
        getSharedPreferences(PREFERENCES, MODE_PRIVATE)
            .edit()
            .putString("server", server.orEmpty())
            .apply()
        connectionTargetAddress = server.orEmpty()
        connectionTargetLabel = server ?: "API tunnel to ${card.displayName}"
        connectionApiPeerId = tunnelPeerId
        connectionSavedAddress = card.savedAddress
        keyboardCapability = false
        textInputCapability = false
        controllerOwnership = ControllerOwnership.UNAVAILABLE
        resetFloatingControls()
        pendingConnect = true
        connected = false
        connectionStatus = "preparing video surface"
        decoderStatus = ""
        audioStatus = ""
        showStreaming()
        if (surfaceReady) {
            startNativeConnection()
        }
    }

    private fun startNativeConnection() {
        if (!pendingConnect || !surfaceReady) {
            return
        }
        pendingConnect = false
        cursorOverlay.clearSession()
        controlTransportGeneration = 0
        currentSessionEpoch = 0L
        sessionStarted = false
        val error = NativeBridge.nativeStart(
            nativeHandle,
            connectionTargetAddress,
            authenticationToken,
            refreshRateHz * 1_000,
            if (connectionApiPeerId != null) ApiDiscovery.BASE_URL else "",
            if (connectionApiPeerId != null) clientPeerId else "",
            connectionApiPeerId.orEmpty(),
            if (connectionApiPeerId != null) nextApiRequestNonce() else 0L,
        )
        if (error != null) {
            homeStatusText.text = error
            connectionStatus = error
            updateStatusText()
            return
        }
        val sessionEpoch = NativeBridge.nativeGetSessionEpoch(nativeHandle)
        if (sessionEpoch == 0L) {
            NativeBridge.nativeStop(nativeHandle)
            connectionStatus = "native session did not start"
            updateStatusText()
            return
        }
        currentSessionEpoch = sessionEpoch
        sessionStarted = true
        connectionStatus = "connecting"
        updateStatusText()
    }

    private fun disconnect() {
        pendingConnect = false
        resetFloatingControls()
        val wasStarted = sessionStarted
        currentSessionEpoch = 0L
        sessionStarted = false
        stopAudio()
        decoder?.stop()
        decoder = null
        decoderStatus = ""
        activeStream = null
        pendingDecoderStream = null
        decoderTransitionStartedAt = 0L
        streamGeneration = 0
        controlTransportGeneration = 0
        audioAttemptGeneration = 0
        connected = false
        connectionStatus = "disconnected"
        connectionSavedAddress = null
        connectionApiPeerId = null
        connectionTargetLabel = ""
        keyboardCapability = false
        textInputCapability = false
        controllerOwnership = ControllerOwnership.UNAVAILABLE
        if (::cursorOverlay.isInitialized) {
            cursorOverlay.clearSession()
        }
        if (wasStarted) {
            NativeBridge.nativeStop(nativeHandle)
        }
        if (::navigation.isInitialized) {
            showHome(currentPage)
        }
    }

    private fun applyStreamConfig(stream: StreamDescription) {
        val sessionEpoch = currentSessionEpoch
        if (!sessionStarted || sessionEpoch == 0L) return
        val previous = activeStream
        val plan = planStreamUpdate(
            previous,
            stream,
            decoderReady = decoder != null && pendingDecoderStream == null,
        )
        val decoderChanged = plan.restartDecoder || stream.generation != streamGeneration
        if (plan.restartAudio) {
            stopAudio()
            audioAttemptGeneration = 0
        } else if (plan.resetAudioDecoder) {
            audioPlayer?.resetForTransport()
        }
        activeStream = stream
        videoView.setVideoSize(stream.width, stream.height)
        cursorOverlay.updateStreamConfig(
            stream.generation,
            stream.width,
            stream.height,
            stream.cursorWidth,
            stream.cursorHeight,
        )
        if (decoderChanged) {
            if (pendingDecoderStream?.generation != stream.generation) {
                decoderTransitionStartedAt = SystemClock.uptimeMillis()
            }
            pendingDecoderStream = stream
            decoderStatus = "starting decoder"
            retryDecoderTransition()
        }
        if (plan.restartAudio) startAudio(stream)
        updateStatusText()
    }

    private fun retryDecoderTransition() {
        val stream = pendingDecoderStream ?: return
        val sessionEpoch = currentSessionEpoch
        if (!sessionStarted || sessionEpoch == 0L) return
        if (SystemClock.uptimeMillis() - decoderTransitionStartedAt >= DECODER_TRANSITION_TIMEOUT_MS) {
            decoderStatus = "decoder restart timed out; reconnect required"
            updateStatusText()
            disconnect()
            return
        }
        if (decoder?.stop() == false) {
            decoderStatus = "waiting for previous decoder to stop"
            updateStatusText()
            return
        }
        decoder = null
        if (streamGeneration != stream.generation) {
            if (!NativeBridge.nativeAcceptStreamGeneration(nativeHandle, sessionEpoch, stream.generation)) {
                decoderStatus = "waiting to accept video configuration"
                updateStatusText()
                return
            }
            streamGeneration = stream.generation
        }
        val next = AvcSurfaceDecoder(nativeHandle, sessionEpoch, stream, videoView.holder.surface) { status ->
            runOnUiThread {
                if (currentSessionEpoch == sessionEpoch && streamGeneration == stream.generation) {
                    decoderStatus = status
                    updateStatusText()
                }
            }
        }
        try {
            next.start()
            decoder = next
            pendingDecoderStream = null
            decoderTransitionStartedAt = 0L
        } catch (error: RuntimeException) {
            next.stop()
            NativeBridge.nativeRequestKeyframe(nativeHandle, sessionEpoch)
            decoderStatus = "decoder start failed; retrying: ${error.message ?: error.javaClass.simpleName}"
            updateStatusText()
        }
    }

    private fun startAudio(stream: StreamDescription) {
        val sessionEpoch = currentSessionEpoch
        if (!audioEnabled || !connected || !sessionStarted || audioPlayer != null ||
            sessionEpoch == 0L || audioAttemptGeneration == stream.generation
        ) {
            return
        }
        audioAttemptGeneration = stream.generation
        audioStatus = "starting audio"
        updateStatusText()
        val player = AndroidAudioPlayer(nativeHandle, sessionEpoch, stream) { status ->
            runOnUiThread {
                if (sessionStarted && currentSessionEpoch == sessionEpoch &&
                    activeStream?.generation == stream.generation &&
                    activeStream?.audioCompatibleWith(stream) == true
                ) {
                    audioStatus = status
                    updateStatusText()
                }
            }
        }
        audioPlayer = player
        player.start()?.let { error ->
            audioPlayer = null
            audioStatus = error
            updateStatusText()
        }
    }

    private fun stopAudio() {
        val player = audioPlayer
        audioPlayer = null
        player?.stop()
        audioStatus = ""
    }

    private fun updateStatusText() {
        if (!::statusText.isInitialized) return
        val decoderDetail = decoderStatus.ifEmpty {
            if (connected) "waiting for video" else "not started"
        }
        val targetDetail = connectionTargetLabel.takeIf { !connected && it.isNotEmpty() }
        statusText.text = listOfNotNull(connectionStatus, targetDetail, decoderDetail)
            .filter { it.isNotEmpty() && it != "not started" }
            .distinct()
            .joinToString("\n")

        if (::menuStatusText.isInitialized) {
            val audioDetail = when {
                !audioEnabled -> "disabled"
                audioStatus.isNotEmpty() -> audioStatus
                activeStream == null -> "waiting for stream"
                else -> "waiting for audio"
            }
            menuStatusText.text = "Connection: $connectionStatus\nDecoder: $decoderDetail\nAudio: $audioDetail"
        }

        val streaming = isStreamingVisible()
        statusPanel.visibility = if (streaming && !(connected && decoderStatus == VIDEO_ACTIVE_STATUS)) {
            View.VISIBLE
        } else {
            View.GONE
        }
        val showLauncher = streaming && connected
        if (showLauncher) {
            if (menuLauncher.visibility != View.VISIBLE) {
                menuLauncher.visibility = View.VISIBLE
                menuLauncher.post(::positionMenuLauncher)
            }
        } else {
            closeFloatingMenu()
            menuLauncher.visibility = View.GONE
        }
        updateKeyboardLauncher()
    }

    private fun applyControlSnapshot(update: CursorSnapshotUpdate) {
        if (controlTransportGeneration != update.transportGeneration) {
            controlTransportGeneration = update.transportGeneration
            keyboardCapability = false
            textInputCapability = false
            controllerOwnership = ControllerOwnership.UNAVAILABLE
        }
        update.capabilities?.let {
            keyboardCapability = it.value.keyboard
            textInputCapability = it.value.textInput
        }
        update.controllerState?.let { controllerOwnership = it.value }
        cursorOverlay.applyUpdate(update)
        updateKeyboardLauncher()
    }

    private fun keyboardInputEligible(): Boolean = connected && keyboardCapability &&
        (controllerOwnership == ControllerOwnership.AVAILABLE ||
            controllerOwnership == ControllerOwnership.OWNED_BY_YOU)

    private fun updateKeyboardLauncher() {
        if (!::keyboardLauncher.isInitialized) return
        val available = keyboardInputEligible()
        keyboardLauncher.setAvailable(available)
        keyboardLauncher.setTextInputAvailable(available && textInputCapability)
        keyboardLauncher.visibility = if (connected && keyboardCapability && isStreamingVisible()) {
            View.VISIBLE
        } else {
            View.GONE
        }
        if (!available && keyboardOpen) closeRemoteKeyboard()
        keyboardLauncher.post(::positionKeyboardLauncher)
    }

    private fun setAudioEnabled(enabled: Boolean) {
        audioEnabled = enabled
        getSharedPreferences(PREFERENCES, MODE_PRIVATE).edit().putBoolean("audio_enabled", enabled).apply()
        syncingControls = true
        try {
            if (::settingsAudioToggle.isInitialized) settingsAudioToggle.isChecked = enabled
            if (::menuAudioToggle.isInitialized) menuAudioToggle.isChecked = enabled
        } finally {
            syncingControls = false
        }
        if (enabled) {
            audioAttemptGeneration = 0
            activeStream?.let(::startAudio)
        } else {
            stopAudio()
        }
        updateStatusText()
    }

    private fun setImmersiveFullscreenEnabled(enabled: Boolean) {
        immersiveFullscreenEnabled = enabled
        getSharedPreferences(PREFERENCES, MODE_PRIVATE)
            .edit()
            .putBoolean("immersive_fullscreen", enabled)
            .apply()
        syncingControls = true
        try {
            if (::settingsImmersiveToggle.isInitialized) settingsImmersiveToggle.isChecked = enabled
            if (::menuImmersiveToggle.isInitialized) menuImmersiveToggle.isChecked = enabled
        } finally {
            syncingControls = false
        }
        if (isStreamingVisible()) {
            applyStreamingFullscreen()
            rootContainer.requestApplyInsets()
        }
    }

    private fun setVideoScaleMode(mode: VideoScaleMode) {
        videoScaleMode = mode
        getSharedPreferences(PREFERENCES, MODE_PRIVATE).edit().putString("video_scale", mode.name).apply()
        if (::videoView.isInitialized) {
            videoView.setScaleMode(mode)
            cursorOverlay.invalidate()
        }
        val position = VideoScaleMode.values().indexOf(mode)
        syncingControls = true
        try {
            if (::settingsScaleSpinner.isInitialized && settingsScaleSpinner.selectedItemPosition != position) {
                settingsScaleSpinner.setSelection(position)
            }
            if (::menuScaleSpinner.isInitialized && menuScaleSpinner.selectedItemPosition != position) {
                menuScaleSpinner.setSelection(position)
            }
        } finally {
            syncingControls = false
        }
    }

    private fun videoScaleSpinner() = Spinner(this).apply {
        val modes = VideoScaleMode.values()
        adapter = ArrayAdapter(
            this@MainActivity,
            android.R.layout.simple_spinner_dropdown_item,
            modes.map(VideoScaleMode::label),
        )
        setSelection(modes.indexOf(videoScaleMode))
        onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                if (!syncingControls) setVideoScaleMode(modes[position])
            }

            override fun onNothingSelected(parent: AdapterView<*>?) = Unit
        }
    }

    private fun handleStreamTouch(event: MotionEvent): Boolean {
        if (event.actionMasked == MotionEvent.ACTION_DOWN && keyboardOpen) {
            closeRemoteKeyboard()
            consumeDismissTouch = true
            cancelTouchInput()
            return true
        }
        if (event.actionMasked == MotionEvent.ACTION_DOWN && menuOpen) {
            closeFloatingMenu()
            consumeDismissTouch = true
            cancelTouchInput()
            return true
        }
        if (consumeDismissTouch) {
            if (event.actionMasked == MotionEvent.ACTION_UP || event.actionMasked == MotionEvent.ACTION_CANCEL) {
                consumeDismissTouch = false
            }
            return true
        }
        return sendTouch(event)
    }

    private fun handleMenuLauncherTouch(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                launcherDownRawX = event.rawX
                launcherDownRawY = event.rawY
                launcherStartX = menuLauncher.x
                launcherStartY = menuLauncher.y
                launcherDragging = false
            }

            MotionEvent.ACTION_MOVE -> {
                val dx = event.rawX - launcherDownRawX
                val dy = event.rawY - launcherDownRawY
                if (!launcherDragging && hypot(dx, dy) > ViewConfiguration.get(this).scaledTouchSlop) {
                    launcherDragging = true
                    closeFloatingMenu()
                    closeRemoteKeyboard()
                }
                if (launcherDragging) {
                    val maxX = max(0f, (streamArea.width - menuLauncher.width).toFloat())
                    val maxY = max(0f, (streamArea.height - menuLauncher.height).toFloat())
                    menuLauncher.x = (launcherStartX + dx).coerceIn(0f, maxX)
                    menuLauncher.y = (launcherStartY + dy).coerceIn(0f, maxY)
                    positionKeyboardLauncher()
                }
            }

            MotionEvent.ACTION_UP -> {
                if (launcherDragging) {
                    persistMenuLauncherPosition()
                } else {
                    menuLauncher.performClick()
                }
                launcherDragging = false
            }

            MotionEvent.ACTION_CANCEL -> {
                if (launcherDragging) persistMenuLauncherPosition()
                launcherDragging = false
            }
        }
        return true
    }

    private fun toggleFloatingMenu() {
        if (menuOpen) {
            closeFloatingMenu()
            return
        }
        if (!connected) return
        if (keyboardOpen) closeRemoteKeyboard()
        menuOpen = true
        updateStatusText()
        floatingMenu.visibility = View.VISIBLE
        floatingMenu.post(::positionFloatingMenu)
    }

    private fun closeFloatingMenu() {
        menuOpen = false
        if (::floatingMenu.isInitialized) floatingMenu.visibility = View.GONE
    }

    private fun resetFloatingControls() {
        closeFloatingMenu()
        closeRemoteKeyboard()
        consumeDismissTouch = false
        launcherDragging = false
        cancelTouchInput()
        if (::menuLauncher.isInitialized) menuLauncher.visibility = View.GONE
        if (::keyboardLauncher.isInitialized) keyboardLauncher.visibility = View.GONE
    }

    private fun positionMenuLauncher() {
        if (!::menuLauncher.isInitialized || streamArea.width <= 0 || streamArea.height <= 0) return
        val launcherWidth = menuLauncher.width.takeIf { it > 0 } ?: dp(MENU_TOUCH_DP)
        val launcherHeight = menuLauncher.height.takeIf { it > 0 } ?: dp(MENU_TOUCH_DP)
        val maxX = max(0f, (streamArea.width - launcherWidth).toFloat())
        val maxY = max(0f, (streamArea.height - launcherHeight).toFloat())
        menuLauncher.x = if (launcherNormalizedX.isFinite()) {
            launcherNormalizedX.coerceIn(0f, 1f) * maxX
        } else {
            dp(12).toFloat().coerceAtMost(maxX)
        }
        menuLauncher.y = if (launcherNormalizedY.isFinite()) {
            launcherNormalizedY.coerceIn(0f, 1f) * maxY
        } else {
            dp(12).toFloat().coerceAtMost(maxY)
        }
        positionKeyboardLauncher()
    }

    private fun positionKeyboardLauncher() {
        if (!::keyboardLauncher.isInitialized || keyboardLauncher.visibility != View.VISIBLE ||
            streamArea.width <= 0 || streamArea.height <= 0
        ) {
            return
        }
        val width = keyboardLauncher.width.takeIf { it > 0 } ?: dp(MENU_TOUCH_DP)
        val gap = dp(4).toFloat()
        val right = menuLauncher.x + menuLauncher.width + gap
        keyboardLauncher.x = if (right + width <= streamArea.width) {
            right
        } else {
            (menuLauncher.x - width - gap).coerceAtLeast(0f)
        }
        keyboardLauncher.y = menuLauncher.y.coerceIn(
            0f,
            max(0f, (streamArea.height - keyboardLauncher.height).toFloat()),
        )
    }

    private fun persistMenuLauncherPosition() {
        val maxX = max(0f, (streamArea.width - menuLauncher.width).toFloat())
        val maxY = max(0f, (streamArea.height - menuLauncher.height).toFloat())
        launcherNormalizedX = if (maxX > 0f) (menuLauncher.x / maxX).coerceIn(0f, 1f) else 0f
        launcherNormalizedY = if (maxY > 0f) (menuLauncher.y / maxY).coerceIn(0f, 1f) else 0f
        getSharedPreferences(PREFERENCES, MODE_PRIVATE).edit()
            .putFloat("menu_x", launcherNormalizedX)
            .putFloat("menu_y", launcherNormalizedY)
            .apply()
    }

    private fun positionFloatingMenu() {
        if (!menuOpen || streamArea.width <= 0 || streamArea.height <= 0) return
        val menuWidth = floatingMenu.width.takeIf { it > 0 } ?: dp(300)
        val menuHeight = floatingMenu.height
        val gap = dp(6).toFloat()
        val maxX = max(0f, (streamArea.width - menuWidth).toFloat())
        val maxY = max(0f, (streamArea.height - menuHeight).toFloat())
        floatingMenu.x = menuLauncher.x.coerceIn(0f, maxX)
        val below = menuLauncher.y + menuLauncher.height + gap
        floatingMenu.y = if (below + menuHeight <= streamArea.height) {
            below
        } else {
            (menuLauncher.y - menuHeight - gap).coerceIn(0f, maxY)
        }
    }

    private fun isStreamingVisible(): Boolean =
        ::streamArea.isInitialized && streamArea.visibility == View.VISIBLE

    private fun toggleRemoteKeyboard() {
        if (keyboardOpen) {
            closeRemoteKeyboard()
            return
        }
        if (!keyboardInputEligible()) return
        closeFloatingMenu()
        keyboardOpen = true
        keyboardShowGeneration += 1
        imeShowRequested = false
        keyboardLauncher.setOpen(true)
        requestRemoteKeyboardIme(keyboardShowGeneration, 0)
    }

    private fun closeRemoteKeyboard(hideIme: Boolean = true) {
        if (!::keyboardLauncher.isInitialized || !keyboardOpen) return
        keyboardOpen = false
        keyboardShowGeneration += 1
        imeShowRequested = false
        keyboardLauncher.setOpen(false)
        keyboardLauncher.closeKeyboardSession()
        if (hideIme) {
            val input = getSystemService(INPUT_METHOD_SERVICE) as InputMethodManager
            input.hideSoftInputFromWindow(keyboardLauncher.windowToken, 0)
        }
        keyboardLauncher.clearFocus()
    }

    private fun requestRemoteKeyboardIme(generation: Long, attempt: Int) {
        keyboardLauncher.postDelayed(
            {
                if (!keyboardOpen || generation != keyboardShowGeneration) return@postDelayed
                val focused = keyboardLauncher.hasFocus() || keyboardLauncher.requestFocus()
                if (!focused || !keyboardLauncher.hasWindowFocus()) {
                    if (attempt < IME_SHOW_RETRIES) {
                        requestRemoteKeyboardIme(generation, attempt + 1)
                    } else {
                        closeRemoteKeyboard(hideIme = false)
                    }
                    return@postDelayed
                }
                val input = getSystemService(INPUT_METHOD_SERVICE) as InputMethodManager
                input.restartInput(keyboardLauncher)
                val shownByInsets = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    keyboardLauncher.windowInsetsController?.let { controller ->
                        controller.show(WindowInsets.Type.ime())
                        true
                    } ?: false
                } else {
                    false
                }
                imeShowRequested = input.showSoftInput(
                    keyboardLauncher,
                    InputMethodManager.SHOW_IMPLICIT,
                ) || shownByInsets
                if (!imeShowRequested && attempt < IME_SHOW_RETRIES) {
                    requestRemoteKeyboardIme(generation, attempt + 1)
                } else if (!imeShowRequested) {
                    closeRemoteKeyboard(hideIme = false)
                }
            },
            if (attempt == 0) 0 else IME_SHOW_RETRY_MS,
        )
    }

    private fun publishKeyboardState(pressed: ByteArray): Boolean {
        val epoch = currentSessionEpoch
        if (!connected || epoch == 0L || pressed.size != KEYBOARD_STATE_BYTES) return false
        if (pressed.any { it != 0.toByte() } && !keyboardInputEligible()) return false
        return NativeBridge.nativeSendKeyboardState(nativeHandle, epoch, pressed)
    }

    private fun publishTextInput(text: String): Boolean {
        val epoch = currentSessionEpoch
        if (!connected || epoch == 0L || !textInputCapability || !keyboardInputEligible()) return false
        return NativeBridge.nativeSendTextInput(nativeHandle, epoch, text)
    }

    private fun installImeInsetsListener() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) {
            val visibleFrame = Rect()
            val listener = ViewTreeObserver.OnGlobalLayoutListener {
                rootContainer.getWindowVisibleDisplayFrame(visibleFrame)
                val obscured = rootContainer.rootView.height - visibleFrame.bottom
                val visible = obscured > rootContainer.rootView.height / 4
                val wasVisible = imeVisible
                imeVisible = visible
                if (imeShowRequested && wasVisible && !visible && keyboardOpen) {
                    handler.post { closeRemoteKeyboard(hideIme = false) }
                }
            }
            legacyImeLayoutListener = listener
            rootContainer.viewTreeObserver.addOnGlobalLayoutListener(listener)
            return
        }
        rootContainer.setOnApplyWindowInsetsListener { _, insets ->
            val visible = insets.isVisible(WindowInsets.Type.ime())
            val ime = insets.getInsets(WindowInsets.Type.ime())
            val system = insets.getInsets(
                WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout(),
            )
            val streamInsets = isStreamingVisible() && !immersiveFullscreenEnabled
            val bottom = when {
                !isStreamingVisible() -> 0
                streamInsets -> maxOf(system.bottom, ime.bottom)
                visible -> ime.bottom
                else -> 0
            }
            val params = streamArea.layoutParams as FrameLayout.LayoutParams
            val left = if (streamInsets) system.left else 0
            val top = if (streamInsets) system.top else 0
            val right = if (streamInsets) system.right else 0
            if (params.leftMargin != left || params.topMargin != top ||
                params.rightMargin != right || params.bottomMargin != bottom
            ) {
                params.setMargins(left, top, right, bottom)
                streamArea.layoutParams = params
            }
            if (isStreamingVisible()) {
                homeContainer.setPadding(0, 0, 0, 0)
            } else {
                homeContainer.setPadding(
                    system.left,
                    system.top,
                    system.right,
                    maxOf(system.bottom, ime.bottom),
                )
            }
            val wasVisible = imeVisible
            imeVisible = visible
            if (imeShowRequested && wasVisible && !visible && keyboardOpen) {
                handler.post { closeRemoteKeyboard(hideIme = false) }
            }
            insets
        }
    }

    @Suppress("DEPRECATION")
    private fun applyStreamingFullscreen() {
        if (!immersiveFullscreenEnabled) {
            exitImmersiveMode()
            return
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.setDecorFitsSystemWindows(false)
            window.insetsController?.let { controller ->
                controller.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
                controller.hide(WindowInsets.Type.systemBars())
            }
        } else {
            @Suppress("DEPRECATION")
            window.decorView.systemUiVisibility =
                View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY or
                View.SYSTEM_UI_FLAG_FULLSCREEN or
                View.SYSTEM_UI_FLAG_HIDE_NAVIGATION or
                View.SYSTEM_UI_FLAG_LAYOUT_STABLE or
                View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN or
                View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
        }
    }

    @Suppress("DEPRECATION")
    private fun exitImmersiveMode() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.insetsController?.show(WindowInsets.Type.systemBars())
            window.setDecorFitsSystemWindows(true)
        } else {
            @Suppress("DEPRECATION")
            window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_VISIBLE
        }
    }

    private fun roundedBackground(color: Int, radiusDp: Int, strokeColor: Int? = null) =
        GradientDrawable().apply {
            setColor(color)
            cornerRadius = dp(radiusDp).toFloat()
            if (strokeColor != null) setStroke(dp(1), strokeColor)
        }

    private fun sendTouch(event: MotionEvent): Boolean {
        val sessionEpoch = currentSessionEpoch
        if (!touchEnabled || !sessionStarted || sessionEpoch == 0L || !connected ||
            streamArea.width <= 1 || streamArea.height <= 1
        ) {
            if (event.actionMasked == MotionEvent.ACTION_UP ||
                event.actionMasked == MotionEvent.ACTION_CANCEL
            ) {
                cancelTouchInput()
            }
            return false
        }
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                lastTouchX = event.x
                lastTouchY = event.y
                pendingTouchX = 0f
                pendingTouchY = 0f
                touchTravel = 0f
                touchDownAt = event.eventTime
                touchMoved = false
                maxTouchPointers = 1
                twoFingerScrolling = false
                wheelAccumulator.reset()
                pointerTravelTracker.reset()
                pointerTravelTracker.pointerDown(event.getPointerId(0), event.x, event.y)
                if (dragGesture.pointerDown(
                        event.eventTime,
                        event.x,
                        event.y,
                        ViewConfiguration.get(this).scaledDoubleTapSlop.toFloat(),
                    )
                ) {
                    sendTouchButtonMask(sessionEpoch, MOUSE_BUTTON_LEFT)
                }
            }

            MotionEvent.ACTION_POINTER_DOWN -> {
                if (dragGesture.cancel()) releaseTouchButtons(sessionEpoch)
                maxTouchPointers = maxOf(maxTouchPointers, event.pointerCount)
                lastTouchX = event.centroidX()
                lastTouchY = event.centroidY()
                if (event.pointerCount == 2 && maxTouchPointers == 2) {
                    twoFingerScrolling = false
                    wheelAccumulator.reset()
                    pointerTravelTracker.reset()
                    for (index in 0 until event.pointerCount) {
                        pointerTravelTracker.pointerDown(
                            event.getPointerId(index),
                            event.getX(index),
                            event.getY(index),
                        )
                    }
                } else if (event.pointerCount > 2) {
                    touchMoved = true
                }
                pendingTouchX = 0f
                pendingTouchY = 0f
            }

            MotionEvent.ACTION_MOVE -> {
                maxTouchPointers = maxOf(maxTouchPointers, event.pointerCount)
                val x = event.centroidX()
                val y = event.centroidY()
                val stepX = x - lastTouchX
                val stepY = y - lastTouchY
                lastTouchX = x
                lastTouchY = y
                touchTravel += hypot(stepX, stepY)
                pendingTouchX += stepX
                pendingTouchY += stepY
                if (maxTouchPointers == 2 && event.pointerCount == 2) {
                    var pointerTravel = 0f
                    for (index in 0 until event.pointerCount) {
                        pointerTravel = pointerTravelTracker.pointerMoved(
                            event.getPointerId(index),
                            event.getX(index),
                            event.getY(index),
                        )
                    }
                    if (!twoFingerScrolling && pointerTravel < dp(6)) return true
                    twoFingerScrolling = true
                    touchMoved = true
                    val wheel = wheelAccumulator.addPixels(
                        pendingTouchX,
                        pendingTouchY,
                        resources.displayMetrics.density,
                    )
                    pendingTouchX = 0f
                    pendingTouchY = 0f
                    if (wheel != null) {
                        NativeBridge.nativeSendMouseWheel(
                            nativeHandle,
                            sessionEpoch,
                            wheel.x,
                            wheel.y,
                            touchButtonMask,
                        )
                    }
                    return true
                }
                if (maxTouchPointers != 1) {
                    if (maxTouchPointers > 2) touchMoved = true
                    return true
                }
                if (touchTravel < dp(6)) {
                    return true
                }
                touchMoved = true

                val scale = touchSensitivity / resources.displayMetrics.density
                val dx = (pendingTouchX * scale).roundToInt()
                val dy = (pendingTouchY * scale).roundToInt()
                if (dx != 0 || dy != 0) {
                    val sentDx = dx.coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt())
                    val sentDy = dy.coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt())
                    val route = NativeBridge.nativeSendTrackpadDelta(
                        nativeHandle,
                        sessionEpoch,
                        sentDx,
                        sentDy,
                        touchButtonMask,
                    )
                    when (PackedTrackpadRoute.mode(route)) {
                        PackedTrackpadRoute.RELATIVE -> {
                            cursorOverlay.predictRelativeTrackpadDelta(sentDx, sentDy)
                        }
                        PackedTrackpadRoute.ABSOLUTE -> {
                            cursorOverlay.predictAbsoluteTrackpadTarget(
                                PackedTrackpadRoute.absoluteX(route),
                                PackedTrackpadRoute.absoluteY(route),
                            )
                        }
                    }
                    pendingTouchX = 0f
                    pendingTouchY = 0f
                }
            }

            MotionEvent.ACTION_POINTER_UP -> {
                var pointerTravel = 0f
                for (index in 0 until event.pointerCount) {
                    pointerTravel = pointerTravelTracker.pointerMoved(
                        event.getPointerId(index),
                        event.getX(index),
                        event.getY(index),
                    )
                }
                if (maxTouchPointers == 2 && event.pointerCount == 2 &&
                    (twoFingerScrolling || pointerTravel >= dp(6))
                ) {
                    twoFingerScrolling = true
                    touchMoved = true
                    pendingTouchX += event.centroidX() - lastTouchX
                    pendingTouchY += event.centroidY() - lastTouchY
                    val wheel = wheelAccumulator.addPixels(
                        pendingTouchX,
                        pendingTouchY,
                        resources.displayMetrics.density,
                    )
                    if (wheel != null) {
                        NativeBridge.nativeSendMouseWheel(
                            nativeHandle,
                            sessionEpoch,
                            wheel.x,
                            wheel.y,
                            touchButtonMask,
                        )
                    }
                }
                pointerTravelTracker.pointerUp(
                    event.getPointerId(event.actionIndex),
                    event.getX(event.actionIndex),
                    event.getY(event.actionIndex),
                )
                val remainingIndex = (0 until event.pointerCount)
                    .firstOrNull { it != event.actionIndex }
                if (remainingIndex != null) {
                    lastTouchX = event.getX(remainingIndex)
                    lastTouchY = event.getY(remainingIndex)
                }
                pendingTouchX = 0f
                pendingTouchY = 0f
                wheelAccumulator.reset()
            }

            MotionEvent.ACTION_UP -> {
                val isTap = !touchMoved && event.eventTime - touchDownAt <= TAP_TIMEOUT_MS
                val wasDrag = if (isTap && maxTouchPointers == 1) {
                    dragGesture.tapUp(event.eventTime, event.x, event.y)
                } else {
                    val dragging = dragGesture.dragging
                    dragGesture.cancel()
                    dragging
                }
                releaseTouchButtons(sessionEpoch)
                val button = when (maxTouchPointers) {
                    1 -> MOUSE_BUTTON_LEFT
                    2 -> MOUSE_BUTTON_RIGHT
                    else -> 0
                }
                if (isTap && button != 0 && !wasDrag) {
                    sendTouchClick(sessionEpoch, button)
                }
                if (isTap && button != 0) {
                    streamArea.performClick()
                }
                resetTouchGesture()
            }

            MotionEvent.ACTION_CANCEL -> cancelTouchInput()
            else -> return true
        }
        return true
    }

    private fun sendTouchButtonMask(sessionEpoch: Long, buttons: Int): Boolean {
        touchButtonMask = buttons
        return NativeBridge.nativeSendMouseButtons(nativeHandle, sessionEpoch, buttons)
    }

    private fun sendTouchClick(sessionEpoch: Long, button: Int) {
        sendTouchButtonMask(sessionEpoch, button)
        releaseTouchButtons(sessionEpoch)
    }

    private fun releaseTouchButtons(sessionEpoch: Long = currentSessionEpoch) {
        touchButtonMask = 0
        if (sessionStarted && sessionEpoch != 0L) {
            NativeBridge.nativeSendMouseButtons(nativeHandle, sessionEpoch, 0)
        }
    }

    private fun cancelTouchInput() {
        dragGesture.cancel()
        releaseTouchButtons()
        resetTouchGesture()
    }

    private fun resetTouchGesture() {
        touchMoved = false
        maxTouchPointers = 0
        pendingTouchX = 0f
        pendingTouchY = 0f
        touchTravel = 0f
        twoFingerScrolling = false
        wheelAccumulator.reset()
        pointerTravelTracker.reset()
    }

    private fun MotionEvent.centroidX(): Float = (0 until pointerCount).sumOf { getX(it).toDouble() }.toFloat() / pointerCount

    private fun MotionEvent.centroidY(): Float = (0 until pointerCount).sumOf { getY(it).toDouble() }.toFloat() / pointerCount

    private fun refreshDiscoveredServers() {
        if (!::serverCardsContainer.isInitialized) return
        val json = NativeBridge.nativeGetDiscoveredServers(nativeHandle, authenticationToken)
        if (json == lastDiscoveryJson) {
            return
        }
        lastDiscoveryJson = json
        lanDiscoveredServers = runCatching {
            val array = JSONArray(json)
            List(array.length()) { index ->
                val value = array.getJSONObject(index)
                LanDiscoveredServer(
                    hostname = value.getString("hostname"),
                    address = value.getString("address"),
                    peerId = value.optString("peer_id").trim(),
                )
            }.filter { it.peerId.isNotEmpty() }
        }.getOrDefault(emptyList())
        val reconciled = reconcileSavedServers(savedServers, lanDiscoveredServers)
        if (reconciled != savedServers) {
            savedServers = reconciled.toMutableList()
            persistSavedServers()
        }
        refreshServerCards()
    }

    private fun refreshServerCards() {
        if (!::serverCardsContainer.isInitialized) return
        if (apiHostPresence != null && SystemClock.uptimeMillis() - apiHostLastSeen > API_HOST_EXPIRY_MS) {
            apiHostPresence = null
        }
        apiStatusText.text = if (apiDiscoveryOnline) "●  Online" else "●  Offline"
        apiStatusText.setTextColor(if (apiDiscoveryOnline) COLOR_ONLINE else COLOR_OFFLINE)
        val query = searchInput.text.toString().trim()
        val cards = buildServerCards(savedServers, lanDiscoveredServers, apiHostPresence).filter { card ->
            query.isEmpty() || card.displayName.contains(query, ignoreCase = true) ||
                card.subtitle.contains(query, ignoreCase = true) ||
                card.connectAddress?.contains(query, ignoreCase = true) == true
        }
        serverCardsContainer.removeAllViews()
        if (cards.isEmpty()) {
            serverCardsContainer.addView(description(if (query.isEmpty()) {
                "No computers. Add a server address using the bar below."
            } else {
                "No matches"
            }).apply { setPadding(dp(4), dp(28), dp(4), dp(28)) })
            return
        }
        cards.forEach { card ->
            serverCardsContainer.addView(
                buildServerCard(card),
                LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply {
                    bottomMargin = dp(10)
                },
            )
        }
    }

    private fun buildServerCard(card: ServerCard) = LinearLayout(this).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(16), dp(12), dp(12), dp(12))
        background = roundedBackground(COLOR_CARD, 10, COLOR_CARD_BORDER)
        addView(TextView(this@MainActivity).apply {
            text = "●"
            textSize = 22f
            gravity = Gravity.CENTER
            setTextColor(if (card.online) COLOR_ONLINE else COLOR_TEXT_DIM)
        }, LinearLayout.LayoutParams(dp(42), dp(48)))
        addView(LinearLayout(this@MainActivity).apply {
            orientation = LinearLayout.VERTICAL
            addView(TextView(this@MainActivity).apply {
                text = card.displayName
                textSize = 17f
                setTextColor(COLOR_TEXT)
                maxLines = 1
            })
            addView(TextView(this@MainActivity).apply {
                text = card.subtitle
                textSize = 12f
                setTextColor(COLOR_TEXT_MUTED)
                maxLines = 1
            })
        }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        card.path?.let { path ->
            addView(TextView(this@MainActivity).apply {
                text = path.label
                textSize = 11f
                gravity = Gravity.CENTER
                setTextColor(COLOR_ACCENT)
                background = roundedBackground(Color.argb(30, 90, 200, 250), 12, Color.argb(100, 90, 200, 250))
            }, LinearLayout.LayoutParams(dp(54), dp(28)).apply { marginEnd = dp(8) })
        }
        addView(Button(this@MainActivity).apply {
            text = "Connect"
            isEnabled = card.connectAddress != null || card.tunnelPeerId != null
            setOnClickListener { connectToServer(card) }
        }, LinearLayout.LayoutParams(dp(112), dp(46)))
        if (card.savedAddress != null && !card.online) {
            addView(Button(this@MainActivity).apply {
                text = "×"
                contentDescription = "Remove ${card.displayName}"
                setOnClickListener { removeSavedServer(card.savedAddress) }
            }, LinearLayout.LayoutParams(dp(52), dp(46)).apply { marginStart = dp(4) })
        }
    }

    private fun startApiDiscovery() {
        if (apiDiscoveryFuture != null) {
            return
        }
        apiDiscoveryToken = authenticationToken
        apiDiscoveryStarted = true
        apiDiscoveryFuture = apiDiscoveryExecutor.scheduleWithFixedDelay(
            ::pollApiDiscovery,
            0,
            API_DISCOVERY_INTERVAL_SECONDS,
            TimeUnit.SECONDS,
        )
    }

    private fun stopApiDiscovery() {
        apiDiscoveryStarted = false
        apiDiscoveryGeneration += 1
        apiDiscoveryFuture?.cancel(true)
        apiDiscoveryFuture = null
    }

    private fun pollApiDiscovery() {
        val token = apiDiscoveryToken
        val generation = apiDiscoveryGeneration
        if (!apiDiscoveryStarted || token.isEmpty()) {
            handler.post {
                apiDiscoveryOnline = false
                apiHostPresence = null
                refreshServerCards()
            }
            return
        }
        val result = runCatching { ApiDiscovery.findHost(token) }
        handler.post {
            if (!apiDiscoveryStarted || generation != apiDiscoveryGeneration || token != apiDiscoveryToken) {
                return@post
            }
            apiDiscoveryOnline = result.isSuccess
            if (result.isSuccess) {
                val host = result.getOrNull()
                apiHostPresence = host
                apiHostLastSeen = if (host != null) SystemClock.uptimeMillis() else 0L
            } else if (SystemClock.uptimeMillis() - apiHostLastSeen > API_HOST_EXPIRY_MS) {
                apiHostPresence = null
            }
            refreshServerCards()
        }
    }

    private fun addServerByAddress() {
        val address = normalizeServerAddress(addServerInput.text.toString())
        if (address == null) {
            homeStatusText.text = "Enter a valid IP or host[:port]"
            return
        }
        val added = savedServers.none { it.address == address }
        if (added) {
            savedServers.add(SavedServer(address = address))
        }
        persistSavedServers()
        addServerInput.text.clear()
        homeStatusText.text = if (added) "Added $address" else "$address is already saved"
        refreshServerCards()
    }

    private fun removeSavedServer(address: String) {
        val removed = savedServers.removeAll { it.address == address }
        if (removed) {
            persistSavedServers()
            homeStatusText.text = "Removed $address"
            refreshServerCards()
        }
    }

    private fun reloadServers() {
        savedServers = loadSavedServers().toMutableList()
        lastDiscoveryJson = ""
        refreshDiscoveredServers()
        refreshServerCards()
        homeStatusText.text = "Server list reloaded"
    }

    private fun markServerConnected() {
        val savedAddress = connectionSavedAddress ?: return
        val index = savedServers.indexOfFirst { it.address == savedAddress }
        if (index < 0) return
        savedServers[index] = savedServers[index].copy(lastConnected = System.currentTimeMillis() / 1_000)
        persistSavedServers()
        connectionSavedAddress = null
        refreshServerCards()
    }

    private fun setAuthenticationToken(token: String) {
        if (authenticationToken == token) return
        authenticationToken = token
        tokenStore.save(token)
        apiDiscoveryToken = token
        apiDiscoveryGeneration += 1
        apiHostPresence = null
        apiHostLastSeen = 0L
        apiDiscoveryOnline = false
        lastDiscoveryJson = ""
        refreshDiscoveredServers()
        refreshServerCards()
    }

    private fun loadSavedServers(): List<SavedServer> {
        var legacyToken: String? = null
        val loaded = runCatching {
            val json = getSharedPreferences(PREFERENCES, MODE_PRIVATE).getString("servers", "[]") ?: "[]"
            val array = JSONArray(json)
            buildList {
                for (index in 0 until array.length()) {
                    val server = runCatching {
                        val value = array.getJSONObject(index)
                        val token = value.optString("token").trim().takeIf(String::isNotEmpty)
                        SavedServer(
                            address = value.getString("address"),
                            nickname = value.optString("nickname", value.optString("name")).trim(),
                            peerId = value.optString("peer_id").trim().takeIf(String::isNotEmpty),
                            lastConnected = value.optLong("last_connected", 0),
                            manuallyAdded = value.optBoolean("manually_added", true),
                            legacyToken = token,
                        )
                    }.getOrNull()
                    if (server?.manuallyAdded == true) {
                        legacyToken = legacyToken ?: server.legacyToken
                        add(server)
                    }
                }
            }
        }.getOrDefault(emptyList())
        if (authenticationToken.isEmpty() && !legacyToken.isNullOrEmpty()) {
            authenticationToken = legacyToken.orEmpty()
            tokenStore.save(authenticationToken)
        }
        return loaded
    }

    private fun persistSavedServers() {
        val array = JSONArray()
        savedServers.forEach { server ->
            array.put(JSONObject().apply {
                put("address", server.address)
                put("nickname", server.nickname)
                put("peer_id", server.peerId)
                put("last_connected", server.lastConnected)
                put("manually_added", server.manuallyAdded)
            })
        }
        getSharedPreferences(PREFERENCES, MODE_PRIVATE).edit().putString("servers", array.toString()).commit()
    }

    private fun loadSettings() {
        val preferences = getSharedPreferences(PREFERENCES, MODE_PRIVATE)
        authenticationToken = tokenStore.loadAndMigrate()
        clientPeerId = preferences.getString("client_peer_id", "")?.trim().orEmpty()
        if (clientPeerId.isEmpty()) {
            val generated = UUID.randomUUID().toString().replace("-", "")
            clientPeerId = generated
            preferences.edit().putString("client_peer_id", generated).apply()
        }
        apiRequestNonce = preferences.getLong("api_request_nonce", 0L)
        refreshRateHz = preferences.getInt("refresh_hz", 60)
        audioEnabled = preferences.getBoolean("audio_enabled", true)
        immersiveFullscreenEnabled = preferences.getBoolean("immersive_fullscreen", true)
        videoScaleMode = runCatching {
            VideoScaleMode.valueOf(preferences.getString("video_scale", VideoScaleMode.FIT.name)!!)
        }.getOrDefault(VideoScaleMode.FIT)
        launcherNormalizedX = preferences.getFloat("menu_x", Float.NaN)
        launcherNormalizedY = preferences.getFloat("menu_y", Float.NaN)
        touchEnabled = preferences.getBoolean("touch_enabled", true)
        touchSensitivity = preferences.getFloat("touch_sensitivity", 1.5f)
    }

    private fun nextApiRequestNonce(): Long {
        val preferences = getSharedPreferences(PREFERENCES, MODE_PRIVATE)
        val incremented = if (apiRequestNonce == Long.MAX_VALUE) Long.MAX_VALUE else apiRequestNonce + 1
        val next = max(incremented, System.currentTimeMillis()).coerceAtLeast(1L)
        apiRequestNonce = next
        preferences.edit().putLong("api_request_nonce", next).apply()
        return next
    }

    private fun applyKeepAwake() {
        val enabled = getSharedPreferences(PREFERENCES, MODE_PRIVATE).getBoolean("keep_awake", true)
        if (enabled) {
            window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        } else {
            window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        }
    }

    private fun acquireDiscoveryLock() {
        if (discoveryLock?.isHeld == true) {
            return
        }
        val wifi = applicationContext.getSystemService(WIFI_SERVICE) as WifiManager
        discoveryLock = wifi.createMulticastLock("st-lan-discovery").apply {
            setReferenceCounted(false)
            runCatching { acquire() }
        }
    }

    private fun releaseDiscoveryLock() {
        discoveryLock?.let { lock ->
            if (lock.isHeld) runCatching { lock.release() }
        }
        discoveryLock = null
    }

    private fun textInput(hint: String, value: String, variation: Int = InputType.TYPE_TEXT_VARIATION_NORMAL) =
        EditText(this).apply {
            this.hint = hint
            setText(value)
            setTextColor(Color.WHITE)
            setHintTextColor(Color.GRAY)
            inputType = InputType.TYPE_CLASS_TEXT or variation
            setSingleLine(true)
        }

    private fun simpleTextWatcher(changed: (String) -> Unit) = object : TextWatcher {
        override fun beforeTextChanged(value: CharSequence?, start: Int, count: Int, after: Int) = Unit
        override fun onTextChanged(value: CharSequence?, start: Int, before: Int, count: Int) = Unit
        override fun afterTextChanged(value: Editable?) = changed(value?.toString().orEmpty())
    }

    private fun title(text: String) = TextView(this).apply {
        this.text = text
        setTextColor(COLOR_TEXT)
        textSize = 30f
    }

    private fun description(text: String) = TextView(this).apply {
        this.text = text
        setTextColor(COLOR_TEXT_MUTED)
        textSize = 13f
        setPadding(dp(4), dp(4), dp(4), dp(4))
    }

    private fun infoCard(name: String, value: String) = LinearLayout(this).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(16), dp(12), dp(16), dp(12))
        background = roundedBackground(COLOR_CARD, 10, COLOR_CARD_BORDER)
        addView(TextView(this@MainActivity).apply {
            text = name
            setTextColor(COLOR_TEXT_MUTED)
            textSize = 14f
        }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        addView(TextView(this@MainActivity).apply {
            text = value
            setTextColor(COLOR_TEXT)
            textSize = 14f
        })
    }.also {
        it.layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply {
            topMargin = dp(10)
        }
    }

    private fun label(text: String) = TextView(this).apply {
        this.text = text
        setTextColor(Color.WHITE)
        textSize = 16f
        setPadding(dp(4), dp(8), dp(4), dp(2))
    }

    private fun rowParams() = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
    private fun dp(value: Int): Int = (value * resources.displayMetrics.density).roundToInt()

    private companion object {
        const val PREFERENCES = "st"
        const val TAP_TIMEOUT_MS = 250L
        const val CURSOR_POLL_MS = 16L
        const val DECODER_TRANSITION_TIMEOUT_MS = 3_000L
        const val VIDEO_ACTIVE_STATUS = "video active"
        const val MENU_VISUAL_DP = 40
        const val MENU_TOUCH_DP = 48
        const val MOUSE_BUTTON_LEFT = 1
        const val MOUSE_BUTTON_RIGHT = 2
        const val KEYBOARD_STATE_BYTES = 16
        const val IME_SHOW_RETRIES = 4
        const val IME_SHOW_RETRY_MS = 50L
        const val API_DISCOVERY_INTERVAL_SECONDS = 5L
        const val API_HOST_EXPIRY_MS = 30_000L
        val COLOR_BACKGROUND = Color.rgb(26, 30, 38)
        val COLOR_SIDEBAR = Color.rgb(21, 24, 31)
        val COLOR_CARD = Color.rgb(42, 46, 56)
        val COLOR_CARD_BORDER = Color.rgb(58, 62, 72)
        val COLOR_TEXT = Color.rgb(230, 233, 240)
        val COLOR_TEXT_MUTED = Color.rgb(138, 142, 150)
        val COLOR_TEXT_DIM = Color.rgb(90, 95, 108)
        val COLOR_ACCENT = Color.rgb(90, 200, 250)
        val COLOR_ONLINE = Color.rgb(80, 200, 120)
        val COLOR_OFFLINE = Color.rgb(180, 80, 80)
    }
}

private class AspectSurfaceView(context: Context) : SurfaceView(context) {
    private var videoWidth = 0
    private var videoHeight = 0
    private var scaleMode = VideoScaleMode.FIT

    fun setScaleMode(mode: VideoScaleMode) {
        if (scaleMode == mode) return
        scaleMode = mode
        requestLayout()
    }

    fun setVideoSize(width: Int, height: Int) {
        videoWidth = width
        videoHeight = height
        holder.setFixedSize(width, height)
        requestLayout()
    }

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val availableWidth = MeasureSpec.getSize(widthMeasureSpec)
        val availableHeight = MeasureSpec.getSize(heightMeasureSpec)
        if (videoWidth <= 0 || videoHeight <= 0 || availableWidth <= 0 || availableHeight <= 0) {
            setMeasuredDimension(availableWidth, availableHeight)
            return
        }
        if (scaleMode == VideoScaleMode.STRETCH) {
            setMeasuredDimension(availableWidth, availableHeight)
            return
        }
        val widthScale = availableWidth.toDouble() / videoWidth
        val heightScale = availableHeight.toDouble() / videoHeight
        val scale = if (scaleMode == VideoScaleMode.COVER) {
            max(widthScale, heightScale)
        } else {
            min(widthScale, heightScale)
        }
        val measuredWidth = (videoWidth * scale).roundToInt().coerceAtLeast(1)
        val measuredHeight = (videoHeight * scale).roundToInt().coerceAtLeast(1)
        setMeasuredDimension(measuredWidth, measuredHeight)
    }
}
