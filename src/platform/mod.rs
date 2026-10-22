use std::{fs, io, path::PathBuf};

use serde::{Deserialize, Serialize};

/// A vibration, if haptic feedback is on (Android). Each feels different;
/// the numbers match Haptics.kt.
#[derive(Clone, Copy, Debug)]
pub enum Haptic {
    /// A link was checked (tiny).
    LinkChecked = 0,
    Downloaded = 1,
    Failed = 2,
    Click = 3,
}

#[derive(Debug)]
pub enum PrefError {
    Io(std::io::Error),
    Serde(serde_json::Error),
    Other(String),
    #[cfg(target_os = "android")]
    Jni(jni::errors::Error),
}

impl std::fmt::Display for PrefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrefError::Io(e) => write!(f, "preference I/O error: {e}"),
            PrefError::Serde(e) => write!(f, "preference (de)serialization error: {e}"),
            PrefError::Other(e) => write!(f, "Error: {e}"),
            #[cfg(target_os = "android")]
            PrefError::Jni(e) => write!(f, "JNI error: {e}"),
        }
    }
}

impl std::error::Error for PrefError {}

impl From<std::io::Error> for PrefError {
    fn from(e: std::io::Error) -> Self {
        PrefError::Io(e)
    }
}

impl From<serde_json::Error> for PrefError {
    fn from(e: serde_json::Error) -> Self {
        PrefError::Serde(e)
    }
}

impl From<String> for PrefError {
    fn from(e: String) -> Self {
        PrefError::Other(e)
    }
}

#[cfg(target_os = "android")]
impl From<jni::errors::Error> for PrefError {
    fn from(e: jni::errors::Error) -> Self {
        PrefError::Jni(e)
    }
}

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "android")]
pub use android::*;

#[cfg(target_os = "ios")]
pub mod ios;
#[cfg(target_os = "ios")]
pub use ios::*;

// Fallback for desktop (Windows, macOS, Linux)
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod desktop;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use desktop::*;

pub fn get_internal_download_dir() -> io::Result<PathBuf> {
    let internal_base_dir = get_app_data_dir();
    let internal_download_dir = PathBuf::from(internal_base_dir).join("downloads");
    if !internal_download_dir.exists() {
        fs::create_dir_all(&internal_download_dir)?;
    }
    Ok(internal_download_dir)
}
