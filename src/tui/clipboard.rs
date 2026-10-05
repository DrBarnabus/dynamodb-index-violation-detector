//! System clipboard via the OSC 52 terminal escape, which the terminal itself
//! applies, so a copy works over SSH and needs no platform clipboard tool.
//! Terminals without OSC 52 support (macOS Terminal.app) silently ignore it.

use std::io::{self, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

/// Ask the terminal to place `text` on the system clipboard.
pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout();
    stdout.write_all(osc52(text).as_bytes())?;
    stdout.flush()
}

fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", BASE64.encode(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_wraps_base64_text_for_the_clipboard_selection() {
        assert_eq!(osc52("id=u-1"), "\x1b]52;c;aWQ9dS0x\x07");
    }
}
