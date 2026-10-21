plugins {
    // AGP 8.7 is the minimum version that automatically zip-aligns
    // native libraries to 16 KB page boundaries inside the APK. Earlier
    // versions (including 8.2.0) emit APKs that fail to install on
    // Android 16 / Pixel 10 Pro with errors like:
    //   libexample.so: Uncompressed library not aligned
    // The .so files themselves are linked with 16 KB segment alignment
    // by NDK r28+, but AGP must do the matching APK-side alignment.
    // AGP 8.7 requires Gradle 8.9+; the wrapper is bumped to match.
    id("com.android.application") version "8.7.3" apply false
    id("org.jetbrains.kotlin.android") version "1.9.25" apply false
}

tasks.register<Exec>("buildRust") {
    description = "Build Rust library for Android"
    group = "rust"
    workingDir = file("../..")
    commandLine("cargo", "ndk", "-t", "arm64-v8a", "-o", "platforms/android/app/src/main/jniLibs", "build", "--release")
}
