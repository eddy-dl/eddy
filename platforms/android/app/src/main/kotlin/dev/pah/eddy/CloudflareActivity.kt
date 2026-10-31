package dev.pah.eddy

import android.app.Activity
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.ViewGroup
import android.webkit.CookieManager
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.LinearLayout
import android.widget.TextView
import androidx.core.app.NotificationCompat
import java.io.File

// A site behind a bot check (Cloudflare's "Just a moment…", Anubis's "Making
// sure you're not a bot!"), opened in a WebView so the check is passed as in
// a browser: the check's script runs, and asks for a tap on its box if it
// wants one. The cookies it leaves and this WebView's user agent (they only
// work with it) then go to Rust through a file, which Rust waits on in
// whichever process asked: line one the user agent, line two the cookies,
// line three "checked" when a check showed before the page (else the
// WebView had a pass already). Closed without passing, the file is left
// empty.
class CloudflareActivity : Activity() {
    companion object {
        private const val TAG = "Eddy"
        const val EXTRA_URL = "url"
        const val EXTRA_RESULT = "result"
        private const val CHANNEL_ID = "checks"
        private const val NOTIFICATION_ID = 7001
        private const val POLL_MS = 1000L
        // "loading" until the page is in (its ads and images may load for
        // long after), then "check" for a bot check, Cloudflare's ("Just a
        // moment…") or Anubis's ("Making sure you're not a bot!"), else
        // "page".
        // (PageFetcher asks the same.)
        const val PAGE_STATE = "(function() { if (document.readyState == 'loading') return 'loading';" +
            // Cloudflare's title is in the phone's language and its page has
            // lost the form: _cf_chl_opt is set on its check page only.
            " return typeof window._cf_chl_opt != 'undefined' || document.title.indexOf('Just a moment') >= 0" +
            " || !!document.getElementById('challenge-form') || !!document.getElementById('challenge-error-text')" +
            " || !!document.getElementById('anubis_challenge') ? 'check' : 'page'; })()"

        // Opens the check: straight away when Eddy is on screen, else (a
        // download in the background) through a notification, as Android
        // doesn't let a background app open a window.
        fun show(context: Context, url: String, result: String, onScreen: Boolean) {
            val intent = Intent(context, CloudflareActivity::class.java)
                .putExtra(EXTRA_URL, url)
                .putExtra(EXTRA_RESULT, result)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            if (onScreen) {
                context.startActivity(intent)
                return
            }
            val manager = context.getSystemService(NotificationManager::class.java)
            manager.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, context.getString(R.string.checks_channel), NotificationManager.IMPORTANCE_HIGH)
            )
            val host = android.net.Uri.parse(url).host?.removePrefix("www.") ?: url
            val notification = NotificationCompat.Builder(context, CHANNEL_ID)
                .setSmallIcon(R.drawable.ic_notification)
                .setContentTitle(context.getString(R.string.check_needed, host))
                .setContentText(context.getString(R.string.check_tap))
                .setAutoCancel(true)
                .setContentIntent(
                    PendingIntent.getActivity(context, NOTIFICATION_ID, intent, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
                )
                .build()
            manager.notify(NOTIFICATION_ID, notification)
        }
    }

    private val handler = Handler(Looper.getMainLooper())
    private lateinit var web: WebView
    private var url = ""
    private var result: File? = null
    private var done = false
    // A check showed before the page.
    private var sawCheck = false

    private val poll = object : Runnable {
        override fun run() {
            if (done) return
            web.evaluateJavascript(PAGE_STATE) { state ->
                if (state == "\"check\"") sawCheck = true
                val cookies = CookieManager.getInstance().getCookie(url).orEmpty()
                // Passed: the site's own page has loaded, with the cookies
                // the check left (cf_clearance, Anubis's pass).
                if (state == "\"page\"" && cookies.isNotEmpty()) {
                    finishWith(web.settings.userAgentString + "\n" + cookies + "\n" + (if (sawCheck) "checked" else "none"))
                } else {
                    handler.postDelayed(this, POLL_MS)
                }
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        getSystemService(NotificationManager::class.java).cancel(NOTIFICATION_ID)
        url = intent.getStringExtra(EXTRA_URL).orEmpty()
        result = intent.getStringExtra(EXTRA_RESULT)?.let(::File)

        val hint = TextView(this).apply {
            text = getString(R.string.check_hint)
            setPadding(48, 48, 48, 24)
            textSize = 15f
        }
        web = WebView(this).apply {
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            webViewClient = object : WebViewClient() {
                // A page in: checked now rather than at the next poll.
                override fun onPageFinished(view: WebView, url: String) {
                    handler.removeCallbacks(poll)
                    handler.post(poll)
                }
            }
            layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f)
        }
        CookieManager.getInstance().setAcceptCookie(true)
        CookieManager.getInstance().setAcceptThirdPartyCookies(web, true)
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            fitsSystemWindows = true
            addView(hint)
            addView(web)
        })
        Log.i(TAG, "browser check for $url")
        web.loadUrl(url)
        handler.postDelayed(poll, POLL_MS)
    }

    private fun finishWith(text: String) {
        if (done) return
        done = true
        handler.removeCallbacks(poll)
        CookieManager.getInstance().flush()
        result?.let { file ->
            try {
                file.parentFile?.mkdirs()
                // Written whole, then moved into place: Rust never reads half of it.
                val part = File(file.path + ".part")
                part.writeText(text)
                part.renameTo(file)
            } catch (e: Exception) {
                Log.w(TAG, "couldn't hand the browser check over", e)
            }
        }
        if (text.isNotEmpty()) Log.i(TAG, "browser check passed for $url")
        finish()
    }

    override fun onDestroy() {
        // Closed without passing: Rust stops waiting.
        finishWith("")
        web.destroy()
        super.onDestroy()
    }
}
