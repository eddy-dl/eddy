package dev.pah.eddy

import android.annotation.SuppressLint
import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.webkit.CookieManager
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import android.webkit.WebViewRenderProcess
import android.webkit.WebViewRenderProcessClient
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
    // A page still unfinished this long after it began, however the WebView
    // is doing: polls only come back through its callbacks, which a hung
    // renderer never calls, so this doesn't wait for them.
    private const val WATCHDOG_MS = TIMEOUT_MS + 5_000L
    // A script call answers in milliseconds, even while a slow page loads:
    // unanswered this long, the WebView is stuck (not the network).
    private const val ANSWER_MS = 10_000L
    private const val WATCH_EVERY_MS = 2_000L
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
    private var watchdog: Runnable? = null
    // When the script call still unanswered went out (0: none).
    private var waitingSince = 0L

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
        waitingSince = 0L
        // Stuck (a script call unanswered, or the page far past its time):
        // this page fails and the WebView goes, so the pages after it get a
        // fresh one rather than waiting until the app restarts.
        val dog = object : Runnable {
            override fun run() {
                if (current !== job) return
                val now = System.currentTimeMillis()
                val unanswered = waitingSince != 0L && now - waitingSince > ANSWER_MS
                if (unanswered || now - began > WATCHDOG_MS) {
                    Log.w(TAG, "the background browser stopped answering on ${job.first}; starting a new one")
                    discard()
                    finish(context, job, "error the background browser stopped answering")
                } else {
                    handler.postDelayed(this, WATCH_EVERY_MS)
                }
            }
        }
        watchdog = dog
        handler.postDelayed(dog, WATCH_EVERY_MS)
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
        if (view.url == null) go() else eval(view, MARK_OLD) { if (current === job) go() }
    }

    private fun origin(url: String): String {
        val uri = Uri.parse(url)
        return uri.scheme + "://" + uri.host + if (uri.port != -1) ":" + uri.port else ""
    }

    private fun askFor(job: Pair<String, File>, view: WebView) {
        ask = Ask.ASKING
        eval(view, FETCH + "(" + JSONObject.quote(job.first) + ", " + JSONObject.quote(origin(job.first)) + ", " + serial + ")", null)
    }

    // A script call, noted until it answers (see the watchdog).
    private fun eval(view: WebView, js: String, callback: ((String) -> Unit)?) {
        waitingSince = System.currentTimeMillis()
        view.evaluateJavascript(js) { result ->
            waitingSince = 0L
            callback?.invoke(result)
        }
    }

    // The WebView, given up on: the next page makes a new one.
    private fun discard() {
        waitingSince = 0L
        val view = web ?: return
        web = null
        try {
            view.stopLoading()
            view.destroy()
        } catch (e: Exception) {
            Log.w(TAG, "couldn't close the background browser", e)
        }
    }

    @SuppressLint("SetJavaScriptEnabled")
    private fun create(context: Context): WebView = WebView(context).apply {
        settings.javaScriptEnabled = true
        settings.domStorageEnabled = true
        // Only the HTML is wanted.
        settings.loadsImagesAutomatically = false
        settings.blockNetworkImage = true
        // Android 10+ notices a renderer that stopped responding: it's ended,
        // which comes back as onRenderProcessGone below.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            setWebViewRenderProcessClient(object : WebViewRenderProcessClient() {
                override fun onRenderProcessUnresponsive(view: WebView, renderer: WebViewRenderProcess?) {
                    Log.w(TAG, "the background browser's renderer stopped responding; ending it")
                    renderer?.terminate()
                }
                override fun onRenderProcessResponsive(view: WebView, renderer: WebViewRenderProcess?) {}
            })
        }
        webViewClient = object : WebViewClient() {
            // The page itself didn't load (no connection, the name didn't
            // resolve…): said straight away rather than found by polling.
            override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) {
                val job = current ?: return
                if (request.isForMainFrame && web === view && loading) {
                    finish(context, job, "error the page didn't load (${error.description})")
                }
            }

            // Its renderer crashed or was killed for memory: that WebView is
            // done for (unhandled, Android would end Eddy with it). The page
            // fails, and the next gets a new WebView.
            override fun onRenderProcessGone(view: WebView, detail: android.webkit.RenderProcessGoneDetail): Boolean {
                Log.w(TAG, "the background browser's renderer went (crashed: ${detail.didCrash()})")
                if (web === view) {
                    web = null
                    view.destroy()
                    current?.let { finish(context, it, "error the background browser's page crashed") }
                } else {
                    view.destroy()
                }
                return true
            }
        }
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
            eval(view, FETCHED + "(" + serial + ")") { json ->
                if (current !== job) return@eval
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
        eval(view, STATE + "(" + JSONObject.quote(origin(job.first)) + ")") { state ->
            if (current !== job) return@eval
            when (state) {
                "\"page\"" -> eval(view, "document.documentElement.outerHTML") { json ->
                    if (current !== job) return@eval
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
        watchdog?.let { handler.removeCallbacks(it) }
        watchdog = null
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
