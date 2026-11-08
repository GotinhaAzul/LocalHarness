//! Incremental Server-Sent Events framing.
//!
//! Handles frames split across TCP chunks, multiple frames per chunk, CRLF,
//! comments, multi-line `data:` fields, and a missing final blank line.

#[derive(Debug, PartialEq, Eq)]
pub enum SseFrame {
    Data(String),
    /// The stream carried `data: [DONE]`.
    Done,
}

#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: String,
    seen_data: bool,
    done: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every frame completed by this chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        if self.done {
            return Vec::new();
        }
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            let Some(pos) = self.buf.iter().position(|b| *b == b'\n') else {
                break;
            };
            let line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            self.process_line(line.trim_end_matches(['\r', '\n']), &mut frames);
            if self.done {
                self.buf.clear();
                break;
            }
        }
        frames
    }

    /// Flush at EOF: a trailing line without a newline is still processed.
    pub fn finish(&mut self) -> Vec<SseFrame> {
        if self.done {
            return Vec::new();
        }
        let mut frames = Vec::new();
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).to_string();
            self.buf.clear();
            self.process_line(line.trim_end_matches(['\r', '\n']), &mut frames);
        }
        if self.seen_data && !self.done {
            self.dispatch(&mut frames);
        }
        frames
    }

    fn process_line(&mut self, line: &str, frames: &mut Vec<SseFrame>) {
        if line.is_empty() {
            self.dispatch(frames);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.seen_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.seen_data = true;
            }
            "event" | "id" | "retry" => {}
            _ => {}
        }
    }

    fn dispatch(&mut self, frames: &mut Vec<SseFrame>) {
        if !self.seen_data {
            return;
        }
        let data = std::mem::take(&mut self.data);
        self.seen_data = false;
        if data.trim() == "[DONE]" {
            self.done = true;
            frames.push(SseFrame::Done);
        } else {
            frames.push(SseFrame::Data(data));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_frames_per_chunk() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"data: one\n\ndata: two\n\n");
        assert_eq!(
            frames,
            vec![SseFrame::Data("one".into()), SseFrame::Data("two".into())]
        );
    }

    #[test]
    fn frame_split_across_chunks() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"da").is_empty());
        assert!(parser.push(b"ta: {\"a\":").is_empty());
        let frames = parser.push(b"1}\n\n");
        assert_eq!(frames, vec![SseFrame::Data("{\"a\":1}".into())]);
    }

    #[test]
    fn crlf_and_comments() {
        let mut parser = SseParser::new();
        let frames = parser.push(b": keepalive\r\ndata: hi\r\n\r\n");
        assert_eq!(frames, vec![SseFrame::Data("hi".into())]);
    }

    #[test]
    fn multiline_data_is_joined_with_newline() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"data: one\ndata: two\n\n");
        assert_eq!(frames, vec![SseFrame::Data("one\ntwo".into())]);
    }

    #[test]
    fn done_stops_parsing() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"data: [DONE]\n\ndata: ignored\n\n");
        assert_eq!(frames, vec![SseFrame::Done]);
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut parser = SseParser::new();
        let text = "data: héllo\n\n".as_bytes();
        let mid = 8; // inside the two-byte é
        assert!(parser.push(&text[..mid]).is_empty());
        let frames = parser.push(&text[mid..]);
        assert_eq!(frames, vec![SseFrame::Data("héllo".into())]);
    }

    #[test]
    fn finish_flushes_trailing_data() {
        let mut parser = SseParser::new();
        parser.push(b"data: tail");
        assert_eq!(parser.finish(), vec![SseFrame::Data("tail".into())]);
    }
}
