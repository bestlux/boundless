use anyhow::Result;
use core_clipboard::ClipboardPayload;

pub struct WindowsClipboardBackend;

impl WindowsClipboardBackend {
    pub fn sequence_number(&mut self) -> Option<u64> {
        clipboard_win::seq_num().map(|num| num.get() as u64)
    }

    pub fn read_payload(&mut self) -> Result<Option<ClipboardPayload>> {
        use clipboard_win::{Format, formats};

        if formats::Bitmap.is_format_avail() {
            let image_bmp: Vec<u8> = clipboard_win::get_clipboard(formats::Bitmap)
                .map_err(|error| anyhow::anyhow!("clipboard image read failed: {error}"))?;
            return Ok(Some(ClipboardPayload::Image(image_bmp)));
        }

        Ok(clipboard_win::get_clipboard_string()
            .ok()
            .map(ClipboardPayload::Text))
    }

    pub fn write_payload(&mut self, payload: &ClipboardPayload) -> Result<()> {
        use clipboard_win::formats;

        match payload {
            ClipboardPayload::Text(text) => clipboard_win::set_clipboard_string(text)
                .map_err(|error| anyhow::anyhow!("clipboard text write failed: {error}")),
            ClipboardPayload::Image(image_bmp) => {
                clipboard_win::set_clipboard(formats::Bitmap, image_bmp.as_slice())
                    .map_err(|error| anyhow::anyhow!("clipboard image write failed: {error}"))
            }
        }
    }
}
