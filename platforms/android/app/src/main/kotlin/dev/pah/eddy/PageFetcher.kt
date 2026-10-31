package dev.pah.eddy

import android.annotation.SuppressLint
import android.content.Context
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.webkit.CookieManager
import android.webkit.WebView
import android.webkit.WebViewClient
import org.json.JSONObject
import org.json.JSONTokener
import java.io.File

// Pages of a site that lets the browser in but turns Eddy's own requests
// away, even with the browser's pass (Cloudflare judges the connection, not
// just the cookies): loaded in a WebView nobody sees, in this process like
// CloudflareActivity, so with the same cookies and pass, and their HTML
// handed to Rust (novels::fetch) through a file, one page at a time. The
// file's first line says how it went: "page" (the HTML follows), "check"
// (a bot check that wants the user: Rust opens CloudflareActivity, then
// asks again) or "error <why>".
//
// One WebView serves every page. Once it has a page of the site open, the
// next pages are asked for from inside it (fetch(), with the browser's
// cookies and connection): only the HTML comes, without the ads and scripts
// that can keep a page "loading" for a minute on a slow connection. A page is
// loaded in full only when that fails (a bot check, the first page of a site),
// and even then its HTML is asked for the same way as soon as the page is on
// the site, without waiting for the rest of it. The page before stays in the
// WebView until the next one comes in, ready as it is: it's marked before
// each load, and a marked page is never the one handed over.
object PageFetcher {
    private const val TAG = "Eddy"
    private const val POLL_MS = 500L
    // A check that hasn't passed by itself in this long wants a tap.
    private const val CHECK_MS = 15_000L
    private const val TIMEOUT_MS = 60_000L
    // The WebView goes once nothing's been asked for this long.
    private const val IDLE_MS = 120_000L
    // Set on the page before a load: the new page doesn't have it.
    private const val MARK_OLD = "window.eddyOldPage = true"
    // 'here': the new page is on the site, still loading. A new WebView's
    // blank page, there until the first page comes in, is no page either.
    // 'failed': the WebView's own error page (no connection, say).
    private const val STATE = "(function(o) { if (window.eddyOldPage || location.href == 'about:blank') return 'old';" +
        " if (location.protocol == 'chrome-error:') return 'failed'; var s = " + CloudflareActivity.PAGE_STATE +
        "; return s == 'loading' && location.origin == o ? 'here' : s; })"
    // Asks for a page from inside the one open (only on the same site), and
    // keeps the answer, or false for none or a bot check, in eddyFetch.
    private const val FETCH = "(function(u, o, n) { window.eddyFetch = {n: n};" +
        " var no = function() { window.eddyFetch = {n: n, ok: false}; };" +
        " if (location.origin != o) { no(); return; }" +
        " fetch(u, {credentials: 'include'}).then(function(r) { return r.text().then(function(t) {" +
        " var check = /id=\"anubis_challenge\"|_cf_chl_opt|id=\"challenge-form\"|<title>Just a moment/.test(t);" +
        " window.eddyFetch = {n: n, ok: r.ok && !check, text: t}; }); }).catch(no); })"
    // null: the page that asked has gone.
    private const val FETCHED = "(function(n) { var f = window.eddyFetch; if (!f || f.n !== n) return null;" +
        " if (!('ok' in f)) return 'wait'; return f.ok ? 'page\\n' + f.text : 'failed'; })"

    private enum class Ask { NONE, ASKING, FAILED }

    private val handler = Handler(Looper.getMainLooper())
    private val queue = ArrayDeque<Pair<String, File>>()
    private var web: WebView? = null
    private var current: Pair<String, File>? = null
    private var began = 0L
    private var checkSince = 0L
    private var serial = 0
    private var ask = Ask.NONE
    private var loading = false

    fun fetch(context: Context, url: String, result: String) {
        val app = context.applicationContext
        handler.post {
            queue.addLast(url to File(result))
            if (current == null) next(app)
        }
    }

    private fun next(context: Context) {
        handler.removeCallbacks(idle)
        val job = queue.removeFirstOrNull()
        current = job
        if (job == null) {
            handler.postDelayed(idle, IDLE_MS)
            return
        }
        began = System.currentTimeMillis()
        checkSince = 0L
        serial++
        ask = Ask.NONE
        loading = false
        Log.i(TAG, "fetching ${job.first} in the background browser")
        val view = web
        if (view == null) {
            load(context, job, create(context).also { web = it })
        } else {
            // The page open may be on the same site: ask from it first.
            askFor(job, view)
            handler.postDelayed({ poll(context, job) }, POLL_MS)
        }
    }

    private fun load(context: Context, job: Pair<String, File>, view: WebView) {
        loading = true
        val go = {
            view.loadUrl(job.first)
            handler.postDelayed({ poll(context, job) }, POLL_MS)
        }
        // A new WebView has no page before.
        if (view.url == null) go() else view.evaluateJavascript(MARK_OLD) { if (current === job) go() }
    }

    private fun origin(url: String): String {
        val uri = Uri.parse(url)
        return uri.scheme + "://" + uri.host + if (uri.port != -1) ":" + uri.port else ""
    }

    private fun askFor(job: Pair<String, File>, view: WebView) {
        ask = Ask.ASKING
        view.evaluateJavascript(FETCH + "(" + JSONObject.quote(job.first) + ", " + JSONObject.quote(origin(job.first)) + ", " + serial + ")", null)
    }

    @SuppressLint("SetJavaScriptEnabled")
    private fun create(context: Context): WebView = WebView(context).apply {
        settings.javaScriptEnabled = true
        settings.domStorageEnabled = true
        // Only the HTML is wanted.
        settings.loadsImagesAutomatically = false
        settings.blockNetworkImage = true
        webViewClient = WebViewClient()
        CookieManager.getInstance().setAcceptCookie(true)
        CookieManager.getInstance().setAcceptThirdPartyCookies(this, true)
    }

    private fun poll(context: Context, job: Pair<String, File>) {
        if (current !== job) return
        val view = web ?: return
        val now = System.currentTimeMillis()
        if (now - began > TIMEOUT_MS) {
            finish(context, job, "error it didn't load in ${TIMEOUT_MS / 1000} s")
            return
        }
        val again = { handler.postDelayed({ poll(context, job) }, POLL_MS) }
        if (ask == Ask.ASKING) {
            view.evaluateJavascript(FETCHED + "(" + serial + ")") { json ->
                if (current !== job) return@evaluateJavascript
                val answer = try { JSONTokener(json).nextValue() as? String } catch (e: Exception) { null }
                when {
                    answer == null -> { ask = Ask.NONE; again() }
                    answer == "wait" -> again()
                    answer.startsWith("page\n") -> finish(context, job, answer)
                    else -> {
                        ask = Ask.FAILED
                        if (loading) again() else load(context, job, view)
                    }
                }
            }
            return
        }
        view.evaluateJavascript(STATE + "(" + JSONObject.quote(origin(job.first)) + ")") { state ->
            if (current !== job) return@evaluateJavascript
            when (state) {
                "\"page\"" -> view.evaluateJavascript("document.documentElement.outerHTML") { json ->
                    if (current !== job) return@evaluateJavascript
                    val html = try { JSONTokener(json).nextValue() as? String } catch (e: Exception) { null }
                    finish(context, job, if (html.isNullOrEmpty()) "error its HTML didn't come out (${json.length} characters)" else "page\n" + html)
                }
                "\"here\"" -> {
                    // On the site but still loading (ads, slow scripts): ask
                    // for the HTML now rather than wait for the rest.
                    if (ask == Ask.NONE) askFor(job, view)
                    again()
                }
                "\"failed\"" -> finish(context, job, "error the page didn't load (no connection?)")
                "\"check\"" -> {
                    // The check page: once it passes, the site's page comes
                    // in and can be asked from again.
                    ask = Ask.NONE
                    if (checkSince == 0L) checkSince = now
                    if (now - checkSince > CHECK_MS) finish(context, job, "check")
                    else again()
                }
                else -> again()
            }
        }
    }

    private fun finish(context: Context, job: Pair<String, File>, text: String) {
        if (current !== job) return
        val file = job.second
        try {
            file.parentFile?.mkdirs()
            // Written whole, then moved into place: Rust never reads half of it.
            val part = File(file.path + ".part")
            part.writeText(text)
            part.renameTo(file)
        } catch (e: Exception) {
            Log.w(TAG, "couldn't hand the page over", e)
        }
        web?.stopLoading()
        next(context)
    }

    private val idle = Runnable {
        web?.destroy()
        web = null
    }
}
