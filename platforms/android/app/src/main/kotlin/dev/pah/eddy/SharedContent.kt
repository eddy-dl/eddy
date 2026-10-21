package dev.pah.eddy

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.OpenableColumns
import java.io.File

// What another app shared or opened with Eddy: some text, or a file copied
// into the cache (Rust can only read plain paths, not content:// URIs).
// Rust deletes the copy once it has read it.
data class SharedContent(val text: String?, val path: String?, val name: String?) {
    companion object {
        fun from(context: Context, intent: Intent?): SharedContent? {
            intent ?: return null
            val uri: Uri? = when (intent.action) {
                Intent.ACTION_SEND -> {
                    val text = intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString()
                    if (!text.isNullOrBlank()) return SharedContent(text, null, null)
                    @Suppress("DEPRECATION")
                    intent.getParcelableExtra(Intent.EXTRA_STREAM)
                }
                Intent.ACTION_VIEW -> intent.data
                else -> null
            }
            uri ?: return null
            if (uri.scheme == "magnet" || uri.scheme == "http" || uri.scheme == "https") {
                return SharedContent(uri.toString(), null, null)
            }
            return copyToCache(context, uri)?.let { (path, name) -> SharedContent(null, path, name) }
        }

        // Returns (path, display name), or null if the file couldn't be read.
        fun copyToCache(context: Context, uri: Uri): Pair<String, String>? {
            val name = displayName(context, uri) ?: "shared"
            return try {
                // A folder per copy, so two quick shares never overwrite each other.
                val dir = File(context.cacheDir, "shared/${System.nanoTime()}").apply { mkdirs() }
                val target = File(dir, name.replace('/', '_'))
                context.contentResolver.openInputStream(uri)?.use { input ->
                    target.outputStream().use { output -> input.copyTo(output) }
                } ?: return null
                Pair(target.absolutePath, name)
            } catch (e: Exception) {
                null
            }
        }

        private fun displayName(context: Context, uri: Uri): String? {
            try {
                context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
                    if (cursor.moveToFirst()) {
                        return cursor.getString(0)
                    }
                }
            } catch (e: Exception) {
                // file:// URIs and some providers can't be queried.
            }
            return uri.lastPathSegment
        }
    }
}
