//! File names for a batch of frames: prefix plus a running, zero-padded number.

/// `frame-07` for prefix `frame` and number 7; the number alone for an empty prefix.
/// Path separators and other characters unsuitable for a file name are replaced by `_`.
pub fn frame_stem(prefix: &str, number: u32) -> String {
    let clean: String = prefix
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        format!("{number:02}")
    } else {
        format!("{clean}-{number:02}")
    }
}

/// "frame-01 … frame-06" for a preview of what a batch will write.
pub fn range_label(prefix: &str, start: u32, count: usize) -> String {
    match count {
        0 => String::new(),
        1 => frame_stem(prefix, start),
        n => format!(
            "{} … {}",
            frame_stem(prefix, start),
            frame_stem(prefix, start + n as u32 - 1)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_to_two_digits_and_grows() {
        assert_eq!(frame_stem("frame", 1), "frame-01");
        assert_eq!(frame_stem("frame", 99), "frame-99");
        assert_eq!(frame_stem("frame", 100), "frame-100");
    }

    #[test]
    fn empty_prefix_gives_just_the_number() {
        assert_eq!(frame_stem("  ", 3), "03");
    }

    #[test]
    fn prefix_cannot_escape_the_directory() {
        assert_eq!(frame_stem("../x/y", 1), ".._x_y-01");
        assert!(!frame_stem("a/b\\c", 2).contains(['/', '\\']));
    }

    #[test]
    fn range_label_shows_first_and_last() {
        assert_eq!(range_label("f", 5, 2), "f-05 … f-06");
        assert_eq!(range_label("f", 5, 1), "f-05");
        assert_eq!(range_label("f", 5, 0), "");
    }
}
