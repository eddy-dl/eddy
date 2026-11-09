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
    // And QuickJS-ng, the JavaScript fallback for iOS's own engine, from
    // its source (EDDY_QUICKJS_IOS).
    println!("cargo::rustc-check-cfg=cfg(eddy_ffmpeg)");
    println!("cargo::rustc-check-cfg=cfg(eddy_quickjs)");
    println!("cargo:rerun-if-env-changed=EDDY_FFMPEG_IOS");
    println!("cargo:rerun-if-env-changed=EDDY_QUICKJS_IOS");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        if let Ok(root) = std::env::var("EDDY_FFMPEG_IOS") {
            link_ffmpeg(Path::new(&root));
        }
        if let Ok(source) = std::env::var("EDDY_QUICKJS_IOS") {
            build_quickjs(Path::new(&source));
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
    // What SwiftFFmpeg-iOS's Package.swift links, and CoreVideo, which Xcode
    // adds by itself.
    for framework in ["AudioToolbox", "AVFoundation", "CoreMedia", "CoreVideo", "VideoToolbox"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    for library in ["z", "bz2", "iconv", "xml2"] {
        println!("cargo:rustc-link-lib=dylib={library}");
    }
    // Clang's runtime (`@available` checks in FFmpeg), which Rust's link
    // leaves out (-nodefaultlibs).
    let runtime = if simulator { "libclang_rt.iossim.a" } else { "libclang_rt.ios.a" };
    let compiler = cc::Build::new().get_compiler();
    if let Ok(output) = Command::new(compiler.path()).arg(format!("-print-file-name={runtime}")).output() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if Path::new(&path).is_file() {
            println!("cargo:rustc-link-arg={path}");
        } else {
            println!("cargo:warning=clang's {runtime} wasn't found ({path})");
        }
    }
    println!("cargo:rustc-cfg=eddy_ffmpeg");
    println!("cargo:rerun-if-changed={}", wrapper.join("ffmpeg_wrapper.c").display());
}

/// Compiles QuickJS-ng's library and Eddy's runner for it
/// (platforms/ios/quickjs_eval.c).
fn build_quickjs(source: &Path) {
    let runner = Path::new("platforms/ios/quickjs_eval.c");
    let mut build = cc::Build::new();
    for file in ["quickjs.c", "libregexp.c", "libunicode.c", "dtoa.c"] {
        build.file(source.join(file));
    }
    build.file(runner).include(source).define("_GNU_SOURCE", None).warnings(false).compile("eddy_quickjs");
    println!("cargo:rustc-cfg=eddy_quickjs");
    println!("cargo:rerun-if-changed={}", runner.display());
}
