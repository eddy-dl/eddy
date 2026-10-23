package dev.pah.eddy

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.graphics.fonts.FontFamily
import android.graphics.fonts.FontStyle
import android.graphics.fonts.SystemFonts
import android.os.Build
import android.system.Os
import android.util.Log

// "Use the system font" (Settings → Appearance): the font the user picked
// for their phone, for Slint, which otherwise draws with its own choice.
// Android doesn't say which file its default font is, and phone makers keep
// theirs in different places, so each system font draws a sample and the
// one that draws exactly like Android's default is it. Slint reads its
// path from SLINT_DEFAULT_FONT when it first lays out text, so this runs
// before the activity starts Rust.
object SystemFont {
    private const val TAG = "eddy"
    private const val PREFS = "app_prefs"
    private const val ENABLED_KEY = "use_system_font"
    private const val SAMPLE = "Hamburgefonstiv 0123"

    fun apply(context: Context) {
        val enabled = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString(ENABLED_KEY, "false") == "true"
        if (!enabled || Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return
        val path = try {
            find()
        } catch (e: Exception) {
            Log.w(TAG, "couldn't look for the system font", e)
            null
        }
        if (path == null) {
            Log.i(TAG, "system font not found; Slint keeps its own")
            return
        }
        Log.i(TAG, "system font: $path")
        Os.setenv("SLINT_DEFAULT_FONT", path, true)
    }

    // The file of the regular upright font that draws like Typeface.DEFAULT.
    private fun find(): String? {
        val wanted = sample(Typeface.DEFAULT)
        for (font in SystemFonts.getAvailableFonts()) {
            val file = font.file ?: continue
            val style = font.style
            if (style.weight != FontStyle.FONT_WEIGHT_NORMAL || style.slant != FontStyle.FONT_SLANT_UPRIGHT) continue
            // Slint takes a file's first font.
            if (font.ttcIndex != 0 || !file.canRead()) continue
            val typeface = Typeface.CustomFallbackBuilder(FontFamily.Builder(font).build()).build()
            if (sample(typeface).sameAs(wanted)) return file.absolutePath
        }
        return null
    }

    private fun sample(typeface: Typeface): Bitmap {
        val paint = Paint().apply {
            this.typeface = typeface
            textSize = 40f
            isAntiAlias = true
        }
        val bitmap = Bitmap.createBitmap(520, 60, Bitmap.Config.ALPHA_8)
        Canvas(bitmap).drawText(SAMPLE, 4f, 44f, paint)
        return bitmap
    }
}
