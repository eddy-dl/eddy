import java.util.regex.Pattern

val cargoTomlFile = file("../../../Cargo.toml")
var cargoVersionName = "1.0.0"

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

if (cargoTomlFile.exists()) {
    val pattern = Pattern.compile("""version\s*=\s*"([^"]+)"""")
    val matcher = pattern.matcher(cargoTomlFile.readText())
    if (matcher.find()) {
        cargoVersionName = matcher.group(1)
    }
}

// The verifier's Java half must match the rustls-platform-verifier-android
// crate in Cargo.lock exactly, or certificate checks crash at runtime.
val cargoLockFile = file("../../../Cargo.lock")
val rustlsVerifierVersion = cargoLockFile.readLines().let { lines ->
    val nameIdx = lines.indexOfFirst { it.trim() == "name = \"rustls-platform-verifier-android\"" }
    lines.drop(nameIdx + 1)
        .firstOrNull { it.trimStart().startsWith("version = ") }
        ?.substringAfter('"')
        ?.substringBefore('"')
        .takeIf { nameIdx >= 0 }
} ?: error("rustls-platform-verifier-android not found in Cargo.lock")

val versionParts = cargoVersionName.split(".")
val cargoVersionCode = if (versionParts.size >= 3) {
    (versionParts[0].toInt() * 10000) + (versionParts[1].toInt() * 100) + versionParts[2].toInt()
} else {
    1
}

android {
    namespace = "dev.pah.eddy"
    compileSdk = 35

        defaultConfig {
        applicationId = "dev.pah.eddy"
        minSdk = 26
        targetSdk = 35
        versionCode = cargoVersionCode
        versionName = cargoVersionName

        ndk {
            abiFilters += listOf("arm64-v8a", "armeabi-v7a")
        }
    }

    splits {
        abi {
            isEnable = true
            reset()
            include("arm64-v8a", "armeabi-v7a")
            isUniversalApk = true
        }
    }


    signingConfigs {
        create("release") {
            val keystorePath = System.getenv("KEYSTORE_FILE")
            if (!keystorePath.isNullOrEmpty()) {
                storeFile = file(keystorePath)
                storePassword = System.getenv("KEYSTORE_PASSWORD")
                keyAlias = System.getenv("KEY_ALIAS")
                keyPassword = System.getenv("KEY_PASSWORD")
            }
        }
        create("releasetest") {
            storeFile = file("../../../debug.keystore")
            storePassword = "android"
            keyAlias = "androiddebugkey"
            keyPassword = "android"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
            signingConfig = signingConfigs.getByName("release")
        }
        create("releasetest") {
            initWith(getByName("release"))
            signingConfig = signingConfigs.getByName("releasetest")
        }
        debug {
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }

    kotlinOptions {
        jvmTarget = "1.8"
    }

    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }

    sourceSets {
        getByName("main") {
            jniLibs.srcDirs("src/main/jniLibs")
        }
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.12.0")
    implementation("androidx.appcompat:appcompat:1.6.1")
    implementation("org.rustls:rustls-platform-verifier:$rustlsVerifierVersion")
    // Unpacking the YTDLnis helper packages (yt-dlp) with their symlinks.
    // See YtdlRuntime.kt.
    implementation("org.apache.commons:commons-compress:1.27.1")
}

tasks.register<Copy>("copyRustLibs") {
    val rustTargetDir = file("../../../target")
    val jniLibsDir = file("src/main/jniLibs")

    from("$rustTargetDir/aarch64-linux-android/release") {
        include("libeddy.so")
        into("arm64-v8a")
    }

    from("$rustTargetDir/armv7-linux-androideabi/release") {
        include("libeddy.so")
        into("armeabi-v7a")
    }

    into(jniLibsDir)
}

tasks.named("preBuild") {
    dependsOn("copyRustLibs")
}
