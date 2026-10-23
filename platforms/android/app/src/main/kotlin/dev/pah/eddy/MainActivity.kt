package dev.pah.eddy

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.view.View
import android.view.WindowInsetsController
import android.view.WindowManager
import android.webkit.MimeTypeMap
import android.widget.Toast
import androidx.core.content.FileProvider
import androidx.core.view.WindowCompat
import java.io.File
import androidx.annotation.Keep
import com.google.androidgamesdk.GameActivity

class MainActivity : GameActivity() {
    companion object {
        private const val REQUEST_CODE_FOLDER_PICKER = 1001
        private const val REQUEST_CODE_FILE_PICKER = 1002

        init {
            System.loadLibrary("eddy")
        }
    }

    external fun onFilePicked(path: String, displayName: String)

    override fun onCreate(savedInstanceState: Bundle?) {
        // Before super.onCreate, which starts Rust (and Slint's fonts).
        SystemFont.apply(this)
        super.onCreate(savedInstanceState)

        // Edge to edge, like NativeActivity's window was: Slint pads the
        // content by the system bars itself (its safe-area insets).
        WindowCompat.setDecorFitsSystemWindows(window, false)

        // Draw under the camera cutout instead of letterboxing it. Slint 1.18+
        // already pads by the cutout inset, so letterboxing doubled the gap.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            window.attributes = window.attributes.apply {
                layoutInDisplayCutoutMode =
                    WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
            }
        }

        DownloadService.askForNotifications(this)
        YtdlRuntime.start(this)
        DownloadService.mainAppOpen = true
        DownloadService.ping(this)
    }

    override fun onDestroy() {
        DownloadService.mainAppOpen = false
        // Not for a rotation or the like: only when it's really going.
        if (isFinishing) EddyNative.screenClosed()
        super.onDestroy()
    }

    // Back from the installer (or anywhere): pick up helper apps that were
    // installed or updated meanwhile.
    override fun onResume() {
        super.onResume()
        YtdlRuntime.start(this)
    }

    // Called from Rust: opens Android's installer for a downloaded helper
    // APK. The user decides whether to install it.
    @Keep
    fun installApk(path: String) {
        runOnUiThread {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && !packageManager.canRequestPackageInstalls()) {
                // Android first needs the user to let Eddy open installers.
                Toast.makeText(this, getString(R.string.allow_installs), Toast.LENGTH_LONG).show()
                startActivity(Intent(android.provider.Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:$packageName")))
                return@runOnUiThread
            }
            try {
                val uri = FileProvider.getUriForFile(this, "$packageName.files", File(path))
                startActivity(
                    Intent(Intent.ACTION_VIEW)
                        .setDataAndType(uri, "application/vnd.android.package-archive")
                        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                )
            } catch (e: Exception) {
                Toast.makeText(this, getString(R.string.cant_open_file), Toast.LENGTH_LONG).show()
            }
        }
    }


    @Keep
    fun openFolderPicker() {
        runOnUiThread {
            val intent = Intent(this, FolderPickerActivity::class.java)
            startActivityForResult(intent, REQUEST_CODE_FOLDER_PICKER)
        }
    }

    // Called from Rust: hands a download to the download service.
    @Keep
    fun startDownload(url: String, backend: Int, format: String, filename: String, selectFiles: String, folder: String, settings: String) {
        runOnUiThread {
            DownloadService.start(this, url, backend, format, filename, selectFiles, folder, settings)
        }
    }

    @Keep fun hasAllFilesAccess(): Boolean = StorageAccess.granted(this)

    @Keep
    fun requestAllFilesAccess() {
        runOnUiThread { StorageAccess.request(this) }
    }

    // Called from Rust: opens a downloaded file in whichever app handles it.
    @Keep
    fun openFile(path: String) {
        runOnUiThread {
            val file = File(path)
            try {
                // A folder (a playlist's, a post's, a torrent's): in the
                // phone's file manager.
                if (file.isDirectory) {
                    openFolder(file)
                    return@runOnUiThread
                }
                // A copy in a picked folder is a document URI already.
                val content = path.startsWith("content://")
                val uri = if (content) android.net.Uri.parse(path)
                    else FileProvider.getUriForFile(this, "$packageName.files", file)
                val mime = (if (content) contentResolver.getType(uri) else null)
                    ?: MimeTypeMap.getSingleton().getMimeTypeFromExtension(file.extension.lowercase()) ?: "*/*"
                val intent = Intent(Intent.ACTION_VIEW)
                    .setDataAndType(uri, mime)
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                startActivity(Intent.createChooser(intent, if (content) null else file.name))
            } catch (e: Exception) {
                // Missing file, a folder FileProvider can't share, or no app for it.
                Toast.makeText(this, getString(R.string.cant_open_file), Toast.LENGTH_LONG).show()
            }
        }
    }

    // Shares downloads: `paths` (newline-separated files or document URIs)
    // as attachments, with `text` (their links) alongside; or the text alone.
    fun shareItems(paths: String, text: String) {
        runOnUiThread {
            try {
                val uris = ArrayList(paths.lines().filter { it.isNotBlank() }.map { path ->
                    if (path.startsWith("content://")) android.net.Uri.parse(path)
                    else FileProvider.getUriForFile(this, "$packageName.files", File(path))
                })
                val intent = when (uris.size) {
                    0 -> Intent(Intent.ACTION_SEND).setType("text/plain")
                    1 -> Intent(Intent.ACTION_SEND).putExtra(Intent.EXTRA_STREAM, uris[0])
                        .setType(contentResolver.getType(uris[0]) ?: "*/*")
                    else -> Intent(Intent.ACTION_SEND_MULTIPLE).putParcelableArrayListExtra(Intent.EXTRA_STREAM, uris)
                        .setType("*/*")
                }
                if (text.isNotBlank()) intent.putExtra(Intent.EXTRA_TEXT, text)
                if (uris.isNotEmpty()) {
                    // The receiving app may only read what's in the clip data.
                    val clip = android.content.ClipData.newRawUri(null, uris[0])
                    uris.drop(1).forEach { clip.addItem(android.content.ClipData.Item(it)) }
                    intent.clipData = clip
                    intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                }
                startActivity(Intent.createChooser(intent, null))
            } catch (e: Exception) {
                android.util.Log.w("eddy", "couldn't share", e)
                Toast.makeText(this, getString(R.string.cant_share), Toast.LENGTH_LONG).show()
            }
        }
    }

    // Shared storage's folders have a document URI the system's Files app
    // (and most others) open as a folder.
    private fun openFolder(folder: File) {
        val root = android.os.Environment.getExternalStorageDirectory().absolutePath
        val rel = folder.absolutePath.removePrefix(root).trimStart('/')
        val uri = android.provider.DocumentsContract.buildDocumentUri("com.android.externalstorage.documents", "primary:$rel")
        val intent = Intent(Intent.ACTION_VIEW)
            .setDataAndType(uri, android.provider.DocumentsContract.Document.MIME_TYPE_DIR)
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        try {
            startActivity(intent)
        } catch (e: Exception) {
            Toast.makeText(this, getString(R.string.saved_in_folder, folder.absolutePath.removePrefix(root)), Toast.LENGTH_LONG).show()
        }
    }

    // A .txt of links or a .torrent; the result goes to onFilePicked.
    @Keep
    // Deletes a download's copy in a picked folder (a document URI).
    fun deleteDocument(uri: String) {
        try {
            android.provider.DocumentsContract.deleteDocument(contentResolver, android.net.Uri.parse(uri))
        } catch (e: Exception) {
            runOnUiThread { Toast.makeText(this, getString(R.string.cant_delete_file), Toast.LENGTH_LONG).show() }
        }
    }

    // A web page, in the browser (Settings → About's links).
    fun openUrl(url: String) {
        runOnUiThread {
            try {
                startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(url)))
            } catch (e: Exception) {
                Toast.makeText(this, url, Toast.LENGTH_LONG).show()
            }
        }
    }

    fun openFilePicker() {
        runOnUiThread {
            val intent = Intent(this, FilePickerActivity::class.java)
            startActivityForResult(intent, REQUEST_CODE_FILE_PICKER)
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        
        if (requestCode == REQUEST_CODE_FOLDER_PICKER && resultCode == RESULT_OK && data != null) {
            val uriString = data.getStringExtra("uri") ?: return
            val displayPath = data.getStringExtra("displayPath") ?: "Unknown Folder"
            
            // Pass the safely parsed data back to your C++ layers
            EddyNative.folderSelected(uriString, displayPath)
        }

        if (requestCode == REQUEST_CODE_FILE_PICKER && resultCode == RESULT_OK && data != null) {
            val path = data.getStringExtra("path") ?: return
            onFilePicked(path, data.getStringExtra("name") ?: File(path).name)
        }
    }

    @Keep
    fun showToast(message: String) {
        runOnUiThread {
            Toast.makeText(this, message, Toast.LENGTH_LONG).show()
        }
    }

    // Called from Rust (kind: see Haptics).
    @Keep
    fun triggerHaptic(kind: Int) {
        runOnUiThread { Haptics.play(this, kind) }
    }

    @Keep
    fun updateSystemBarsAppearance(darkIconsFlag: Int) {
        val darkIcons = darkIconsFlag != 0
        
        runOnUiThread {
            window.addFlags(WindowManager.LayoutParams.FLAG_DRAWS_SYSTEM_BAR_BACKGROUNDS)

            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                val controller = window.insetsController
                if (controller != null) {
                    val lightFlags = WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS or
                            WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS
                    
                    val appearance = if (darkIcons) lightFlags else 0
                    controller.setSystemBarsAppearance(appearance, lightFlags)
                }
            } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                val decorView = window.decorView
                var flags = decorView.systemUiVisibility
                
                val lightStatus = View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR
                val lightNav = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) 0x00000010 else 0
                val mask = lightStatus or lightNav

                flags = if (darkIcons) {
                    flags or mask
                } else {
                    flags and mask.inv()
                }
                decorView.systemUiVisibility = flags
            }
        }
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
    }
}

