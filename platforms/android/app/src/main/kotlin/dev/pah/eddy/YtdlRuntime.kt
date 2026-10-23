package dev.pah.eddy

import android.content.Context
import android.system.Os
import org.apache.commons.compress.archivers.zip.ZipFile
import org.json.JSONObject
import java.io.File

// yt-dlp on Android, through YTDLnis's helper apps (deniscerri/ytdlnis-packages),
// which the user installs: Python (with the yt-dlp script and QuickJS),
// ffmpeg and Deno (optional: a better JS runtime for yt-dlp, and needed by
// the Instagram extractor's SnapSave fallback), built from termux-packages, each shipped as libxxx.so (the
// program) plus libxxx.zip.so (its usr/ tree). Android only lets an app run
// programs from an installed APK's native library folder, which is why
// they're dressed up as .so files. The zips are unpacked into eddy's
// storage, and the programs run from the helper's library folder with
// LD_LIBRARY_PATH pointing at the unpacked libraries. eddy bundles none of it.
object YtdlRuntime {
    private const val BASE = "ytdl"

    private class Package(val binDir: File, val exe: File, val ldDir: File)

    const val PYTHON_APP = "com.deniscerri.ytdl.python"
    const val FFMPEG_APP = "com.deniscerri.ytdl.ffmpeg"
    const val DENO_APP = "com.deniscerri.ytdl.deno"

    // Finds a helper app's package, unpacking its zip when it's new or
    // changed (the helper was updated). Null if it isn't installed.
    private fun find(context: Context, exeName: String, zipName: String, helperApp: String, folder: String): Package? {
        val binDir = try {
            File(context.packageManager.getApplicationInfo(helperApp, 0).nativeLibraryDir)
        } catch (e: Exception) {
            return null
        }
        val ldDir = File(context.noBackupFilesDir, "$BASE/$folder")
        val exe = File(binDir, exeName)
        if (!exe.exists()) return null

        val zip = File(binDir, zipName)
        if (zip.exists()) {
            // Unpacked again only when the zip changes (an update).
            val stamp = "${zip.length()}-${zip.lastModified()}"
            val marker = File(ldDir, ".eddy-unpacked")
            if (!marker.exists() || marker.readText() != stamp) {
                ldDir.deleteRecursively()
                ldDir.mkdirs()
                unzip(zip, ldDir)
                marker.writeText(stamp)
            }
        }
        return Package(binDir, exe, ldDir)
    }

    // Keeps the zip's symlinks (termux packages are full of them).
    private fun unzip(zip: File, target: File) {
        val root = target.canonicalPath + File.separator
        ZipFile.builder().setFile(zip).get().use { file ->
            for (entry in file.entries) {
                val out = File(target, entry.name)
                if (!out.canonicalPath.startsWith(root)) continue // zip slip
                when {
                    entry.isDirectory -> out.mkdirs()
                    entry.isUnixSymlink -> {
                        out.parentFile?.mkdirs()
                        val link = file.getInputStream(entry).use { it.readBytes().toString(Charsets.UTF_8) }
                        try {
                            Os.symlink(link, out.absolutePath)
                        } catch (e: Exception) {
                            // Already there from an earlier, interrupted unpack.
                        }
                    }
                    else -> {
                        out.parentFile?.mkdirs()
                        file.getInputStream(entry).use { input -> out.outputStream().use { input.copyTo(it) } }
                    }
                }
            }
        }
    }

    // The yt-dlp script (a zipapp the Python helper carries as a raw
    // resource), copied out whenever the helper's changes.
    private fun ytdlpScript(context: Context): File? {
        val resources = try {
            context.packageManager.getResourcesForApplication(PYTHON_APP)
        } catch (e: Exception) {
            return null
        }
        val id = resources.getIdentifier("ytdlp", "raw", PYTHON_APP)
        if (id == 0) return null
        val script = File(context.noBackupFilesDir, "$BASE/yt-dlp/yt-dlp")
        val size = resources.openRawResourceFd(id).use { it.length }
        if (!script.exists() || script.length() != size) {
            script.parentFile?.mkdirs()
            resources.openRawResource(id).use { input -> script.outputStream().use { input.copyTo(it) } }
        }
        return script
    }

    // Everything Rust needs to run yt-dlp and ffmpeg, as JSON (empty
    // object when Python isn't there). One unpack at a time: the app, the
    // service and the share popup (another process) can all get here.
    @Synchronized
    fun setup(context: Context): String {
        val base = File(context.noBackupFilesDir, BASE).apply { mkdirs() }
        return java.io.RandomAccessFile(File(base, ".lock"), "rw").use { lockFile ->
            lockFile.channel.lock().use { describe(context) }
        }
    }

    private fun describe(context: Context): String {
        val json = JSONObject()
        val python = find(context, "libpython.so", "libpython.zip.so", PYTHON_APP, "python")
        val ffmpeg = find(context, "libffmpeg.so", "libffmpeg.zip.so", FFMPEG_APP, "ffmpeg")
        val deno = find(context, "libdeno.so", "libdeno.zip.so", DENO_APP, "deno")
        val script = ytdlpScript(context)

        val libDirs = listOfNotNull(python, ffmpeg, deno).flatMap {
            val usrLib = File(it.ldDir, "usr/lib")
            listOf(if (usrLib.exists()) usrLib.absolutePath else it.ldDir.absolutePath, it.binDir.absolutePath)
        }
        val binDirs = listOfNotNull(python, ffmpeg, deno).map { it.binDir.absolutePath } + (System.getenv("PATH") ?: "/system/bin")
        val env = JSONObject()
            .put("LD_LIBRARY_PATH", libDirs.distinct().joinToString(":"))
            .put("PATH", binDirs.distinct().joinToString(":"))
            .put("TMPDIR", context.cacheDir.absolutePath)
            // Deno keeps compiled scripts here.
            .put("DENO_DIR", File(context.cacheDir, "deno").absolutePath)

        // Python whether or not its package carries yt-dlp: newer packages
        // don't, and Eddy downloads yt-dlp itself (components.rs).
        if (python != null) {
            val usr = File(python.ldDir, "usr")
            env.put("PYTHONHOME", usr.absolutePath)
                .put("HOME", usr.absolutePath)
                .put("SSL_CERT_FILE", File(usr, "etc/tls/cert.pem").absolutePath)
            json.put("python", python.exe.absolutePath)
            script?.let { json.put("ytdlp", it.absolutePath) }
            // QuickJS comes with the Python package; yt-dlp needs a JS runtime
            // for YouTube.
            File(python.binDir, "libqjs.so").takeIf { it.exists() }?.let { json.put("quickjs", it.absolutePath) }
        }
        ffmpeg?.let { json.put("ffmpeg", it.exe.absolutePath) }
        deno?.let { json.put("deno", it.exe.absolutePath) }

        // For Settings → Downloads: which helpers are installed (their
        // versions), and which builds fit this device.
        val installed = JSONObject()
        for ((key, app) in listOf("python" to PYTHON_APP, "ffmpeg" to FFMPEG_APP, "deno" to DENO_APP)) {
            try {
                @Suppress("DEPRECATION")
                installed.put(key, context.packageManager.getPackageInfo(app, 0).versionName ?: "")
            } catch (e: Exception) {
                // Not installed.
            }
        }
        json.put("installed", installed)
        json.put("abi", android.os.Build.SUPPORTED_ABIS.firstOrNull() ?: "arm64-v8a")
        json.put("env", env)
        return json.toString()
    }

    // Sets up in the background (the first unpack takes a moment) and hands
    // the result to Rust.
    fun start(context: Context) {
        val app = context.applicationContext
        Thread {
            try {
                EddyNative.setRuntime(setup(app))
            } catch (e: Exception) {
                android.util.Log.e("eddy", "couldn't set up yt-dlp", e)
            }
        }.start()
    }
}
