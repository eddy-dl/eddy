# Only reached through JNI, so the shrinker can't see it's used.
-keep, includedescriptorclasses class org.rustls.platformverifier.** { *; }
