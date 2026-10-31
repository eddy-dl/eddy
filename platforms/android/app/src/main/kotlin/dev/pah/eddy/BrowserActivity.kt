package dev.pah.eddy

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.util.Log
import android.view.ViewGroup
import android.webkit.CookieManager
import android.webkit.URLUtil
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import org.json.JSONObject
import java.io.File

// A page whose file only comes from its own download button (a countdown,
// a captcha, a link made when it's clicked), opened so the user can click
// through it as in a browser. The download it starts isn't saved here: its
// link, the cookies for it, this WebView's user agent and the page it was
// clicked on go to Rust (crate::browser) through a file, as JSON, and
// Eddy's downloader fetches it. Closed without one, the file is left empty.
class BrowserActivity : Activity() {
    companion object {
        private const val TAG = "Eddy"
        const val EXTRA_PAGE = "page"
        const val EXTRA_RESULT = "result"

        fun show(context: Context, page: String, result: String) {
            context.startActivity(
                Intent(context, BrowserActivity::class.java)
                    .putExtra(EXTRA_PAGE, page)
                    .putExtra(EXTRA_RESULT, result)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            )
        }
    }

    private lateinit var web: WebView
    private var result: File? = null
    private var done = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val page = intent.getStringExtra(EXTRA_PAGE).orEmpty()
        result = intent.getStringExtra(EXTRA_RESULT)?.let(::File)

        val hint = TextView(this).apply {
            text = getString(R.string.browser_hint)
            setPadding(48, 48, 48, 24)
            textSize = 15f
        }
        web = WebView(this).apply {
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            // A download button's new tab or pop-up opens here instead.
            settings.setSupportMultipleWindows(false)
            settings.javaScriptCanOpenWindowsAutomatically = true
            webViewClient = object : WebViewClient() {
                // Pages only: other apps' links (intent:, market:) stay shut.
                override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                    val scheme = request.url.scheme?.lowercase()
                    return scheme != "http" && scheme != "https"
                }
            }
            setDownloadListener { url, userAgent, contentDisposition, mimetype, length ->
                caught(url, userAgent, contentDisposition, mimetype, length)
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
        Log.i(TAG, "browser for $page")
        web.loadUrl(page)
    }

    // The page started a download: handed over, unless it's made inside the
    // page (blob:, data:), which there's no link to fetch again for.
    private fun caught(url: String, userAgent: String, contentDisposition: String?, mimetype: String?, length: Long) {
        val scheme = Uri.parse(url).scheme?.lowercase()
        if (scheme != "http" && scheme != "https") {
            Toast.makeText(this, getString(R.string.browser_made_in_page), Toast.LENGTH_LONG).show()
            return
        }
        val json = JSONObject()
            .put("url", url)
            .put("user_agent", userAgent)
            .put("cookies", CookieManager.getInstance().getCookie(url).orEmpty())
            .put("referer", web.url.orEmpty())
            .put("filename", URLUtil.guessFileName(url, contentDisposition, mimetype))
            .put("mime", mimetype.orEmpty())
            .put("size", if (length > 0) length else 0L)
        Log.i(TAG, "browser caught $url")
        finishWith(json.toString())
    }

    private fun finishWith(text: String) {
        if (done) return
        done = true
        CookieManager.getInstance().flush()
        result?.let { file ->
            try {
                file.parentFile?.mkdirs()
                // Written whole, then moved into place: Rust never reads half of it.
                val part = File(file.path + ".part")
                part.writeText(text)
                part.renameTo(file)
            } catch (e: Exception) {
                Log.w(TAG, "couldn't hand the download over", e)
            }
        }
        finish()
    }

    // Back goes back a page first, as a browser's does.
    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        if (web.canGoBack()) web.goBack() else super.onBackPressed()
    }

    override fun onDestroy() {
        // Closed without a download: Rust stops waiting.
        finishWith("")
        web.destroy()
        super.onDestroy()
    }
}
