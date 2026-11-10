package io.kubemaxx.st

import android.app.Activity
import android.os.Build
import android.util.DisplayMetrics
import android.view.Display

internal fun Activity.currentDisplay(): Display =
    (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) display else null)
        ?: @Suppress("DEPRECATION") windowManager.defaultDisplay

/** Physical pixel size of a display, which is what a virtual output is sized to. */
internal fun displayPixelSize(display: Display): Pair<Int, Int> {
    val metrics = DisplayMetrics()
    @Suppress("DEPRECATION")
    display.getRealMetrics(metrics)
    return metrics.widthPixels to metrics.heightPixels
}

internal fun displayRefreshMilliHz(display: Display): Int = (display.refreshRate * 1_000).toInt()
