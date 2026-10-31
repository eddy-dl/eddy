use super::PrefError;
use jni::errors::LogErrorAndDefault;
use jni::objects::{JClass, JObject, JString};
use jni::refs::Global;
use jni::sys::{jboolean, jint};
use jni::{Env, EnvUnowned, JValue, JavaVM, jni_sig, jni_str};
use std::sync::{Mutex, Once, OnceLock};

// The running NativeActivity (main app or share popup). Replaced each time
// one starts, since Android can recreate it.
static ANDROID_APP: Mutex<Option<android_activity::AndroidApp>> = Mutex::new(None);
// The app context, for when Rust runs without a NativeActivity: the download
// service, or the share target adding links in the background.
static APP_CONTEXT: OnceLock<Global<JObject<'static>>> = OnceLock::new();
const PREFS_NAME: &str = "app_prefs";

/// Called from android_main, before anything else touches JNI.
pub fn init(app: android_activity::AndroidApp) {
    *ANDROID_APP.lock().unwrap_or_else(|e| e.into_inner()) = Some(app);
    init_backend();
}

/// Called when android_main returns: the activity is gone, so JNI calls fall
/// back to the app context (the download service may still be running).
pub fn deinit() {
    *ANDROID_APP.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Called from Kotlin (EddyNative.init) by the download service and share
/// target, which run without a NativeActivity.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_init<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    context: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> jni::errors::Result<()> {
            if APP_CONTEXT.get().is_none() {
                let _ = APP_CONTEXT.set(env.new_global_ref(&context)?);
            }
            Ok(())
        })
        .resolve::<LogErrorAndDefault>();
    init_backend();
}

/// One-time setup the background work needs, whichever way Rust started.
fn init_backend() {
    static BACKEND: Once = Once::new();
    BACKEND.call_once(|| {
        // android_main sets this up too, but the download service and share
        // target run without it; it's set up once either way.
        crate::logs::init();

        // HTTPS certificate checks (reqwest in aria2-core and the link checks)
        // go through Android's own verifier, which needs the app context to
        // reach its Java half. Without this the first HTTPS connection panics.
        let result = with_env(|env, _context| {
            let app_context = env.call_method(
                _context,
                jni_str!("getApplicationContext"),
                jni_sig!("()Landroid/content/Context;"),
                &[],
            )?
            .l()?;
            rustls_platform_verifier::android::init_with_env(env, app_context)?;
            Ok(())
        });
        if let Err(e) = result {
            log::error!("Failed to set up the HTTPS certificate verifier: {e}");
        }

        if let Err(e) = crate::db::init(get_app_data_dir()) {
            log::error!("Failed to initialize database: {e}");
        }
    });
}

/// Runs `f` with a JNI environment and the running activity (or, without
/// one, the app context), from any thread: the thread is attached to the
/// JVM if it isn't already. Java exceptions thrown inside come back as
/// errors. Activity-only methods fail when there's no activity.
fn with_env<T>(
    f: impl FnOnce(&mut Env, &JObject) -> Result<T, PrefError>,
) -> Result<T, PrefError> {
    let app = ANDROID_APP.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let vm = match &app {
        Some(app) => unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) },
        None => JavaVM::singleton()?,
    };
    vm.attach_current_thread(|env| match (&app, APP_CONTEXT.get()) {
        (Some(app), _) => {
            // android-activity owns this global reference to the activity, so
            // it's only borrowed here (a plain JObject, never deleted on drop).
            let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
            f(env, &activity)
        }
        (None, Some(context)) => f(env, context.as_obj()),
        (None, None) => Err(PrefError::Other("Android context not initialized yet".into())),
    })
}

/// A Java String result as a Rust String.
fn rust_string(env: &mut Env, obj: JObject) -> Result<String, PrefError> {
    let string = env.cast_local::<JString>(obj)?;
    Ok(string.try_to_string(env)?)
}

fn shared_preferences<'local>(
    env: &mut Env<'local>,
    activity: &JObject,
) -> Result<JObject<'local>, PrefError> {
    let name = env.new_string(PREFS_NAME)?;
    Ok(env
        .call_method(
            activity,
            jni_str!("getSharedPreferences"),
            jni_sig!("(Ljava/lang/String;I)Landroid/content/SharedPreferences;"),
            // MODE_MULTI_PROCESS: re-read when another of Eddy's processes
            // changed them (the share popup's copy otherwise stays as it
            // was when it first read them, e.g. category folders off).
            &[JValue::Object(&name), JValue::Int(4)],
        )?
        .l()?)
}

pub fn save_pref(key: &str, value: &str) -> Result<(), PrefError> {
    with_env(|env, activity| {
        let prefs = shared_preferences(env, activity)?;
        let editor = env
            .call_method(
                &prefs,
                jni_str!("edit"),
                jni_sig!("()Landroid/content/SharedPreferences$Editor;"),
                &[],
            )?
            .l()?;
        let jkey = env.new_string(key)?;
        let jval = env.new_string(value)?;
        env.call_method(
            &editor,
            jni_str!("putString"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/SharedPreferences$Editor;"),
            &[JValue::Object(&jkey), JValue::Object(&jval)],
        )?;
        env.call_method(&editor, jni_str!("apply"), jni_sig!("()V"), &[])?;
        Ok(())
    })
}

pub fn get_pref(key: &str, default: &str) -> Result<String, PrefError> {
    with_env(|env, activity| {
        let prefs = shared_preferences(env, activity)?;
        let jkey = env.new_string(key)?;
        let jdefault = env.new_string(default)?;
        let result = env
            .call_method(
                &prefs,
                jni_str!("getString"),
                jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                &[JValue::Object(&jkey), JValue::Object(&jdefault)],
            )?
            .l()?;
        rust_string(env, result)
    })
}

/// Gets the standard Android Downloads directory string.
pub fn get_default_download_location() -> String {
    with_env(|env, _activity| {
        let env_class = env.find_class(jni_str!("android/os/Environment"))?;
        let dir_downloads = env
            .get_static_field(
                &env_class,
                jni_str!("DIRECTORY_DOWNLOADS"),
                jni_sig!("Ljava/lang/String;"),
            )?
            .l()?;

        let file_obj = env
            .call_static_method(
                &env_class,
                jni_str!("getExternalStoragePublicDirectory"),
                jni_sig!("(Ljava/lang/String;)Ljava/io/File;"),
                &[JValue::Object(&dir_downloads)],
            )?
            .l()?;

        let path = env
            .call_method(
                &file_obj,
                jni_str!("getAbsolutePath"),
                jni_sig!("()Ljava/lang/String;"),
                &[],
            )?
            .l()?;
        rust_string(env, path)
    })
    .unwrap_or_else(|_| String::from("/storage/emulated/0/Download"))
}

/// Material You's main accent (Android 12+), for the System accent.
pub fn system_accent() -> Option<slint::Color> {
    system_color("system_accent1_500")
}

/// Material You's tertiary accent, the System accent's second colour (its
/// secondary palette is only a muted copy of the main one).
pub fn system_second_accent() -> Option<slint::Color> {
    system_color("system_accent3_500")
}

/// One of Android's system colours (`android.R.color.<name>`), if it has it.
fn system_color(name: &str) -> Option<slint::Color> {
    with_env(|env, activity| {
        let resources = env
            .call_method(
                activity,
                jni_str!("getResources"),
                jni_sig!("()Landroid/content/res/Resources;"),
                &[],
            )?
            .l()?;
        let name = env.new_string(name)?;
        let def_type = env.new_string("color")?;
        let def_pkg = env.new_string("android")?;
        let res_id = env
            .call_method(
                &resources,
                jni_str!("getIdentifier"),
                jni_sig!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I"),
                &[
                    JValue::Object(&name),
                    JValue::Object(&def_type),
                    JValue::Object(&def_pkg),
                ],
            )?
            .i()?;

        if res_id == 0 {
            return Ok(None);
        }

        let theme = env
            .call_method(
                activity,
                jni_str!("getTheme"),
                jni_sig!("()Landroid/content/res/Resources$Theme;"),
                &[],
            )?
            .l()?;
        let argb = env
            .call_method(
                &resources,
                jni_str!("getColor"),
                jni_sig!("(ILandroid/content/res/Resources$Theme;)I"),
                &[JValue::Int(res_id), JValue::Object(&theme)],
            )?
            .i()?;

        Ok(Some(slint::Color::from_argb_encoded(argb as u32)))
    })
    .ok()
    .flatten()
}

pub fn show_toast(message: &str) {
    let _ = with_env(|env, activity| {
        let j_message = env.new_string(message)?;
        env.call_method(
            activity,
            jni_str!("showToast"),
            jni_sig!("(Ljava/lang/String;)V"),
            &[JValue::Object(&j_message)],
        )?;
        Ok(())
    });
}

pub fn set_bar_icons_dark(dark: bool) {
    let _ = with_env(|env, activity| {
        env.call_method(
            activity,
            jni_str!("updateSystemBarsAppearance"),
            jni_sig!("(I)V"),
            &[JValue::Int(if dark { 1 } else { 0 })],
        )?;
        Ok(())
    });
}

/// Vibrates through the activity showing (the app or the share popup),
/// which checks the haptic feedback setting.
pub fn trigger_haptic(kind: super::Haptic) {
    let _ = with_env(|env, activity| {
        env.call_method(activity, jni_str!("triggerHaptic"), jni_sig!("(I)V"), &[JValue::Int(kind as i32)])?;
        Ok(())
    });
}

/// Asks the download service to open a site's Cloudflare check in a WebView
/// (CloudflareActivity), or to post a notification for it when Eddy isn't
/// on screen. The outcome is written to `result` (see novels::fetch). Works
/// from any of Eddy's processes; false if it couldn't be asked.
pub fn open_site_check(url: &str, result: &str) -> bool {
    ask_service("dev.pah.eddy.CHECK_SITE", &[("url", url), ("result", result)])
        .inspect_err(|e| log::warn!("couldn't ask for a browser check: {e:?}"))
        .is_ok()
}

/// Asks the download service to load `url` in the background browser
/// (PageFetcher), which writes how it went and the page's HTML to `result`
/// (see novels::fetch).
pub fn fetch_page(url: &str, result: &str) -> bool {
    ask_service("dev.pah.eddy.FETCH_PAGE", &[("url", url), ("result", result)])
        .inspect_err(|e| log::warn!("couldn't ask the background browser: {e:?}"))
        .is_ok()
}

/// Asks the download service to open `page` in Eddy's browser
/// (BrowserActivity), where the user clicks its download: the file's link
/// and what came with it are written to `result` (see crate::browser).
pub fn open_browser(page: &str, result: &str) -> bool {
    ask_service("dev.pah.eddy.BROWSE", &[("page", page), ("result", result)])
        .inspect_err(|e| log::warn!("couldn't open the browser: {e:?}"))
        .is_ok()
}

/// Starts the download service (if it isn't running) with `action` and
/// string `extras`.
fn ask_service(action: &str, extras: &[(&str, &str)]) -> Result<(), PrefError> {
    with_env(|env, context| {
        let package = env.call_method(context, jni_str!("getPackageName"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
        let intent_class = env.find_class(jni_str!("android/content/Intent"))?;
        let intent = env.new_object(&intent_class, jni_sig!("()V"), &[])?;
        let service = env.new_string("dev.pah.eddy.DownloadService")?;
        env.call_method(
            &intent,
            jni_str!("setClassName"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;"),
            &[JValue::Object(&package), JValue::Object(&service)],
        )?;
        let action = env.new_string(action)?;
        env.call_method(&intent, jni_str!("setAction"), jni_sig!("(Ljava/lang/String;)Landroid/content/Intent;"), &[JValue::Object(&action)])?;
        for (key, value) in extras {
            let key = env.new_string(key)?;
            let value = env.new_string(value)?;
            env.call_method(
                &intent,
                jni_str!("putExtra"),
                jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;"),
                &[JValue::Object(&key), JValue::Object(&value)],
            )?;
        }
        // The service may not be running yet (a book checked from the app).
        env.call_method(
            context,
            jni_str!("startForegroundService"),
            jni_sig!("(Landroid/content/Intent;)Landroid/content/ComponentName;"),
            &[JValue::Object(&intent)],
        )?;
        Ok(())
    })
}

pub fn open_native_url(url: &str) {
    let _ = with_env(|env, activity| {
        let url_jstring = env.new_string(url)?;
        let uri = env
            .call_static_method(
                jni_str!("android/net/Uri"),
                jni_str!("parse"),
                jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
                &[JValue::Object(&url_jstring)],
            )?
            .l()?;
        let intent_class = env.find_class(jni_str!("android/content/Intent"))?;
        let action_view = env
            .get_static_field(
                &intent_class,
                jni_str!("ACTION_VIEW"),
                jni_sig!("Ljava/lang/String;"),
            )?
            .l()?;
        let intent = env.new_object(
            &intent_class,
            jni_sig!("(Ljava/lang/String;Landroid/net/Uri;)V"),
            &[JValue::Object(&action_view), JValue::Object(&uri)],
        )?;
        env.call_method(
            activity,
            jni_str!("startActivity"),
            jni_sig!("(Landroid/content/Intent;)V"),
            &[JValue::Object(&intent)],
        )?;
        Ok(())
    });
}

static FOLDER_PICKER_CALLBACK: Mutex<Option<Box<dyn Fn(String, String) + Send + 'static>>> =
    Mutex::new(None);

pub fn pick_download_folder<F>(on_picked: F)
where
    F: Fn(String, String) + Send + 'static,
{
    // With "All files access", Eddy's own browser: Android's picker refuses
    // the top of internal storage and of Download.
    if has_all_files_access() {
        let current = crate::db::raw_download_path().filter(|p| p.starts_with('/'));
        crate::folders::browse(current.as_deref(), on_picked);
        return;
    }

    // Store the callback
    if let Ok(mut guard) = FOLDER_PICKER_CALLBACK.lock() {
        *guard = Some(Box::new(on_picked));
    }

    // Trigger the Android intent
    let result = with_env(|env, activity| {
        env.call_method(activity, jni_str!("openFolderPicker"), jni_sig!("()V"), &[])?;
        Ok(())
    });

    if let Err(e) = result {
        eprintln!("Failed to open folder picker: {e}");
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_folderSelected<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    uri_jstring: JString<'caller>,
    display_jstring: JString<'caller>,
) {
    let (uri_str, display_str) = unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            Ok((uri_jstring.try_to_string(env)?, display_jstring.try_to_string(env)?))
        })
        .resolve::<LogErrorAndDefault>();

    if uri_str.is_empty() {
        return;
    }

    // Retrieve and execute the stored callback
    if let Ok(mut guard) = FOLDER_PICKER_CALLBACK.lock() {
        if let Some(callback) = guard.take() {
            callback(uri_str, display_str);
        }
    }
}

static FILE_PICKER_CALLBACK: Mutex<Option<Box<dyn FnOnce(String, String) + Send + 'static>>> =
    Mutex::new(None);

/// Lets the user pick a .txt of links or a .torrent. `on_picked` gets a
/// readable copy's path and the file's display name, on the Java UI thread.
pub fn pick_link_file<F>(on_picked: F)
where
    F: FnOnce(String, String) + Send + 'static,
{
    if let Ok(mut guard) = FILE_PICKER_CALLBACK.lock() {
        *guard = Some(Box::new(on_picked));
    }

    let result = with_env(|env, activity| {
        env.call_method(activity, jni_str!("openFilePicker"), jni_sig!("()V"), &[])?;
        Ok(())
    });

    if let Err(e) = result {
        eprintln!("Failed to open file picker: {e}");
    }
}

/// Lets the user pick a cookies.txt (same picker). The picker's copy in the
/// cache goes once `on_picked` has read it: cookies are private.
pub fn pick_cookies_file<F>(on_picked: F)
where
    F: FnOnce(String, String) + Send + 'static,
{
    pick_link_file(move |path, name| {
        on_picked(path.clone(), name);
        let _ = std::fs::remove_file(&path);
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_MainActivity_onFilePicked<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _activity: JObject<'caller>,
    path_jstring: JString<'caller>,
    name_jstring: JString<'caller>,
) {
    let (path, name) = unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            Ok((path_jstring.try_to_string(env)?, name_jstring.try_to_string(env)?))
        })
        .resolve::<LogErrorAndDefault>();

    if path.is_empty() {
        return;
    }

    if let Ok(mut guard) = FILE_PICKER_CALLBACK.lock() {
        if let Some(callback) = guard.take() {
            callback(path, name);
        }
    }
}

/// Hands a download to the download service (via the running activity's
/// startDownload), which keeps the app alive until it finishes.
pub fn start_download(request: &crate::downloader::DownloadRequest) -> Result<(), String> {
    with_env(|env, activity| {
        let url = env.new_string(&request.url)?;
        let format = env.new_string(&request.format)?;
        let filename = env.new_string(&request.filename)?;
        let select_files = env.new_string(&request.select_files)?;
        let folder = env.new_string(&request.folder)?;
        // The whole request, record id included (a resumed download keeps
        // its place in the lists); the other arguments are for reading logs.
        let settings = env.new_string(serde_json::to_string(request).unwrap_or_default())?;
        env.call_method(
            activity,
            jni_str!("startDownload"),
            jni_sig!("(Ljava/lang/String;ILjava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"),
            &[
                JValue::Object(&url),
                JValue::Int(request.backend.index()),
                JValue::Object(&format),
                JValue::Object(&filename),
                JValue::Object(&select_files),
                JValue::Object(&folder),
                JValue::Object(&settings),
            ],
        )?;
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// The download service starting a download it was handed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_startDownload<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    url: JString<'caller>,
    backend: jint,
    format: JString<'caller>,
    filename: JString<'caller>,
    select_files: JString<'caller>,
    folder: JString<'caller>,
    settings: JString<'caller>,
) -> jboolean {
    let request = unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            Ok(Some(crate::downloader::DownloadRequest {
                url: url.try_to_string(env)?,
                backend: crate::downloader::Backend::from_index(backend),
                format: format.try_to_string(env)?,
                filename: filename.try_to_string(env)?,
                select_files: select_files.try_to_string(env)?,
                folder: folder.try_to_string(env)?,
                settings: Default::default(),
                id: 0,
                resume: false,
                unpicked: Vec::new(),
                choices: Vec::new(),
                browser: None,
            }))
            // The whole request as JSON, when it's there.
            .map(|request| {
                let full = settings.try_to_string(env).ok().and_then(|json| serde_json::from_str::<crate::downloader::DownloadRequest>(&json).ok());
                full.or(request)
            })
        })
        .resolve::<LogErrorAndDefault>();
    let Some(request) = request else {
        return false;
    };
    match crate::downloader::start(&request) {
        Ok(()) => true,
        Err(e) => {
            log::error!("Couldn't start {}: {e}", request.url);
            false
        }
    }
}

/// The notification's text: "count\tfirst name\tprogress\ttype" (progress in
/// thousandths, -1 when the sizes aren't known; type: the one running
/// download's FileType place, -1 for several).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_status<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    let (count, name, progress) = crate::downloader::summary();
    let permille = progress.map(|p| (p * 1000.0) as i64).unwrap_or(-1);
    let kind = crate::downloads::running_type().unwrap_or(-1);
    let text = format!("{count}\t{}\t{permille}\t{kind}", name.replace(['\t', '\n'], " "));
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> { env.new_string(text) })
        .resolve::<LogErrorAndDefault>()
}

/// Downloads that ended since the last call, one per line:
/// "1 or 0 (succeeded)\tname\tmessage\tstaging folder\tpicked folder's tree URI\tid\tmerge\ttype"
/// (type: FileType's place, -1 unknown).
/// The folder and URI are set when the files still have to be copied into the
/// picked folder (the service does that, then deletes the staging folder).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_takeFinished<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    let clean = |s: &str| s.replace(['\t', '\n'], " ");
    let finished = crate::downloader::take_finished();
    let text: Vec<String> = finished
        .iter()
        .map(|f| {
            let (staging, tree) = match &f.deliver {
                Some(d) => (d.staging.to_string_lossy().into_owned(), d.tree_uri.clone()),
                None => (String::new(), String::new()),
            };
            let merge = f.deliver.as_ref().is_some_and(|d| d.merge);
            let kind = crate::downloads::type_of(f.id).unwrap_or(-1);
            format!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t{kind}", f.ok as u8, clean(&f.name), clean(&f.message), staging, tree, f.id, merge as u8)
        })
        .collect();
    let text = text.join("\n");
    let text = unowned_env
        .with_env(|env| -> jni::errors::Result<_> { env.new_string(text) })
        .resolve::<LogErrorAndDefault>();
    // Kotlin gets null then (and skips this round); the results wait for
    // the next one rather than being lost.
    if text.is_null() {
        crate::downloader::return_finished(finished);
    }
    text
}

/// A staged download's copy in its picked folder (a document URI): the list
/// opens it from there.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_delivered<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jni::sys::jlong,
    uri: JString<'caller>,
) {
    let uri = unowned_env
        .with_env(|env| -> jni::errors::Result<_> { uri.try_to_string(env) })
        .resolve::<LogErrorAndDefault>();
    if !uri.is_empty() {
        crate::downloads::delivered(id, &uri);
    }
}

/// The file a finished download saved, for its notification to open (null
/// for a folder: the app shows those).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_downloadFile<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jni::sys::jlong,
) -> JString<'caller> {
    let Some(path) = crate::downloads::file_of(id) else {
        return JString::default();
    };
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> { env.new_string(path) })
        .resolve::<LogErrorAndDefault>()
}

/// A "Downloaded" notification tapped: its download opens, as its card does.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_openDownload<'caller>(
    _unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jni::sys::jlong,
) {
    crate::downloads::open_when_ready(id);
}

/// The progress notification's Pause (0) and Cancel (1): every running or
/// queued download.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_controlDownloads<'caller>(
    _unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    action: jint,
) {
    crate::downloads::control_all(action == 1);
}

/// The app's or the share popup's screen is going for good: link checks
/// running for it are stopped (downloads carry on in the service).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_screenClosed<'caller>(
    _unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) {
    crate::children::stop_checks();
}

/// How many downloads are still going, so the service knows when to stop.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_activeDownloads<'caller>(
    _unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jint {
    crate::downloader::active_count() as jint
}

/// What was shared to the share popup.
pub enum Shared {
    Text(String),
    /// A readable copy of a shared file, and its display name.
    File { path: String, name: String },
}

/// A Java String that may be null.
fn optional_string(env: &mut Env, obj: JObject) -> Result<Option<String>, PrefError> {
    if obj.is_null() {
        return Ok(None);
    }
    rust_string(env, obj).map(Some)
}

/// True when this NativeActivity is the share popup (SharePopupActivity)
/// rather than the main app.
pub fn is_share_popup() -> bool {
    with_env(|env, activity| {
        let class = env.call_method(activity, jni_str!("getClass"), jni_sig!("()Ljava/lang/Class;"), &[])?.l()?;
        let name = env.call_method(&class, jni_str!("getName"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
        Ok(rust_string(env, name)?.ends_with(".SharePopupActivity"))
    })
    .unwrap_or(false)
}

/// The share popup's mode (1 = download now, 2 = preview) and what was
/// shared, as SharePopupActivity read it from its intent.
pub fn popup_share() -> Option<(i32, Shared)> {
    with_env(|env, activity| {
        let mode = env.call_method(activity, jni_str!("shareMode"), jni_sig!("()I"), &[])?.i()?;
        let text = env.call_method(activity, jni_str!("sharedText"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
        let path = env.call_method(activity, jni_str!("sharedPath"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
        let name = env.call_method(activity, jni_str!("sharedName"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
        let shared = match (optional_string(env, text)?, optional_string(env, path)?) {
            (Some(text), _) => Some(Shared::Text(text)),
            (None, Some(path)) => {
                let name = optional_string(env, name)?.unwrap_or_default();
                Some(Shared::File { path, name })
            }
            (None, None) => None,
        };
        Ok(shared.map(|shared| (mode, shared)))
    })
    .ok()
    .flatten()
}

/// Closes the running activity (the share popup when it's done).
pub fn finish_activity() {
    let _ = with_env(|env, activity| {
        env.call_method(activity, jni_str!("finish"), jni_sig!("()V"), &[])?;
        Ok(())
    });
}

/// "Add to Eddy" from the share sheet: saves the shared links as pending,
/// without any UI. Returns how many were added.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_addShared<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    text: JString<'caller>,
    path: JString<'caller>,
    name: JString<'caller>,
) -> jint {
    let shared = unowned_env
        .with_env(|env| -> Result<_, PrefError> {
            let text = optional_string(env, text.into())?;
            let path = optional_string(env, path.into())?;
            let name = optional_string(env, name.into())?.unwrap_or_default();
            Ok(match (text, path) {
                (Some(text), _) => Some(Shared::Text(text)),
                (None, Some(path)) => Some(Shared::File { path, name }),
                (None, None) => None,
            })
        })
        .resolve::<LogErrorAndDefault>();
    match shared {
        Some(Shared::Text(text)) => crate::links::remember_text(&text) as jint,
        Some(Shared::File { path, name }) => crate::links::remember_file(&path, &name) as jint,
        None => 0,
    }
}

/// Whether downloads may go straight into any folder: "All files access"
/// on Android 11+, the storage permission before that.
pub fn has_all_files_access() -> bool {
    with_env(|env, activity| {
        Ok(env.call_method(activity, jni_str!("hasAllFilesAccess"), jni_sig!("()Z"), &[])?.z()?)
    })
    .unwrap_or(false)
}

/// Opens Android's screen to grant (or take back) "All files access".
pub fn request_all_files_access() {
    let _ = with_env(|env, activity| {
        env.call_method(activity, jni_str!("requestAllFilesAccess"), jni_sig!("()V"), &[])?;
        Ok(())
    });
}

/// Opens a downloaded file with whichever app handles its type (Android
/// asks if there's more than one).
pub fn open_file(path: &str) {
    let result = with_env(|env, activity| {
        let path = env.new_string(path)?;
        env.call_method(activity, jni_str!("openFile"), jni_sig!("(Ljava/lang/String;)V"), &[JValue::Object(&path)])?;
        Ok(())
    });
    if let Err(e) = result {
        log::error!("Couldn't open the file: {e}");
    }
}

/// Shares downloads through Android's share sheet: their files (paths or
/// document URIs), with `text` (their links) alongside, or the text alone.
pub fn share(files: &[String], text: &str) {
    let result = with_env(|env, activity| {
        let files = env.new_string(files.join("\n"))?;
        let text = env.new_string(text)?;
        env.call_method(
            activity,
            jni_str!("shareItems"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)V"),
            &[JValue::Object(&files), JValue::Object(&text)],
        )?;
        Ok(())
    });
    if let Err(e) = result {
        log::error!("Couldn't share: {e}");
    }
}

/// Opens `rel` ("sub/file", inside the download's folder) in a picked
/// folder's copy of a download (`doc`: that folder's document, or the
/// category folder it sits in).
pub fn open_in_folder(doc: &str, rel: &str) {
    let result = with_env(|env, activity| {
        let doc = env.new_string(doc)?;
        let rel = env.new_string(rel)?;
        env.call_method(
            activity,
            jni_str!("openInFolder"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)V"),
            &[JValue::Object(&doc), JValue::Object(&rel)],
        )?;
        Ok(())
    });
    if let Err(e) = result {
        log::error!("Couldn't open the file: {e}");
    }
}

/// Deletes a document (a download's copy in a picked folder).
pub fn delete_document(uri: &str) {
    let result = with_env(|env, activity| {
        let uri = env.new_string(uri)?;
        env.call_method(activity, jni_str!("deleteDocument"), jni_sig!("(Ljava/lang/String;)V"), &[JValue::Object(&uri)])?;
        Ok(())
    });
    if let Err(e) = result {
        log::error!("Couldn't delete the document: {e}");
    }
}

/// Opens a web page in the browser.
pub fn open_url(url: &str) {
    let result = with_env(|env, activity| {
        let url = env.new_string(url)?;
        env.call_method(activity, jni_str!("openUrl"), jni_sig!("(Ljava/lang/String;)V"), &[JValue::Object(&url)])?;
        Ok(())
    });
    if let Err(e) = result {
        log::error!("Couldn't open the link: {e}");
    }
}

pub fn get_app_data_dir() -> String {
    let result = with_env(|env, context| {
        let file_obj = env
            .call_method(context, jni_str!("getFilesDir"), jni_sig!("()Ljava/io/File;"), &[])?
            .l()?;
        let path = env
            .call_method(
                &file_obj,
                jni_str!("getAbsolutePath"),
                jni_sig!("()Ljava/lang/String;"),
                &[],
            )?
            .l()?;
        rust_string(env, path)
    });

    result.unwrap_or_else(|_| String::from("/data/data/dev.pah.eddy/files"))
}

/// Where yt-dlp, Python and ffmpeg are, and the environment they run in
/// (from YtdlRuntime.kt: YTDLnis's packages, unpacked).
#[derive(serde::Deserialize, Default)]
struct YtdlRuntime {
    python: Option<String>,
    ytdlp: Option<String>,
    ffmpeg: Option<String>,
    deno: Option<String>,
    quickjs: Option<String>,
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
    /// Installed helper apps' versions ("python", "ffmpeg", "deno").
    #[serde(default)]
    installed: std::collections::HashMap<String, String>,
    /// The device's main ABI, e.g. "arm64-v8a".
    #[serde(default)]
    abi: String,
}

/// None until Kotlin has set things up (the first run unpacks the packages).
static YTDL_RUNTIME: Mutex<Option<YtdlRuntime>> = Mutex::new(None);
static YTDL_READY: std::sync::Condvar = std::sync::Condvar::new();

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_pah_eddy_EddyNative_setRuntime<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    json: JString<'caller>,
) {
    let json = unowned_env
        .with_env(|env| -> jni::errors::Result<_> { json.try_to_string(env) })
        .resolve::<LogErrorAndDefault>();
    let runtime: YtdlRuntime = serde_json::from_str(&json).unwrap_or_default();
    log::info!(
        "yt-dlp runtime: python {:?}, ffmpeg {:?}, deno {:?}, quickjs {:?}",
        runtime.python, runtime.ffmpeg, runtime.deno, runtime.quickjs
    );
    *YTDL_RUNTIME.lock().unwrap_or_else(|e| e.into_inner()) = Some(runtime);
    YTDL_READY.notify_all();
    crate::components::runtime_changed();
}

/// The installed helper apps' versions and the device's ABI, if Kotlin has
/// looked yet (doesn't wait).
pub fn helper_apps() -> Option<(std::collections::HashMap<String, String>, String)> {
    with_runtime(std::time::Duration::ZERO, |r| Some((r.installed.clone(), r.abi.clone())))
}

/// Opens Android's installer for a downloaded helper APK.
pub fn install_apk(path: &str) {
    let _ = with_env(|env, activity| {
        let path = env.new_string(path)?;
        env.call_method(activity, jni_str!("installApk"), jni_sig!("(Ljava/lang/String;)V"), &[JValue::Object(&path)])?;
        Ok(())
    });
}

/// Runs `f` on the runtime, waiting (up to `wait`) for Kotlin to set it up.
fn with_runtime<T>(wait: std::time::Duration, f: impl FnOnce(&YtdlRuntime) -> Option<T>) -> Option<T> {
    let guard = YTDL_RUNTIME.lock().unwrap_or_else(|e| e.into_inner());
    let (guard, _) = YTDL_READY
        .wait_timeout_while(guard, wait, |runtime| runtime.is_none())
        .unwrap_or_else(|e| e.into_inner());
    guard.as_ref().and_then(f)
}

/// Long enough for the first unpack after installing.
const RUNTIME_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether yt-dlp can run (waits for the setup if it's still going).
/// Python, and yt-dlp: the package's (older packages carry it) or Eddy's own.
pub fn ytdlp_available() -> bool {
    let own = own_ytdlp().is_file();
    with_runtime(RUNTIME_WAIT, |r| Some(r.python.is_some() && (own || r.ytdlp.is_some()))).unwrap_or(false)
}

/// Like ytdlp_available, without waiting (for the UI thread).
pub fn ytdlp_ready() -> bool {
    let own = own_ytdlp().is_file();
    with_runtime(std::time::Duration::ZERO, |r| Some(r.python.is_some() && (own || r.ytdlp.is_some()))).unwrap_or(false)
}

/// yt-dlp, ready for its arguments: Python running the yt-dlp script, told
/// where ffmpeg and a JS runtime are.
pub fn ytdlp_command() -> Option<std::process::Command> {
    // Eddy's own, newer yt-dlp when there is one (the Python package's
    // ages; sites change): see components::update_ytdlp.
    let own = own_ytdlp();
    with_runtime(RUNTIME_WAIT, |r| {
        let mut command = std::process::Command::new(r.python.as_ref()?);
        if own.is_file() {
            command.arg(&own);
        } else {
            command.arg(r.ytdlp.as_ref()?);
        }
        command.envs(&r.env);
        // The community plugin, next to Eddy's yt-dlp (ytdlp_plugin.rs).
        if let Some(dir) = own.parent().filter(|d| crate::ytdlp_plugin::installed(d)) {
            command.arg("--plugin-dirs").arg(crate::ytdlp_plugin::plugins_dir(dir));
        }
        if let Some(ffmpeg) = &r.ffmpeg {
            command.args(["--ffmpeg-location", ffmpeg]);
        }
        // Deno first: yt-dlp's preferred JS runtime (for YouTube); QuickJS
        // comes with Python as a fallback.
        if let Some(deno) = &r.deno {
            command.args(["--js-runtimes", &format!("deno:{deno}")]);
        }
        if let Some(quickjs) = &r.quickjs {
            command.args(["--js-runtimes", &format!("quickjs:{quickjs}")]);
        }
        Some(command)
    })
}

/// Where Eddy keeps its own yt-dlp (one file, run by the Python package).
pub fn own_ytdlp() -> std::path::PathBuf {
    std::path::PathBuf::from(get_app_data_dir()).join("ytdl").join("yt-dlp")
}

/// Deno (the Instagram extractor's SnapSave fallback), if its helper is
/// installed.
pub fn deno_command() -> Option<std::process::Command> {
    with_runtime(RUNTIME_WAIT, |r| {
        let mut command = std::process::Command::new(r.deno.as_ref()?);
        command.envs(&r.env);
        Some(command)
    })
}

/// ffmpeg, with the libraries it needs.
pub fn ffmpeg_command() -> Option<std::process::Command> {
    with_runtime(RUNTIME_WAIT, |r| {
        let mut command = std::process::Command::new(r.ffmpeg.as_ref()?);
        command.envs(&r.env);
        Some(command)
    })
}
