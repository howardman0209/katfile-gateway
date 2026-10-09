//! Remote name handling verified against the live KatFile API (see docs/API_COMPATIBILITY.md):
//!
//! * names are stored as raw bytes and returned by the JSON API as if they were
//!   Latin-1, so UTF-8 names come back as mojibake (`相片` -> `ç\u{9b}¸ç\u{89}\u{87}`);
//! * `"` is rewritten to `&quote;`;
//! * names longer than 128 bytes are truncated;
//! * duplicate and case-variant folder names are both allowed.
//!
//! The gateway therefore sanitizes every name it sends so that the stored
//! value is predictable, and demangles listed names before comparing them.

/// Longest name (in UTF-8 bytes) KatFile stores without truncation.
pub const MAX_REMOTE_NAME_BYTES: usize = 128;

/// Longest extension preserved when a file name must be shortened.
const MAX_PRESERVED_EXT_BYTES: usize = 16;

/// Recover the original UTF-8 name from KatFile's Latin-1 style JSON rendering.
///
/// ASCII names are returned unchanged; anything that does not round-trip as
/// Latin-1 encoded UTF-8 is also returned unchanged.
pub fn demangle(raw: &str) -> String {
    if raw.is_ascii() {
        return raw.to_owned();
    }
    let mut bytes = Vec::with_capacity(raw.len());
    for c in raw.chars() {
        let cp = c as u32;
        if cp > 0xFF {
            return raw.to_owned();
        }
        bytes.push(cp as u8);
    }
    String::from_utf8(bytes).unwrap_or_else(|_| raw.to_owned())
}

/// Make a single path component safe to send to KatFile as a folder name.
pub fn sanitize_folder_name(component: &str) -> String {
    let cleaned = replace_unsafe_chars(component);
    let truncated = truncate_utf8(&cleaned, MAX_REMOTE_NAME_BYTES);
    if truncated.is_empty() { "_".to_owned() } else { truncated.to_owned() }
}

/// Make a file name safe to send to KatFile, keeping a short extension when shortening.
pub fn sanitize_file_name(file_name: &str) -> String {
    let cleaned = replace_unsafe_chars(file_name);
    if cleaned.len() <= MAX_REMOTE_NAME_BYTES {
        return if cleaned.is_empty() { "_".to_owned() } else { cleaned };
    }
    match cleaned.rfind('.') {
        Some(dot) if dot > 0 && cleaned.len() - dot <= MAX_PRESERVED_EXT_BYTES => {
            let ext = &cleaned[dot..];
            let stem = truncate_utf8(&cleaned[..dot], MAX_REMOTE_NAME_BYTES - ext.len());
            format!("{stem}{ext}")
        }
        _ => truncate_utf8(&cleaned, MAX_REMOTE_NAME_BYTES).to_owned(),
    }
}

/// Characters KatFile rewrites or that are unsafe in a multipart `filename="..."` header.
fn replace_unsafe_chars(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '"' | '\'' | '<' | '>' | '&' | '\\' | '/' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// Truncate to at most `max` bytes without splitting a UTF-8 character.
fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demangles_observed_katfile_mojibake() {
        // Exact value returned by the live API for "相片 ü 2026".
        let raw = "ç\u{9b}¸ç\u{89}\u{87} Ã¼ 2026";
        assert_eq!(demangle(raw), "相片 ü 2026");
        assert_eq!(demangle("plain-ascii.txt"), "plain-ascii.txt");
    }

    #[test]
    fn demangle_leaves_real_unicode_untouched() {
        // Characters above U+00FF cannot be Latin-1 mojibake.
        assert_eq!(demangle("相片"), "相片");
    }

    #[test]
    fn sanitizes_characters_katfile_rewrites() {
        assert_eq!(sanitize_folder_name("quote\"amp&x"), "quote_amp_x");
        assert_eq!(sanitize_folder_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_folder_name("tab\there"), "tab_here");
        assert_eq!(sanitize_folder_name(""), "_");
        assert_eq!(sanitize_folder_name("相片 2026"), "相片 2026");
    }

    #[test]
    fn truncates_to_128_bytes_on_char_boundary() {
        let name = "相".repeat(60); // 180 bytes
        let s = sanitize_folder_name(&name);
        assert!(s.len() <= MAX_REMOTE_NAME_BYTES);
        assert_eq!(s.len() % 3, 0);
    }

    #[test]
    fn file_names_keep_extension_when_shortened() {
        let name = format!("{}.mp4", "v".repeat(300));
        let s = sanitize_file_name(&name);
        assert_eq!(s.len(), MAX_REMOTE_NAME_BYTES);
        assert!(s.ends_with(".mp4"));
    }
}
