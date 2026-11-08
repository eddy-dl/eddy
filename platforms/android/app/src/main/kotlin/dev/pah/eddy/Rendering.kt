package dev.pah.eddy

import android.content.Context
import android.system.Os

// Settings → Advanced → Redraw: whether Slint redraws only what changed in a
// frame (its "partial rendering"), off unless asked. Slint 1.18's GPU
// renderer otherwise redraws the whole window every frame, so a small
// animation (the Downloads tab's icon) costs as much as scrolling. It relies
// on the GPU driver telling the age of the screen's buffers right, hence the
// switch; "show" also outlines each frame's redrawn areas, to see it work.
// Slint reads SLINT_SKIA_PARTIAL_RENDERING when it starts (any value turns
// it on, "visualize" outlines too), so this runs before the activity starts
// Rust.
object Rendering {
    private const val PREFS = "app_prefs"
    private const val KEY = "partial_rendering"

    fun apply(context: Context) {
        when (context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString(KEY, "off")) {
            "on" -> Os.setenv("SLINT_SKIA_PARTIAL_RENDERING", "1", true)
            "show" -> Os.setenv("SLINT_SKIA_PARTIAL_RENDERING", "visualize", true)
            else -> Os.unsetenv("SLINT_SKIA_PARTIAL_RENDERING")
        }
    }
}
