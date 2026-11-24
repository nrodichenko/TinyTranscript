//! The standalone `match_speakers` program; the installed pipeline runs the same code as
//! `TinyTranscript.app/Contents/MacOS/TinyTranscript match-speakers` (one app, one macOS permission).
fn main() {
    match_speakers::main_with(std::env::args().collect())
}
