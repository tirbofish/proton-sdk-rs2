#[derive(Debug)]
pub struct ReplUnclosedQuoteError;

impl std::fmt::Display for ReplUnclosedQuoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Unclosed quote in command line")
    }
}

impl std::error::Error for ReplUnclosedQuoteError {}

pub fn split_quoted_line(line: &str) -> Result<Vec<String>, ReplUnclosedQuoteError> {
    let mut result = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
        i += 1;
    }
    while i < chars.len() {
        let c = chars[i];
        match in_quote {
            Some('"') => {
                if c == '\\' && i + 1 < chars.len() && (chars[i + 1] == '"' || chars[i + 1] == '\\')
                {
                    current.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if c == '"' {
                    in_quote = None;
                    i += 1;
                    continue;
                }
                current.push(c);
                i += 1;
            }
            Some('\'') => {
                if c == '\'' {
                    in_quote = None;
                    i += 1;
                    continue;
                }
                current.push(c);
                i += 1;
            }
            _ => {
                if c == '"' || c == '\'' {
                    in_quote = Some(c);
                    i += 1;
                    continue;
                }
                if c == ' ' || c == '\t' {
                    result.push(std::mem::take(&mut current));
                    i += 1;
                    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                        i += 1;
                    }
                    continue;
                }
                current.push(c);
                i += 1;
            }
        }
    }
    if in_quote.is_some() {
        return Err(ReplUnclosedQuoteError);
    }
    result.push(current);
    Ok(result)
}

pub fn local_file_media_type(path: &str) -> String {
    let lower = std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match lower.as_str() {
        "jpg" | "jpeg" => "image/jpeg".into(),
        "png" => "image/png".into(),
        "txt" => "text/plain".into(),
        _ => "application/octet-stream".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_quoted_line_matches_typescript() {
        assert_eq!(
            split_quoted_line(r#""ab \"c\" d""#).unwrap(),
            vec!["ab \"c\" d"]
        );
        assert!(split_quoted_line("abc\"").is_err());
        assert!(split_quoted_line("abc'").is_err());
        assert!(split_quoted_line("\"").is_err());
        assert!(split_quoted_line("'").is_err());
        assert_eq!(
            split_quoted_line("one two  three").unwrap(),
            vec!["one", "two", "three"]
        );
        assert_eq!(split_quoted_line(r#"ab"c d"ef"#).unwrap(), vec!["abc def"]);
        assert_eq!(split_quoted_line("ab'c d'ef").unwrap(), vec!["abc def"]);
        assert_eq!(split_quoted_line(r#"a "" b"#).unwrap(), vec!["a", "", "b"]);
        assert_eq!(split_quoted_line("a '' b").unwrap(), vec!["a", "", "b"]);
        assert_eq!(split_quoted_line(r#""ab c d""#).unwrap(), vec!["ab c d"]);
        assert_eq!(split_quoted_line("'ab c d'").unwrap(), vec!["ab c d"]);
    }

    #[test]
    fn local_file_media_type_matches_common_extensions() {
        assert_eq!(local_file_media_type("folder/photo.jpg"), "image/jpeg");
        assert_eq!(local_file_media_type("folder/photo.JPG"), "image/jpeg");
        assert_eq!(local_file_media_type("folder/photo.png"), "image/png");
        assert_eq!(
            local_file_media_type("folder/archive.xyz"),
            "application/octet-stream"
        );
        assert_eq!(
            local_file_media_type("folder/readme"),
            "application/octet-stream"
        );
    }
}
