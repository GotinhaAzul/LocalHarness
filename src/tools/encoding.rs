//! Text encodings the file tools understand.
//!
//! `read` and `grep` decode a BOM-labelled UTF-16 file, and a file that is
//! valid UTF-8 is read as UTF-8. Anything else without NUL bytes is decoded as
//! Latin-1 and labelled; only real binary is refused. `write` re-encodes an
//! existing file in its own encoding when the new text fits, so a Latin-1
//! properties file or a UTF-16 string table never silently becomes UTF-8.

/// How a byte sequence is interpreted as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    #[default]
    Utf8,
    /// UTF-8 with a leading EF BB BF.
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    /// One byte per code point, ISO-8859-1.
    Latin1,
}

impl Encoding {
    /// The name shown to the model and recorded in session entries.
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf8Bom => "utf-8 (BOM)",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
            Encoding::Latin1 => "latin-1",
        }
    }

    /// Whether the encoding is plain or BOM-marked UTF-8.
    pub fn is_utf8(self) -> bool {
        matches!(self, Encoding::Utf8 | Encoding::Utf8Bom)
    }

    /// The BOM this encoding writes, if any.
    pub fn bom(self) -> &'static [u8] {
        match self {
            Encoding::Utf8Bom => b"\xef\xbb\xbf",
            Encoding::Utf16Le => b"\xff\xfe",
            Encoding::Utf16Be => b"\xfe\xff",
            Encoding::Utf8 | Encoding::Latin1 => b"",
        }
    }

    /// Parse the label written by [`Encoding::label`]. Unknown labels are
    /// UTF-8, the format every other tool writes.
    pub fn from_label(label: &str) -> Self {
        match label.trim().to_ascii_lowercase().as_str() {
            "utf-8 (bom)" | "utf-8 bom" => Encoding::Utf8Bom,
            "utf-16le" | "utf-16-le" => Encoding::Utf16Le,
            "utf-16be" | "utf-16-be" => Encoding::Utf16Be,
            "latin-1" | "latin1" | "iso-8859-1" => Encoding::Latin1,
            _ => Encoding::Utf8,
        }
    }

    /// One line for a tool result when the encoding is not plain UTF-8.
    pub fn note(self) -> Option<String> {
        match self {
            Encoding::Utf8 => None,
            Encoding::Latin1 => Some(
                "[encoding] latin-1 assumed (not valid UTF-8); a write keeps this encoding, and characters outside latin-1 are refused"
                    .into(),
            ),
            other => Some(format!(
                "[encoding] {} decoded; a write keeps this encoding",
                other.label()
            )),
        }
    }
}

/// What [`decode`] found in a byte sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    Text {
        text: String,
        encoding: Encoding,
    },
    /// NUL bytes without a UTF-16 shape or BOM: not text.
    Binary,
}

/// Decode a file's bytes the way `read` and `grep` should.
pub fn decode(bytes: &[u8]) -> Decoded {
    if bytes.is_empty() {
        return Decoded::Text {
            text: String::new(),
            encoding: Encoding::Utf8,
        };
    }
    if let Some(rest) = bytes.strip_prefix(b"\xef\xbb\xbf")
        && let Ok(text) = std::str::from_utf8(rest)
    {
        return Decoded::Text {
            text: text.to_string(),
            encoding: Encoding::Utf8Bom,
        };
    }
    if let Some(rest) = bytes.strip_prefix(b"\xff\xfe") {
        return decode_utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(b"\xfe\xff") {
        return decode_utf16(rest, false);
    }
    // NUL bytes settle the question before UTF-8: NUL is valid UTF-8, so
    // `a\0p\0p` would otherwise be read as text with embedded control
    // characters instead of UTF-16.
    if bytes[..bytes.len().min(8192)].contains(&0) {
        if let Some(little_endian) = utf16_shape(bytes) {
            return decode_utf16(bytes, little_endian);
        }
        return Decoded::Binary;
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Decoded::Text {
            text: text.to_string(),
            encoding: Encoding::Utf8,
        };
    }
    Decoded::Text {
        text: bytes.iter().map(|byte| char::from(*byte)).collect(),
        encoding: Encoding::Latin1,
    }
}

/// Whether `bytes` look like BOM-less UTF-16 and in which byte order. ASCII
/// text in UTF-16LE is `x\0y\0`; the reverse is big-endian.
fn utf16_shape(bytes: &[u8]) -> Option<bool> {
    let prefix = &bytes[..bytes.len().min(4096)];
    if prefix.len() < 4 {
        return None;
    }
    let mut zero_even = 0usize;
    let mut zero_odd = 0usize;
    for (index, byte) in prefix.iter().enumerate() {
        if *byte == 0 {
            if index % 2 == 0 {
                zero_even += 1;
            } else {
                zero_odd += 1;
            }
        }
    }
    let pairs = prefix.len() / 2;
    if zero_odd * 4 >= pairs * 3 && zero_even * 8 <= pairs {
        return Some(true);
    }
    if zero_even * 4 >= pairs * 3 && zero_odd * 8 <= pairs {
        return Some(false);
    }
    None
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Decoded {
    let encoding = if little_endian {
        Encoding::Utf16Le
    } else {
        Encoding::Utf16Be
    };
    let units: Vec<u16> = bytes
        .chunks(2)
        .filter(|pair| pair.len() == 2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    let text = String::from_utf16(&units).unwrap_or_else(|_| {
        // Unpaired surrogates become U+FFFD; the rest of the file is still
        // readable instead of being called binary.
        char::decode_utf16(units.iter().copied())
            .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    });
    Decoded::Text { text, encoding }
}

/// Encode `text` in `encoding`. `None` when a character has no representation
/// in the target (only Latin-1 can fail).
pub fn encode(text: &str, encoding: Encoding) -> Option<Vec<u8>> {
    match encoding {
        Encoding::Utf8 => Some(text.as_bytes().to_vec()),
        Encoding::Utf8Bom => {
            let mut out = encoding.bom().to_vec();
            out.extend_from_slice(text.as_bytes());
            Some(out)
        }
        Encoding::Utf16Le => Some(encode_utf16(text, true)),
        Encoding::Utf16Be => Some(encode_utf16(text, false)),
        Encoding::Latin1 => {
            let mut out = Vec::with_capacity(text.len());
            for character in text.chars() {
                let code = u32::from(character);
                if code > 0xFF {
                    return None;
                }
                out.push(code as u8);
            }
            Some(out)
        }
    }
}

fn encode_utf16(text: &str, little_endian: bool) -> Vec<u8> {
    let mut out = if little_endian {
        vec![0xff, 0xfe]
    } else {
        vec![0xfe, 0xff]
    };
    for unit in text.encode_utf16() {
        let pair = if little_endian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        out.extend_from_slice(&pair);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_and_bom_files_decode_as_utf8() {
        assert_eq!(
            decode("héllo".as_bytes()),
            Decoded::Text {
                text: "héllo".into(),
                encoding: Encoding::Utf8
            }
        );
        assert_eq!(
            decode("\u{feff}héllo".as_bytes()),
            Decoded::Text {
                text: "héllo".into(),
                encoding: Encoding::Utf8Bom
            }
        );
    }

    #[test]
    fn utf16_files_with_and_without_a_bom_decode() {
        let text = "app.title=Nimbus\r\n";
        let mut le = vec![0xff, 0xfe];
        le.extend(text.encode_utf16().flat_map(|unit| unit.to_le_bytes()));
        assert_eq!(
            decode(&le),
            Decoded::Text {
                text: text.into(),
                encoding: Encoding::Utf16Le
            }
        );
        let bare: Vec<u8> = text
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        assert_eq!(
            decode(&bare),
            Decoded::Text {
                text: text.into(),
                encoding: Encoding::Utf16Le
            }
        );
        let mut be = vec![0xfe, 0xff];
        be.extend(text.encode_utf16().flat_map(|unit| unit.to_be_bytes()));
        assert_eq!(
            decode(&be),
            Decoded::Text {
                text: text.into(),
                encoding: Encoding::Utf16Be
            }
        );
    }

    #[test]
    fn latin1_decodes_without_replacement_characters() {
        let bytes = b"Sobre o Nimbus \xe9 vers\xe3o 2\n";
        assert_eq!(
            decode(bytes),
            Decoded::Text {
                text: "Sobre o Nimbus é versão 2\n".into(),
                encoding: Encoding::Latin1
            }
        );
    }

    #[test]
    fn binary_is_still_binary() {
        assert_eq!(decode(b"\x00\x01\x02PNG"), Decoded::Binary);
        assert_eq!(decode(&[0u8; 16]), Decoded::Binary);
    }

    #[test]
    fn encoding_round_trips_and_refuses_what_does_not_fit() {
        for encoding in [
            Encoding::Utf8,
            Encoding::Utf8Bom,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Latin1,
        ] {
            let bytes = encode("aé\n", encoding).unwrap();
            assert_eq!(
                decode(&bytes),
                Decoded::Text {
                    text: "aé\n".into(),
                    encoding
                },
                "{encoding:?}"
            );
        }
        assert!(encode("日本語", Encoding::Latin1).is_none());
        assert_eq!(
            Encoding::from_label("utf-16le"),
            Encoding::Utf16Le,
            "labels round-trip"
        );
    }
}
