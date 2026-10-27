#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
use std::process::Command;

fn main() {
    
    {
        // Add rpaths for libraries that webrtc-sys dlopens at runtime.
        // This is mostly required for hosts with non-standard SO installation
        // locations such as NixOS.
        let dlopened_libs = ["libva", "libva-drm", "egl"];

        let mut rpath_dirs = std::collections::BTreeSet::new();
        for lib in &dlopened_libs {
            if let Some(libdir) = pkg_config::get_variable(lib, "libdir").ok() {
                rpath_dirs.insert(libdir);
            } else {
                eprintln!("zed build.rs: {lib} not found in pkg-config's path");
            }
        }

        for dir in &rpath_dirs {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
        }
    }

    // Populate git sha environment variable if git is available
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    println!(
        "cargo:rustc-env=TARGET={}",
        std::env::var("TARGET").unwrap()
    );

    let git_sha = match std::env::var("ZED_COMMIT_SHA").ok() {
        Some(git_sha) => {
            // In deterministic build environments such as Nix, we inject the commit sha into the build script.
            Some(git_sha)
        }
        None => {
            if let Some(output) = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                && output.status.success()
            {
                let git_sha = String::from_utf8_lossy(&output.stdout);
                Some(git_sha.trim().to_string())
            } else {
                None
            }
        }
    };

    if let Some(git_sha) = git_sha {
        println!("cargo:rustc-env=ZED_COMMIT_SHA={git_sha}");

        if let Some(build_identifier) = option_env!("GITHUB_RUN_NUMBER") {
            println!("cargo:rustc-env=ZED_BUILD_ID={build_identifier}");
        }

        if let Ok(build_profile) = std::env::var("PROFILE")
            && build_profile == "release"
        {
            // This is currently the best way to make `cargo build ...`'s build script
            // to print something to stdout without extra verbosity.
            println!("cargo::warning=Info: using '{git_sha}' hash for ZED_COMMIT_SHA env var");
        }
    }
    
    prepare_app_icon_x11();
}


fn icon_path() -> std::path::PathBuf {
    use std::str::FromStr;

    let release_channel = option_env!("RELEASE_CHANNEL").unwrap_or("dev");
    let channel = match release_channel {
        "stable" => "",
        "preview" => "-preview",
        "nightly" => "-nightly",
        "dev" => "-dev",
        _ => "-dev",
    };

    let icon = format!("resources/app-icon{}.png", channel);

    std::path::PathBuf::from_str(&icon).unwrap()
}


fn prepare_app_icon_x11() {
    use image::{ImageReader, imageops};
    use std::env;
    use std::path::Path;

    let out_dir = env::var("OUT_DIR").unwrap();

    let resized_image = ImageReader::open(icon_path())
        .unwrap()
        .decode()
        .unwrap()
        .resize(256, 256, imageops::FilterType::Lanczos3);

    // name should match include_bytes! call in src/zed.rs
    let icon_out_path = Path::new(&out_dir).join("app_icon.png");
    resized_image.save(&icon_out_path).expect("saving app icon");

    println!("cargo:rerun-if-env-changed=RELEASE_CHANNEL");
    println!("cargo:rerun-if-changed={}", icon_path().to_string_lossy());
}
