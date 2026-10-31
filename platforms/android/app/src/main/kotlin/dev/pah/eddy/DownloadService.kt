package dev.pah.eddy

import android.app.Activity
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import android.widget.Toast
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider

// Foreground service that keeps the app process (and the downloads running
// in Rust) alive, with a notification showing what's downloading. It runs
// from the moment any part of the app opens (main app or share popup) and
// stops once nothing is downloading and nothing is open. Finished and failed
// downloads get a notification of their own.
class DownloadService : Service() {
    companion object {
        private const val TAG = "eddy"
        private const val CHANNEL_ID = "downloads"
        private const val RESULTS_CHANNEL_ID = "results"
        private const val NOTIFICATION_ID = 1
        private const val CHECK_INTERVAL_MS = 2000L
        private const val REQUEST_NOTIFICATIONS = 2001
        // The progress notification's buttons (EddyNative.controlDownloads).
        private const val ACTION_PAUSE = "dev.pah.eddy.PAUSE_ALL"
        private const val ACTION_CANCEL = "dev.pah.eddy.CANCEL_ALL"
        // Its Exit button: everything paused, Eddy closed.
        private const val ACTION_EXIT = "dev.pah.eddy.EXIT"
        // From Rust (any process): a site's Cloudflare check to pass in a
        // WebView (CloudflareActivity), with "url" and "result" extras.
        private const val ACTION_CHECK_SITE = "dev.pah.eddy.CHECK_SITE"
        // From Rust (any process): a page to click through to its download
        // in Eddy's browser (BrowserActivity), with "page" and "result".
        private const val ACTION_BROWSE = "dev.pah.eddy.BROWSE"
        // From Rust (any process): a page to load in the background browser
        // (PageFetcher), with "url" and "result".
        private const val ACTION_FETCH_PAGE = "dev.pah.eddy.FETCH_PAGE"

        // The main app's activity is open (same process as the service).
        @Volatile var mainAppOpen = false
        // That activity, so Exit can close it.
        @Volatile var mainActivity: java.lang.ref.WeakReference<Activity>? = null

        // Hands a download to the service, starting it if needed. Works from
        // any process (the share popup runs in its own).
        fun start(activity: Activity, url: String, backend: Int, format: String, filename: String, selectFiles: String, folder: String, settings: String) {
            askForNotifications(activity)
            val intent = Intent(activity, DownloadService::class.java).apply {
                putExtra("url", url)
                putExtra("backend", backend)
                putExtra("format", format)
                putExtra("filename", filename)
                putExtra("select_files", selectFiles)
                putExtra("folder", folder)
                putExtra("settings", settings)
            }
            ContextCompat.startForegroundService(activity, intent)
        }

        // Starts the service (and its notification) without a download, as
        // soon as the app opens. `popupOpen` tells it about the share popup,
        // which lives in another process.
        fun ping(context: Context, popupOpen: Boolean? = null) {
            val intent = Intent(context, DownloadService::class.java)
            if (popupOpen != null) {
                intent.putExtra("popup_open", popupOpen)
            }
            try {
                ContextCompat.startForegroundService(context, intent)
            } catch (e: Exception) {
                // Not allowed from the background (e.g. a popup that's closing).
                Log.w(TAG, "couldn't reach the download service", e)
            }
        }

        // Android 13+ hides the notifications without this; downloads run
        // either way. Asked when the app opens, since a request made by the
        // share popup is lost when the popup closes.
        fun askForNotifications(activity: Activity) {
            if (Build.VERSION.SDK_INT >= 33 &&
                ContextCompat.checkSelfPermission(activity, android.Manifest.permission.POST_NOTIFICATIONS)
                    != PackageManager.PERMISSION_GRANTED
            ) {
                activity.requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), REQUEST_NOTIFICATIONS)
            }
        }
    }

    private val handler = Handler(Looper.getMainLooper())
    private var idleChecks = 0
    private var popupOpen = false
    private var nextResultId = NOTIFICATION_ID + 1
    // Finished downloads still being copied into their picked folders.
    @Volatile private var copying = 0

    private val check = object : Runnable {
        override fun run() {
            postResults()
            val (active, name, permille, kind) = status()
            if (active == 0 && copying == 0 && !mainAppOpen && !popupOpen) {
                // Twice in a row, so a download that's just being added
                // doesn't stop the service.
                idleChecks += 1
                if (idleChecks >= 2) {
                    Log.i(TAG, "download service idle, stopping")
                    ServiceCompat.stopForeground(this@DownloadService, ServiceCompat.STOP_FOREGROUND_REMOVE)
                    stopSelf()
                    return
                }
            } else {
                idleChecks = 0
            }
            notificationManager().notify(NOTIFICATION_ID, progressNotification(active, name, permille, kind))
            handler.postDelayed(this, CHECK_INTERVAL_MS)
        }
    }

    override fun onCreate() {
        super.onCreate()
        EddyNative.init(applicationContext)
        YtdlRuntime.start(applicationContext)
        notificationManager().createNotificationChannel(
            NotificationChannel(CHANNEL_ID, getString(R.string.notification_channel), NotificationManager.IMPORTANCE_LOW)
        )
        notificationManager().createNotificationChannel(
            NotificationChannel(RESULTS_CHANNEL_ID, getString(R.string.results_channel), NotificationManager.IMPORTANCE_DEFAULT)
        )
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC else 0
        val (active, name, permille, kind) = status()
        ServiceCompat.startForeground(this, NOTIFICATION_ID, progressNotification(active, name, permille, kind), type)

        // Pause / Cancel on the progress notification: every running or
        // queued download.
        when (intent?.action) {
            ACTION_PAUSE -> EddyNative.controlDownloads(0)
            ACTION_CANCEL -> EddyNative.controlDownloads(1)
            ACTION_EXIT -> {
                exit()
                return START_NOT_STICKY
            }
            ACTION_CHECK_SITE -> CloudflareActivity.show(
                this,
                intent.getStringExtra(CloudflareActivity.EXTRA_URL).orEmpty(),
                intent.getStringExtra(CloudflareActivity.EXTRA_RESULT).orEmpty(),
                mainAppOpen || popupOpen,
            )
            // Asked for from the preview, so Eddy is on screen.
            ACTION_BROWSE -> BrowserActivity.show(
                this,
                intent.getStringExtra(BrowserActivity.EXTRA_PAGE).orEmpty(),
                intent.getStringExtra(BrowserActivity.EXTRA_RESULT).orEmpty(),
            )
            ACTION_FETCH_PAGE -> PageFetcher.fetch(
                this,
                intent.getStringExtra("url").orEmpty(),
                intent.getStringExtra("result").orEmpty(),
            )
        }

        if (intent?.hasExtra("popup_open") == true) {
            popupOpen = intent.getBooleanExtra("popup_open", false)
        }

        // A download to start (those come without an action; a browser
        // check's "url" is the page to check, not one to download).
        val url = intent?.takeIf { it.action == null }?.getStringExtra("url")
        if (url != null) {
            Log.i(TAG, "download service got $url")
            val started = EddyNative.startDownload(
                url,
                intent.getIntExtra("backend", 0),
                intent.getStringExtra("format") ?: "",
                intent.getStringExtra("filename") ?: "",
                intent.getStringExtra("select_files") ?: "",
                intent.getStringExtra("folder") ?: "",
                intent.getStringExtra("settings") ?: "",
            )
            // Also shows the download arrived when notifications are off.
            val message = if (started) R.string.download_started else R.string.download_failed
            Toast.makeText(this, getString(message), Toast.LENGTH_SHORT).show()
        }

        idleChecks = 0
        handler.removeCallbacks(check)
        handler.postDelayed(check, CHECK_INTERVAL_MS)
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        handler.removeCallbacks(check)
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun notificationManager() = getSystemService(NotificationManager::class.java)

    // Active downloads, the first one's name, progress in thousandths (or
    // -1), and the one running download's type (TypeIcon; -1 for several).
    private data class Status(val active: Int, val name: String, val permille: Int, val kind: Int)

    private fun status(): Status {
        val parts = EddyNative.status()?.split('\t')
            // Rust couldn't say (it logs why): keep the count right at least,
            // so the service doesn't stop under a running download.
            ?: return Status(EddyNative.activeDownloads(), "", -1, -1)
        return Status(
            parts.getOrNull(0)?.toIntOrNull() ?: 0,
            parts.getOrNull(1) ?: "",
            parts.getOrNull(2)?.toIntOrNull() ?: -1,
            parts.getOrNull(3)?.toIntOrNull() ?: -1,
        )
    }

    private fun openAppIntent(): PendingIntent = PendingIntent.getActivity(
        this, 0,
        Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
        PendingIntent.FLAG_IMMUTABLE,
    )

    // The app, opening download `id` (MainActivity.EXTRA_OPEN_DOWNLOAD); one
    // per download, so each notification keeps its own.
    private fun openDownloadIntent(id: Long): PendingIntent = PendingIntent.getActivity(
        this, id.toInt(),
        Intent(this, MainActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            .putExtra(MainActivity.EXTRA_OPEN_DOWNLOAD, id),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    // The file download `id` saved, in the app the phone opens it with,
    // without Eddy's window; null for a folder (the app lists those) or a
    // file that can't be shared.
    private fun openFileIntent(id: Long): PendingIntent? {
        val path = EddyNative.downloadFile(id) ?: return null
        return try {
            val content = path.startsWith("content://")
            val file = java.io.File(path)
            val uri = if (content) android.net.Uri.parse(path) else FileProvider.getUriForFile(this, "$packageName.files", file)
            val mime = (if (content) contentResolver.getType(uri) else null)
                ?: android.webkit.MimeTypeMap.getSingleton().getMimeTypeFromExtension(file.extension.lowercase()) ?: "*/*"
            val view = Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, mime)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
            PendingIntent.getActivity(this, id.toInt(), view, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        } catch (e: Exception) {
            Log.w(TAG, "can't open download $id from its notification", e)
            null
        }
    }

    // Exit: every download paused (each carries on from there next time),
    // the app's window closed and out of recents, the service stopped, then
    // Eddy's processes (the share popup's too) ended once the pause has
    // written its state.
    private fun exit() {
        Log.i(TAG, "exit from the notification")
        EddyNative.controlDownloads(0)
        mainActivity?.get()?.let { activity -> activity.runOnUiThread { activity.finishAndRemoveTask() } }
        handler.removeCallbacks(check)
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        stopSelf()
        handler.postDelayed({
            val manager = getSystemService(android.app.ActivityManager::class.java)
            manager?.runningAppProcesses?.filter { it.pid != android.os.Process.myPid() }?.forEach {
                android.os.Process.killProcess(it.pid)
            }
            android.os.Process.killProcess(android.os.Process.myPid())
        }, 1500)
    }

    private fun controlIntent(action: String): PendingIntent = PendingIntent.getService(
        this, action.hashCode(),
        Intent(this, DownloadService::class.java).setAction(action),
        PendingIntent.FLAG_IMMUTABLE,
    )

    private fun progressNotification(active: Int, name: String, permille: Int, kind: Int): android.app.Notification {
        val builder = NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            // The download's type, as on its card (one running only).
            .setLargeIcon(if (active == 1) TypeIcon.bitmap(this, kind) else null)
            .setContentIntent(openAppIntent())
            .setOngoing(true)
            .setSilent(true)
        if (active == 0) {
            builder.setContentTitle(getString(R.string.app_name))
                .setContentText(getString(R.string.no_active_downloads))
        } else {
            val title = if (active == 1 && name.isNotEmpty()) name
                else resources.getQuantityString(R.plurals.downloading_count, active, active)
            builder.setContentTitle(title)
            if (permille >= 0) {
                builder.setContentText("${permille / 10}%").setProgress(1000, permille, false)
            } else {
                builder.setContentText(getString(R.string.starting_download)).setProgress(0, 0, true)
            }
            val several = active > 1
            builder.addAction(
                R.drawable.ic_notification,
                getString(if (several) R.string.pause_all else R.string.pause),
                controlIntent(ACTION_PAUSE),
            )
            builder.addAction(
                R.drawable.ic_notification,
                getString(if (several) R.string.cancel_all else R.string.cancel),
                controlIntent(ACTION_CANCEL),
            )
        }
        builder.addAction(R.drawable.ic_notification, getString(R.string.exit), controlIntent(ACTION_EXIT))
        return builder.build()
    }

    // One notification per download that finished or failed since last time.
    private fun postResults() {
        val finished = EddyNative.takeFinished() ?: return
        for (line in finished.split('\n')) {
            val parts = line.split('\t')
            if (parts.size < 2) continue
            val ok = parts[0] == "1"
            val name = parts[1]
            val message = parts.getOrNull(2) ?: ""
            val staging = parts.getOrNull(3) ?: ""
            val tree = parts.getOrNull(4) ?: ""
            val id = parts.getOrNull(5)?.toLongOrNull() ?: 0L
            // More of a finished download's files: into its folder there.
            val merge = parts.getOrNull(6) == "1"
            val kind = parts.getOrNull(7)?.toIntOrNull() ?: -1
            if (ok && staging.isNotEmpty() && tree.isNotEmpty()) {
                // Copy into the picked folder off the main thread, then say so.
                copying += 1
                Thread {
                    val delivered = TreeCopy.deliver(applicationContext, java.io.File(staging), tree, merge)
                    // Where it ended up, so the list can open it there.
                    delivered.uri?.let { EddyNative.delivered(id, it.toString()) }
                    handler.post {
                        copying -= 1
                        postResult(delivered.error == null, name, delivered.error ?: "", id, kind)
                    }
                }.start()
                continue
            }
            postResult(ok, name, message, id, kind)
        }
    }

    // Downloaded: a tap opens it (as its card in the list does); failed, the
    // app.
    private fun postResult(ok: Boolean, name: String, message: String, id: Long, kind: Int) {
        Haptics.play(this, if (ok) Haptics.DOWNLOADED else Haptics.FAILED)
        val notification = NotificationCompat.Builder(this, RESULTS_CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setLargeIcon(TypeIcon.bitmap(this, kind))
            .setContentTitle(getString(if (ok) R.string.downloaded_name else R.string.download_failed_name, name))
            .setContentText(if (ok) null else message)
            .setStyle(if (ok) null else NotificationCompat.BigTextStyle().bigText(message))
            .setContentIntent(if (ok && id != 0L) openFileIntent(id) ?: openDownloadIntent(id) else openAppIntent())
            .setAutoCancel(true)
            .build()
        notificationManager().notify(nextResultId++, notification)
    }
}
