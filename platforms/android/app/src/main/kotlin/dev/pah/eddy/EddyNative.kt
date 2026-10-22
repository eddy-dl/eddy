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

    // backend: 0 = direct, 1 = torrent, 2 = yt-dlp, 3 = auto. selectFiles is
    // a torrent's files to fetch ("1,3", 1-based), "" for all. Returns false
    // if it couldn't be started.
    // folder is a tree URI picked for this download, "" for the usual one.
    // `settings`: speed, connections, user agent, category folder, as JSON.
    @JvmStatic external fun startDownload(url: String, backend: Int, format: String, filename: String, selectFiles: String, folder: String, settings: String): Boolean

    // A folder was picked (FolderPickerActivity's result): its tree URI and
    // how to show it.
    @JvmStatic external fun folderSelected(uri: String, displayName: String)

    // Where yt-dlp, Python and ffmpeg are and the environment they need
    // (JSON from YtdlRuntime.setup).
    @JvmStatic external fun setRuntime(json: String)

    // Downloads still running or queued.
    @JvmStatic external fun activeDownloads(): Int

    // For the notification: "count\tfirst name\tprogress" (progress in
    // thousandths, -1 when unknown). Null if Rust couldn't make the string
    // (it logs why).
    @JvmStatic external fun status(): String?

    // Downloads that ended since the last call, one per line:
    // "1 or 0 (succeeded)\tname\tmessage\tstaging folder\tpicked folder's tree URI"
    // (the last two set when the files still need copying into that folder).
    // Null if Rust couldn't make the string (it logs why, and keeps them for
    // the next call).
    @JvmStatic external fun takeFinished(): String?

    // Saves shared text, or a shared file's links, as pending. Returns how
    // many links were added.
    @JvmStatic external fun addShared(text: String?, path: String?, name: String?): Int
}
