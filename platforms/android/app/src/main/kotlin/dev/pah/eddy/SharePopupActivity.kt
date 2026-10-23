package dev.pah.eddy

import android.content.Intent
import android.graphics.Color
import android.graphics.PixelFormat
import android.os.Build
import android.os.Bundle
import android.os.Process
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
        SystemFont.apply(this)
        // Read before super.onCreate: that starts android_main, which asks
        // for these straight away.
        mode = if (intent?.component?.className?.endsWith(".DownloadShare") == true) MODE_DOWNLOAD else MODE_PREVIEW
        shared = SharedContent.from(this, intent)
        super.onCreate(savedInstanceState)

        // The window covers the whole screen (or the whole small window, so
        // HiOS's border goes round all of it). Slint draws the sheet at the
        // bottom and leaves the rest see-through; Android dims or blurs
        // what's behind the window. Slint pads by the system bars itself.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.setDecorFitsSystemWindows(false)
        }
        @Suppress("DEPRECATION")
        window.statusBarColor = Color.TRANSPARENT
        @Suppress("DEPRECATION")
        window.navigationBarColor = Color.TRANSPARENT
        window.setLayout(WindowManager.LayoutParams.MATCH_PARENT, WindowManager.LayoutParams.MATCH_PARENT)
        window.addFlags(WindowManager.LayoutParams.FLAG_DIM_BEHIND)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            window.addFlags(WindowManager.LayoutParams.FLAG_BLUR_BEHIND)
            window.attributes = window.attributes.apply { blurBehindRadius = dp(24) }
            // Blur can be off (battery saver, some devices): dim more then.
            windowManager.addCrossWindowBlurEnabledListener(mainExecutor, blurListener)
        } else {
            setDim(blurEnabled = false)
        }

        YtdlRuntime.start(this)
        DownloadService.ping(this, popupOpen = true)
    }

    override fun onDestroy() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            windowManager.removeCrossWindowBlurEnabledListener(blurListener)
        }
        DownloadService.ping(this, popupOpen = false)
        if (isFinishing) EddyNative.screenClosed()
        super.onDestroy()
        // Each popup needs a fresh process for Slint to start again.
        if (isFinishing) {
            Process.killProcess(Process.myPid())
        }
    }

    private val blurListener = java.util.function.Consumer<Boolean> { setDim(it) }

    private fun setDim(blurEnabled: Boolean) {
        window.attributes = window.attributes.apply { dimAmount = if (blurEnabled) 0.3f else 0.5f }
    }

    // A see-through surface, so the window shows what's behind it above
    // the sheet (GameActivity's SurfaceView is opaque by default).
    override fun onCreateSurfaceView() {
        super.onCreateSurfaceView()
        mSurfaceView?.holder?.setFormat(PixelFormat.TRANSLUCENT)
    }

    private fun dp(value: Int): Int = (value * resources.displayMetrics.density).toInt()

    // Called from Rust (kind: see Haptics).
    @Keep
    fun triggerHaptic(kind: Int) {
        runOnUiThread { Haptics.play(this, kind) }
    }

    @Keep fun shareMode(): Int = mode
    @Keep fun sharedText(): String? = shared?.text
    @Keep fun sharedPath(): String? = shared?.path
    @Keep fun sharedName(): String? = shared?.name

    // Called from Rust: hands a download to the download service, which runs
    // in the main app's process.
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
