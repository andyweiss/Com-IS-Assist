//! Stamps the build with a version string for the GUI's help page.
//!
//! A build script rather than an `option_env!` in the source so the rebuild dependency is
//! explicit: `cargo:rerun-if-env-changed` states it outright, where `option_env!` leaves Cargo to
//! infer it from rustc's dep-info. Both work, but only one of them says so.
//!
//! It also puts the "what does an unstamped build call itself?" decision in one place, instead of
//! spreading a fallback across every reader of the variable.

fn main() {
    println!("cargo:rerun-if-env-changed=COMIS_RELEASE_VERSION");

    // CI sets this from the GitHub release tag. A local build has no tag to take, and reporting the
    // crate version there would claim a release that was never cut - the wrong answer when someone
    // is telling you which build is misbehaving. A whitespace-only value counts as unset, so a CI
    // variable that exists but was never filled in does not produce a blank version.
    let version = std::env::var("COMIS_RELEASE_VERSION")
        .ok()
        .filter(|tag| !tag.trim().is_empty())
        .unwrap_or_else(|| {
            let crate_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
            format!("dev build ({crate_version})")
        });

    println!("cargo:rustc-env=COMIS_DISPLAY_VERSION={version}");
}
