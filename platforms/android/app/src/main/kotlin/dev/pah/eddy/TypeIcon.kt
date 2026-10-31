package dev.pah.eddy

import android.content.Context
import android.content.res.Configuration
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import androidx.core.content.ContextCompat

// A download type's tile, as its card shows it (ui/components/FileTypeIcon.slint):
// the type's glyph in its colour, on a faint tint of that colour. For the
// notifications' large icon.
object TypeIcon {
    // In FileType's order (0 Video … 7 Folder, 8 Other), as Rust sends it.
    private val glyphs = intArrayOf(
        R.drawable.type_movie,
        R.drawable.type_music_note,
        R.drawable.type_image,
        R.drawable.type_description,
        R.drawable.type_folder_zip,
        R.drawable.type_album,
        R.drawable.type_apps,
        R.drawable.type_folder,
        R.drawable.type_draft,
    )
    private val colors = intArrayOf(
        0xFFE07A5F.toInt(),
        0xFF9C89B8.toInt(),
        0xFF81B29A.toInt(),
        0xFF5B8DEF.toInt(),
        0xFFE0A85C.toInt(),
        0xFF7E8CE0.toInt(),
        0xFF8D99AE.toInt(),
        0xFFE9B949.toInt(),
        0xFF9E9E9E.toInt(),
    )

    // Made once per type and theme (the progress notification asks every
    // second).
    private val made = HashMap<Pair<Int, Boolean>, Bitmap>()

    // Null for a type it doesn't know (none sent).
    fun bitmap(context: Context, type: Int): Bitmap? {
        if (type !in glyphs.indices) return null
        val dark = context.resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK == Configuration.UI_MODE_NIGHT_YES
        return made.getOrPut(type to dark) { draw(context, type, dark) ?: return null }
    }

    private fun draw(context: Context, type: Int, dark: Boolean): Bitmap? {
        val glyph = ContextCompat.getDrawable(context, glyphs[type])?.mutate() ?: return null
        val color = colors[type]
        // The card's 44 dp tile and 24 dp glyph, at the notification's
        // large icon size.
        val size = (64 * context.resources.displayMetrics.density).toInt()
        val bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888)
        val canvas = Canvas(bitmap)
        val tint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            this.color = Color.argb(if (dark) 51 else 38, Color.red(color), Color.green(color), Color.blue(color))
        }
        val radius = size * 12f / 44f
        canvas.drawRoundRect(RectF(0f, 0f, size.toFloat(), size.toFloat()), radius, radius, tint)
        val glyphSize = size * 24 / 44
        val inset = (size - glyphSize) / 2
        glyph.setBounds(inset, inset, inset + glyphSize, inset + glyphSize)
        glyph.setTint(color)
        glyph.draw(canvas)
        return bitmap
    }
}
