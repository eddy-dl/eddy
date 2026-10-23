# Only reached through JNI, so the shrinker can't see it's used.
-keep, includedescriptorclasses class org.rustls.platformverifier.** { *; }

# The activities' methods Rust calls through JNI (platform/android.rs):
# kept whole, as one with no Kotlin caller is otherwise stripped (openUrl
# was, breaking Settings → About's links).
-keepclassmembers class dev.pah.eddy.MainActivity { public *; }
-keepclassmembers class dev.pah.eddy.SharePopupActivity { public *; }
