//! Bakes the build's git commit into the binary (`runtime::BUILD`), so recordings can say which
//! build made them: a recording replays exactly only on that build. `LMA_BUILD_ID` overrides it.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=LMA_BUILD_ID");
    // a new commit or checkout, or staged changes (the dirty flag)
    for p in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r])) {
        println!("cargo:rerun-if-changed={r}");
    }
    let id = std::env::var("LMA_BUILD_ID").ok().or_else(|| git(&["describe", "--always", "--dirty", "--abbrev=7", "--exclude=*"]));
    println!("cargo:rustc-env=LMA_BUILD={}", id.unwrap_or_else(|| "unknown".into()));

    // XMA decoding through FFmpeg (#199): a C shim against libavcodec's own headers
    #[cfg(feature = "xma-ffmpeg")]
    {
        println!("cargo:rerun-if-changed=src/xma_ffmpeg.c");
        println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
        let dir = std::env::var("FFMPEG_DIR").unwrap_or_else(|_| "/opt/homebrew".into());
        cc::Build::new().file("src/xma_ffmpeg.c").include(format!("{dir}/include")).compile("lma_xma_ffmpeg");
        println!("cargo:rustc-link-search=native={dir}/lib");
        // FFMPEG_STATIC=1: the decoder-only static libraries of tools/ffmpeg_min.sh (Route 1 for
        // the release zip: nothing beside the exe), with what libavutil needs from the system
        println!("cargo:rerun-if-env-changed=FFMPEG_STATIC");
        let kind = if std::env::var("FFMPEG_STATIC").as_deref() == Ok("1") { "static" } else { "dylib" };
        println!("cargo:rustc-link-lib={kind}=avcodec");
        println!("cargo:rustc-link-lib={kind}=avutil");
        if kind == "static" && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            for lib in ["bcrypt", "user32", "ole32"] {
                println!("cargo:rustc-link-lib=dylib={lib}");
            }
        }
    }
}
