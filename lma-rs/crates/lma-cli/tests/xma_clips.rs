//! Regression (#199): every XMA clip in the game's shared wave banks, and the English speech
//! banks, decodes through the runtime's XMA hardware model exactly as `ffmpeg` decodes the same
//! bytes (`lma-cli xma-check`). Needs the game (never in the repo) and ffmpeg: set
//! `LMA_GAME_DIR=<dir with default.xex>` and build lma-cli with `--features xma-ffmpeg`;
//! otherwise it skips, as CI does.

#[cfg(feature = "xma-ffmpeg")]
#[test]
fn every_clip_decodes_like_ffmpeg() {
    let Some(game) = std::env::var_os("LMA_GAME_DIR") else {
        eprintln!("skipped: LMA_GAME_DIR not set");
        return;
    };
    for sub in ["Common", "Data_01"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_lma-cli"))
            .arg("xma-check")
            .arg(std::path::Path::new(&game).join(sub))
            .output()
            .expect("run lma-cli");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{sub}: clips decode differently:\n{text}");
        assert!(text.contains(" 0 mismatched"), "{sub}: {text}");
    }
}
