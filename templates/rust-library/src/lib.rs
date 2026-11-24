/// Return the first word, ignoring leading whitespace.
///
/// ```
/// use devkit_library::first_word;
///
/// assert_eq!(first_word("  hello world"), Some("hello"));
/// ```
pub fn first_word(text: &str) -> Option<&str> {
    text.split_whitespace().next()
}

#[cfg(test)]
mod tests {
    use super::first_word;

    #[test]
    fn blank_text_has_no_words() {
        assert_eq!(first_word(" \t\n"), None);
    }
}
