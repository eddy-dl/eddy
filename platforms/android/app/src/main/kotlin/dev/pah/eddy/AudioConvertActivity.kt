package dev.pah.eddy

import android.content.ContentValues
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.MediaStore
import android.view.Gravity
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.FileProvider
import org.json.JSONObject
import java.io.File

// "Convert to audio" from the share sheet: takes the sound out of a shared
// video with the ffmpeg helper app (the one yt-dlp uses, see YtdlRuntime) and
// saves it in Download/Eddy. Plain Android dialogs, no Slint: the original
// audio is copied as it is (seconds, nothing lost), or re-encoded to MP3 or
// AAC. Runs while the dialog is open; leaving it cancels.
class AudioConvertActivity : AppCompatActivity() {
    private enum class Format(val label: Int) { ORIGINAL(R.string.audio_original), MP3(R.string.audio_mp3), AAC(R.string.audio_aac) }

    // What an audio codec is saved as when it's copied: extension, MIME type.
    private val containers = mapOf(
        "aac" to ("m4a" to "audio/mp4"),
        "alac" to ("m4a" to "audio/mp4"),
        "mp3" to ("mp3" to "audio/mpeg"),
        "opus" to ("opus" to "audio/ogg"),
        "vorbis" to ("ogg" to "audio/ogg"),
        "flac" to ("flac" to "audio/flac"),
    )

    private var work: File? = null
    @Volatile private var process: Process? = null
    @Volatile private var cancelled = false
    private var progressBar: ProgressBar? = null
    private var progressText: TextView? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        @Suppress("DEPRECATION")
        val uri: Uri? = intent?.getParcelableExtra(Intent.EXTRA_STREAM) ?: intent?.data
        if (uri == null) {
            finishWith(getString(R.string.audio_no_video))
            return
        }
        AlertDialog.Builder(this)
            .setTitle(R.string.audio_convert)
            .setSingleChoiceItems(Format.values().map { getString(it.label) }.toTypedArray(), 0, null)
            .setPositiveButton(R.string.audio_convert_button) { dialog, _ ->
                val chosen = Format.values()[(dialog as AlertDialog).listView.checkedItemPosition]
                start(uri, chosen)
            }
            .setNegativeButton(R.string.cancel) { _, _ -> finish() }
            .setOnCancelListener { finish() }
            .show()
    }

    private fun start(uri: Uri, format: Format) {
        showProgress()
        Thread {
            val message = try {
                convert(uri, format)
            } catch (e: Exception) {
                android.util.Log.e("eddy", "audio conversion failed", e)
                getString(R.string.audio_failed, e.message ?: e.toString())
            }
            work?.deleteRecursively()
            runOnUiThread { if (!cancelled) finishWith(message) }
        }.start()
    }

    // Returns the message to show at the end.
    private fun convert(uri: Uri, format: Format): String {
        val runtime = JSONObject(YtdlRuntime.setup(applicationContext))
        val ffmpeg = runtime.optString("ffmpeg").takeIf { it.isNotEmpty() }
            ?: return getString(R.string.audio_no_ffmpeg)
        val env = runtime.getJSONObject("env").let { json -> json.keys().asSequence().associateWith { json.getString(it) } }

        val (inputPath, inputName) = SharedContent.copyToCache(this, uri)
            ?: return getString(R.string.audio_no_video)
        val input = File(inputPath)
        work = input.parentFile
        if (cancelled) return ""

        // The audio codec and length, from ffmpeg's description of the file.
        val info = run(ffmpeg, env, listOf("-hide_banner", "-i", input.absolutePath), null).second
        val codec = Regex("""Stream #\S+.*?: Audio: (\w+)""").find(info)?.groupValues?.get(1)
            ?: return getString(R.string.audio_no_sound)
        val duration = Regex("""Duration: (\d+):(\d+):(\d+(?:\.\d+)?)""").find(info)?.destructured
            ?.let { (h, m, s) -> ((h.toLong() * 3600 + m.toLong() * 60) * 1_000_000 + (s.toDouble() * 1_000_000).toLong()) }
            ?: 0L

        val (ext, mime) = when (format) {
            Format.ORIGINAL -> containers[codec] ?: ("mka" to "audio/x-matroska")
            Format.MP3 -> "mp3" to "audio/mpeg"
            Format.AAC -> "m4a" to "audio/mp4"
        }
        val codecArgs = when (format) {
            Format.ORIGINAL -> listOf("-c:a", "copy")
            Format.MP3 -> listOf("-c:a", "libmp3lame", "-q:a", "2")
            Format.AAC -> listOf("-c:a", "aac", "-b:a", "192k")
        }
        val name = inputName.substringBeforeLast('.').ifEmpty { "audio" } + ".$ext"
        val output = File(work, "out.$ext")
        val args = listOf("-hide_banner", "-nostats", "-y", "-i", input.absolutePath, "-map", "0:a:0", "-vn", "-map_metadata", "0") +
            codecArgs + listOf("-progress", "pipe:1", output.absolutePath)

        val (code, log) = run(ffmpeg, env, args, duration)
        if (cancelled) return ""
        if (code != 0 || !output.exists()) {
            val reason = log.lines().lastOrNull { it.isNotBlank() } ?: "ffmpeg exited with $code"
            return getString(R.string.audio_failed, reason)
        }
        val saved = save(output, name, mime)
        return getString(R.string.audio_saved, saved)
    }

    // Runs ffmpeg; returns its exit code and stderr. With `duration` (µs),
    // reads -progress lines from stdout to move the bar.
    private fun run(ffmpeg: String, env: Map<String, String>, args: List<String>, duration: Long?): Pair<Int, String> {
        val builder = ProcessBuilder(listOf(ffmpeg) + args)
        builder.environment().putAll(env)
        val proc = builder.start()
        process = proc
        val stderr = StringBuilder()
        val errReader = Thread { proc.errorStream.bufferedReader().forEachLine { synchronized(stderr) { stderr.appendLine(it) } } }
        errReader.start()
        proc.inputStream.bufferedReader().forEachLine { line ->
            if (duration != null && duration > 0 && line.startsWith("out_time_us=")) {
                line.substringAfter('=').toLongOrNull()?.let { done ->
                    val permille = (done * 1000 / duration).toInt().coerceIn(0, 1000)
                    runOnUiThread { showPercent(permille) }
                }
            }
        }
        val code = proc.waitFor()
        errReader.join()
        process = null
        return code to synchronized(stderr) { stderr.toString() }
    }

    // Copies the result into Download/Eddy; returns where it went, to show.
    private fun save(file: File, name: String, mime: String): String {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val values = ContentValues().apply {
                put(MediaStore.MediaColumns.DISPLAY_NAME, name)
                put(MediaStore.MediaColumns.MIME_TYPE, mime)
                put(MediaStore.MediaColumns.RELATIVE_PATH, "${Environment.DIRECTORY_DOWNLOADS}/Eddy")
                put(MediaStore.MediaColumns.IS_PENDING, 1)
            }
            val resolver = contentResolver
            val target = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                ?: throw IllegalStateException("couldn't create $name in Download/Eddy")
            try {
                resolver.openOutputStream(target)?.use { out -> file.inputStream().use { it.copyTo(out) } }
                    ?: throw IllegalStateException("couldn't write $name")
                values.clear()
                values.put(MediaStore.MediaColumns.IS_PENDING, 0)
                resolver.update(target, values, null, null)
            } catch (e: Exception) {
                resolver.delete(target, null, null)
                throw e
            }
            // MediaStore may have renamed it (" (1)") if the name was taken.
            val savedName = resolver.query(target, arrayOf(MediaStore.MediaColumns.DISPLAY_NAME), null, null, null)?.use {
                if (it.moveToFirst()) it.getString(0) else null
            } ?: name
            openTarget = target to mime
            return "Download/Eddy/$savedName"
        }
        @Suppress("DEPRECATION")
        val dir = File(Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS), "Eddy").apply { mkdirs() }
        var target = File(dir, name)
        var n = 1
        while (target.exists()) {
            target = File(dir, "${name.substringBeforeLast('.')} ($n).${name.substringAfterLast('.')}")
            n++
        }
        file.copyTo(target)
        openTarget = FileProvider.getUriForFile(this, "$packageName.files", target) to mime
        return "Download/Eddy/${target.name}"
    }

    // The saved file, for the Open button.
    private var openTarget: Pair<Uri, String>? = null

    private fun showProgress() {
        val pad = (24 * resources.displayMetrics.density).toInt()
        val bar = ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal).apply {
            max = 1000
            isIndeterminate = true
        }
        val text = TextView(this).apply { gravity = Gravity.END }
        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad / 2, pad, 0)
            addView(bar)
            addView(text)
        }
        progressBar = bar
        progressText = text
        AlertDialog.Builder(this)
            .setTitle(R.string.audio_converting)
            .setView(layout)
            .setCancelable(false)
            .setNegativeButton(R.string.cancel) { _, _ -> cancel() }
            .show()
    }

    private fun showPercent(permille: Int) {
        progressBar?.apply {
            isIndeterminate = false
            progress = permille
        }
        progressText?.text = "${permille / 10}%"
    }

    private fun finishWith(message: String) {
        if (isFinishing || isDestroyed) return
        val target = openTarget
        val dialog = AlertDialog.Builder(this)
            .setMessage(message)
            .setPositiveButton(android.R.string.ok) { _, _ -> finish() }
            .setOnCancelListener { finish() }
        if (target != null) {
            dialog.setNeutralButton(R.string.audio_open) { _, _ ->
                try {
                    startActivity(Intent(Intent.ACTION_VIEW).setDataAndType(target.first, target.second).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
                } catch (e: Exception) {
                    Toast.makeText(this, R.string.cant_open_file, Toast.LENGTH_SHORT).show()
                }
                finish()
            }
        }
        dialog.show()
    }

    private fun cancel() {
        cancelled = true
        process?.destroy()
        finish()
    }

    override fun onDestroy() {
        // Leaving (Back, or the system closing it) stops ffmpeg.
        if (isFinishing) {
            cancelled = true
            process?.destroy()
        }
        super.onDestroy()
    }
}
