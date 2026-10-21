package dev.pah.eddy

import android.content.Intent
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.os.Process
import android.view.Gravity
import android.view.WindowManager
import androidx.annotation.Keep
import com.google.androidgamesdk.GameActivity

// "Download with Eddy" / "Preview in Eddy" from the share sheet: a small
// Slint window at the bottom of the screen, over the app the links came from.
// It runs in its own process (see the manifest), because Slint can only start
// once per process and the main app may already be running. Rust's
// android_main sees this class and shows the popup instead of the app.
class SharePopupActivity : GameActivity() {
    companion object {
        const val MODE_DOWNLOAD = 1
        const val MODE_PREVIEW = 2
        private const val REQUEST_CODE_FOLDER_PICKER = 1001
    }

    private var mode = MODE_PREVIEW
    private var shared: SharedContent? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        // Read before super.onCreate: that starts android_main, which asks
        // for these straight away.
        mode = if (intent?.component?.className?.endsWith(".DownloadShare") == true) MODE_DOWNLOAD else MODE_PREVIEW
        shared = SharedContent.from(this, intent)
        super.onCreate(savedInstanceState)

        // Reaches the bottom edge, under the navigation bar; Slint pads the
        // content by the bar's height (its safe-area inset).
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.setDecorFitsSystemWindows(false)
        }
        @Suppress("DEPRECATION")
        window.navigationBarColor = Color.TRANSPARENT
        window.setGravity(Gravity.BOTTOM)
        window.setLayout(WindowManager.LayoutParams.MATCH_PARENT, dp(320))
        window.addFlags(WindowManager.LayoutParams.FLAG_DIM_BEHIND)
        window.attributes = window.attributes.apply { dimAmount = 0.5f }

        YtdlRuntime.start(this)
        DownloadService.ping(this, popupOpen = true)
    }

    override fun onDestroy() {
        DownloadService.ping(this, popupOpen = false)
        super.onDestroy()
        // Each popup needs a fresh process for Slint to start again.
        if (isFinishing) {
            Process.killProcess(Process.myPid())
        }
    }

    private fun dp(value: Int): Int = (value * resources.displayMetrics.density).toInt()

    @Keep fun shareMode(): Int = mode
    @Keep fun sharedText(): String? = shared?.text
    @Keep fun sharedPath(): String? = shared?.path
    @Keep fun sharedName(): String? = shared?.name

    // Called from Rust with the height the content needs, in dp.
    @Keep
    fun setPopupHeight(heightDp: Float) {
        runOnUiThread {
            val pixels = (heightDp * resources.displayMetrics.density).toInt()
            val maxHeight = (resources.displayMetrics.heightPixels * 0.85).toInt()
            window.setLayout(WindowManager.LayoutParams.MATCH_PARENT, pixels.coerceIn(dp(120), maxHeight))
        }
    }

    // Called from Rust: hands a download to the download service, which runs
    // in the main app's process.
    @Keep
    fun startDownload(url: String, backend: Int, format: String, filename: String, selectFiles: String, folder: String) {
        runOnUiThread {
            DownloadService.start(this, url, backend, format, filename, selectFiles, folder)
        }
    }

    @Keep fun hasAllFilesAccess(): Boolean = StorageAccess.granted(this)

    @Keep
    fun requestAllFilesAccess() {
        runOnUiThread { StorageAccess.request(this) }
    }

    // "Save to" in the preview: same picker as the main app.
    @Keep
    fun openFolderPicker() {
        runOnUiThread {
            startActivityForResult(Intent(this, FolderPickerActivity::class.java), REQUEST_CODE_FOLDER_PICKER)
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == REQUEST_CODE_FOLDER_PICKER && resultCode == RESULT_OK && data != null) {
            val uri = data.getStringExtra("uri") ?: return
            EddyNative.folderSelected(uri, data.getStringExtra("displayPath") ?: uri)
        }
    }
}
