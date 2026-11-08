package dev.pah.eddy

import android.app.Activity
import android.os.Build
import android.view.Display

// The screen's fastest refresh rate for Eddy's window. With the phone's
// refresh rate on auto, Android gives an app a high rate when its views ask
// for one (while scrolling, say); Slint draws everything into one surface
// and asks for nothing, so Eddy stayed at 60 Hz unless the phone was set to
// the high rate outright. Its window asks for the fastest mode at the
// screen's current resolution instead. Idle, Eddy draws no frames either way.
object RefreshRate {
    fun apply(activity: Activity) {
        val display: Display = (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) activity.display else null)
            ?: @Suppress("DEPRECATION") activity.windowManager.defaultDisplay
        val current = display.mode
        val fastest = display.supportedModes
            .filter { it.physicalWidth == current.physicalWidth && it.physicalHeight == current.physicalHeight }
            .maxByOrNull { it.refreshRate }
            ?: return
        if (fastest.modeId == activity.window.attributes.preferredDisplayModeId) return
        activity.window.attributes = activity.window.attributes.apply { preferredDisplayModeId = fastest.modeId }
    }
}
