package dev.pah.eddy

import android.content.Context
import android.graphics.Paint
import android.graphics.Typeface
import android.os.Build
import android.system.Os
import android.util.Log

// "Use the system font" (Settings → Appearance): the font the user picked
// for their phone, for Slint, which otherwise draws with its own choice.
// Android 12+ says which font it lays a word out with (TextRunShaper): the
// default font, whatever the phone maker calls it. Theme fonts (HiOS's, for
// one) live where apps can't read the file, so its bytes, which Android has
// loaded, are copied into Eddy's own folder for Slint. Slint reads the path
// from SLINT_DEFAULT_FONT when it starts, so this runs before the activity
// starts Rust.
object SystemFont {
    private const val TAG = "eddy"
    private const val PREFS = "app_prefs"
    private const val ENABLED_KEY = "use_system_font"
    // What happened, for Settings and the logs: the font's path, or
    // "none: <why>".
    private const val STATUS_KEY = "system_font_status"
    private const val SAMPLE = "Hamburgefonstiv 0123"

    fun apply(context: Context) {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val enabled = prefs.getString(ENABLED_KEY, "false") == "true"
        val status: String
        if (!enabled) {
            Os.unsetenv("SLINT_DEFAULT_FONT")
            status = "none: off"
        } else if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) {
            status = "none: needs Android 12"
        } else {
            status = try {
                find(context)?.also { Os.setenv("SLINT_DEFAULT_FONT", it, true) } ?: "none: not found"
            } catch (e: Exception) {
                Log.w(TAG, "couldn't look for the system font", e)
                "none: ${e.javaClass.simpleName}"
            }
        }
        Log.i(TAG, "system font: $status")
        prefs.edit().putString(STATUS_KEY, status).apply()
    }

    // The font Android lays letters out with in its default typeface
    // (Android 12+; before that there's no way to tell reliably: comparing
    // drawn samples matches any font without letters, as those fall back to
    // the default font). Its file when Eddy can read it, else a copy of it.
    private fun find(context: Context): String? {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return null
        val paint = Paint().apply {
            typeface = Typeface.DEFAULT
            textSize = 40f
        }
        val glyphs = android.graphics.text.TextRunShaper.shapeTextRun(SAMPLE, 0, SAMPLE.length, 0, SAMPLE.length, 0f, 0f, false, paint)
        if (glyphs.glyphCount() == 0) return null
        val font = glyphs.getFont(0)
        // Slint takes a file's first font.
        if (font.ttcIndex != 0) return null
        font.file?.takeIf { it.canRead() }?.let { return it.absolutePath }
        // Not readable (a theme's): the bytes Android loaded, saved as ours.
        val buffer = font.buffer.duplicate().apply { rewind() }
        val bytes = ByteArray(buffer.remaining()).also { buffer.get(it) }
        if (bytes.isEmpty()) return null
        val copy = java.io.File(context.filesDir, "system-font.ttf")
        if (!copy.exists() || copy.length() != bytes.size.toLong() || !copy.readBytes().contentEquals(bytes)) {
            copy.writeBytes(bytes)
        }
        return copy.absolutePath
    }
}
