package dev.pah.eddy

import android.content.Context

// Rust functions for the parts of the app that run without a NativeActivity
// (the download service and the background share target).
object EddyNative {
    init {
        System.loadLibrary("eddy")
    }

    // Sets up Rust (app context, HTTPS verifier, database). Safe to call more
    // than once.
    @JvmStatic external fun init(context: Context)

    // backend: 0 = direct, 1 = torrent, 2 = media, 3 = auto, 4 = book,
    // 5 = Google Drive, 6 = MEGA (Backend::from_index). selectFiles is
    // a torrent's files to fetch ("1,3", 1-based), "" for all. Returns false
    // if it couldn't be started.
    // folder is a tree URI picked for this download, "" for the usual one.
    // `settings`: the whole request as JSON (its record id, speed,
    // connections, user agent, folder); the other arguments say the same.
    @JvmStatic external fun startDownload(url: String, backend: Int, format: String, filename: String, selectFiles: String, folder: String, settings: String): Boolean

    // A folder was picked (FolderPickerActivity's result): its tree URI and
    // how to show it.
    @JvmStatic external fun folderSelected(uri: String, displayName: String)

    // Where yt-dlp, Python and ffmpeg are and the environment they need
    // (JSON from YtdlRuntime.setup).
    @JvmStatic external fun setRuntime(json: String)

    // The progress notification's buttons: 0 pauses, 1 cancels every
    // running or queued download.
    @JvmStatic external fun controlDownloads(action: Int)

    // Downloads still running or queued.
    // The app's or the share popup's screen is going for good: stops the
    // link checks it started (downloads go on).
    @JvmStatic external fun screenClosed()

    @JvmStatic external fun activeDownloads(): Int

    // For the notification: "count\tfirst name\tprogress" (progress in
    // thousandths, -1 when unknown). Null if Rust couldn't make the string
    // (it logs why).
    @JvmStatic external fun status(): String?

    // Downloads that ended since the last call, one per line:
    // "1 or 0 (succeeded)\tname\tmessage\tstaging folder\tpicked folder's tree URI\tid"
    // (the folders set when the files still need copying into that folder;
    // id is the download's record).
    // Null if Rust couldn't make the string (it logs why, and keeps them for
    // the next call).
    @JvmStatic external fun takeFinished(): String?

    // A finished download's copy in its picked folder (a document URI), so
    // the list can open it there.
    @JvmStatic external fun delivered(id: Long, uri: String)

    // The file a finished download saved (a path or a document URI), for its
    // notification to open; null for a folder.
    @JvmStatic external fun downloadFile(id: Long): String?

    // Opens a finished download (from its notification), as its card does.
    @JvmStatic external fun openDownload(id: Long)

    // Saves shared text, or a shared file's links, as pending. Returns how
    // many links were added.
    @JvmStatic external fun addShared(text: String?, path: String?, name: String?): Int
}
