use std::path::Path;
use std::process::Command;

fn main() {
    // The UI itself is compiled by its own crate (ui/build.rs).
    let output = Command::new("cargo")
        .args(["pkgid", "slint"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let version = output.split('@').last().unwrap_or("Unknown");

    println!("cargo:rustc-env=DEPENDENCY_SLINT_VERSION={}", version);
    // Slint's version is in the lock file (otherwise Cargo would run this
    // again for any change in the package).
    println!("cargo:rerun-if-changed=Cargo.lock");

    // iOS: FFmpeg's command-line tools, linked into the app, when CI has
    // fetched them (tfourj/SwiftFFmpeg-iOS's release, named by EDDY_FFMPEG_IOS).
    println!("cargo::rustc-check-cfg=cfg(eddy_ffmpeg)");
    println!("cargo:rerun-if-env-changed=EDDY_FFMPEG_IOS");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        if let Ok(root) = std::env::var("EDDY_FFMPEG_IOS") {
            link_ffmpeg(Path::new(&root));
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = winres::WindowsResource::new()
            .set_icon("ui/icons/logo.ico")
            .compile();
    }
}

/// Links SwiftFFmpeg-iOS's static FFmpeg (ffmpeg and ffprobe as functions)
/// and compiles its C wrapper (run with captured output, cancel), which
/// the library calls back into.
fn link_ffmpeg(root: &Path) {
    let target = std::env::var("TARGET").unwrap_or_default();
    let simulator = target.ends_with("-sim") || target.starts_with("x86_64");
    let slice = root.join("FFmpeg.xcframework").join(if simulator { "ios-arm64-simulator" } else { "ios-arm64" });
    let wrapper = root.join("Sources/CFFmpegCLI");
    cc::Build::new()
        .file(wrapper.join("ffmpeg_wrapper.c"))
        .include(wrapper.join("include"))
        .include(slice.join("Headers"))
        .warnings(false)
        .compile("eddy_ffmpeg_wrapper");
    println!("cargo:rustc-link-search=native={}", slice.display());
    println!("cargo:rustc-link-lib=static=FFmpeg");
    // What SwiftFFmpeg-iOS's Package.swift links.
    for framework in ["AudioToolbox", "AVFoundation", "CoreMedia", "VideoToolbox"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    for library in ["z", "bz2", "iconv", "xml2"] {
        println!("cargo:rustc-link-lib=dylib={library}");
    }
    println!("cargo:rustc-cfg=eddy_ffmpeg");
    println!("cargo:rerun-if-changed={}", wrapper.join("ffmpeg_wrapper.c").display());
}
