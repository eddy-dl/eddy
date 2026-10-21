package dev.pah.eddy

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts

class FolderPickerActivity : ComponentActivity() {

    private val folderPickerLauncher = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri: Uri? ->
        if (uri != null) {
            val takeFlags = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
            contentResolver.takePersistableUriPermission(uri, takeFlags)

            var displayPath = uri.lastPathSegment ?: "Unknown Folder"
            if (displayPath.contains(":")) {
                val parts = displayPath.split(":")
                val storageName = if (parts[0] == "primary") "Internal Storage" else "SD Card"
                displayPath = "$storageName/${parts[1]}"
            }

            // Return values back to MainActivity
            val resultIntent = Intent().apply {
                putExtra("uri", uri.toString())
                putExtra("displayPath", displayPath)
            }
            setResult(Activity.RESULT_OK, resultIntent)
        } else {
            setResult(Activity.RESULT_CANCELED)
        }
        finish() // Close the trampoline
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Launch picker immediately on creation
        folderPickerLauncher.launch(null)
    }
}
