//! The standalone `zoom_fetch` program; the installed pipeline runs the same code as
//! `TinyTranscript.app/Contents/MacOS/TinyTranscript zoom-fetch` (one app, one macOS permission).
fn main() {
    zoom_fetch::main_with(std::env::args().collect())
}
