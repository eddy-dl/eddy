//! The iOS app's executable (Xcode bundles it; see platforms/ios). Other
//! targets build it as an empty program.

fn main() {
    #[cfg(target_os = "ios")]
    eddy::ios_main();
}
