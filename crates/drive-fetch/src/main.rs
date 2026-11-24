//! The standalone `drive_fetch` program; the installed pipeline runs the same code as
//! `TinyTranscript.app/Contents/MacOS/TinyTranscript drive-fetch` (one app, one macOS permission).
fn main() {
    drive_fetch::main_with(std::env::args().collect())
}
