package dev.pah.eddy

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts
import java.io.File

// Trampoline that lets the user pick a .txt list of links or a .torrent file.
// The picked document is copied into the app's cache, because Rust can only
// read plain file paths, not content:// URIs.
class FilePickerActivity : ComponentActivity() {

    private val filePickerLauncher = registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri: Uri? ->
        val copied = uri?.let { copyToCache(it) }
        if (copied != null) {
            val resultIntent = Intent().apply {
                putExtra("path", copied.first)
                putExtra("name", copied.second)
            }
            setResult(Activity.RESULT_OK, resultIntent)
        } else {
            setResult(Activity.RESULT_CANCELED)
        }
        finish() // Close the trampoline
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Many file managers don't tag .torrent files, so octet-stream is
        // allowed too; Rust checks what the file actually is.
        filePickerLauncher.launch(arrayOf("text/plain", "application/x-bittorrent", "application/octet-stream"))
    }

    // Returns (path, display name), or null if the file couldn't be read.
    private fun copyToCache(uri: Uri): Pair<String, String>? {
        val name = displayName(uri) ?: "picked"
        return try {
            val dir = File(cacheDir, "picked").apply { mkdirs() }
            // Only the latest pick is needed; don't let old copies pile up.
            dir.listFiles()?.forEach { it.delete() }
            val target = File(dir, name.replace('/', '_'))
            contentResolver.openInputStream(uri)?.use { input ->
                target.outputStream().use { output -> input.copyTo(output) }
            } ?: return null
            Pair(target.absolutePath, name)
        } catch (e: Exception) {
            null
        }
    }

    private fun displayName(uri: Uri): String? {
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
            if (cursor.moveToFirst()) {
                return cursor.getString(0)
            }
        }
        return uri.lastPathSegment
    }
}
