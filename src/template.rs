use crate::metadata::FileMeta;

/// A parsed rename template, e.g. `{date_taken:%Y}/{date_taken:%m}/{name}`.
#[derive(Debug, Clone)]
pub struct Template {
    tokens: Vec<Token>,
}

#[derive(Debug, Clone)]
enum Token {
    Literal(String),
    /// `{field}` or `{field:fmt}`
    Field { name: String, fmt: Option<String> },
}

#[derive(Debug, Clone)]
pub enum RenderError {
    /// Field name not recognized or no value for this file.
    MissingField(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::MissingField(name) => write!(f, "no value for {{{}}}", name),
        }
    }
}

impl Template {
    /// Parse a template string. Unbalanced braces are treated as literals so
    /// the user never gets a hard parse failure while typing.
    pub fn parse(input: &str) -> Template {
        let mut tokens = Vec::new();
        let mut chars = input.chars().peekable();
        let mut lit = String::new();

        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    let mut inner = String::new();
                    let mut closed = false;
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        if nc == '}' {
                            closed = true;
                            break;
                        }
                        inner.push(nc);
                    }
                    if closed {
                        if !lit.is_empty() {
                            tokens.push(Token::Literal(std::mem::take(&mut lit)));
                        }
                        let (name, fmt) = match inner.split_once(':') {
                            Some((n, f)) => (n.trim().to_string(), Some(f.to_string())),
                            None => (inner.trim().to_string(), None),
                        };
                        tokens.push(Token::Field { name, fmt });
                    } else {
                        // no closing brace — keep verbatim
                        lit.push('{');
                        lit.push_str(&inner);
                    }
                }
                _ => lit.push(c),
            }
        }
        if !lit.is_empty() {
            tokens.push(Token::Literal(lit));
        }
        Template { tokens }
    }

    /// Render to a relative path string. `index` feeds the `{n}` counter
    /// (1-based); `fmt` on `n` is a zero-pad width, e.g. `{n:03}`.
    pub fn render(&self, meta: &FileMeta, index: usize) -> Result<String, RenderError> {
        let mut out = String::new();
        for tok in &self.tokens {
            match tok {
                Token::Literal(s) => out.push_str(s),
                Token::Field { name, fmt } => {
                    if name == "n" {
                        let width: usize = fmt
                            .as_deref()
                            .and_then(|f| f.trim_start_matches('0').parse().ok())
                            .or_else(|| fmt.as_deref().map(|f| f.len()))
                            .unwrap_or(0);
                        out.push_str(&format!("{:0>width$}", index, width = width));
                    } else {
                        let val = meta
                            .field(name, fmt.as_deref())
                            .ok_or_else(|| RenderError::MissingField(name.clone()))?;
                        out.push_str(&sanitize_component(&val));
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Strip characters illegal in path components, but keep `/` so the template
/// can express subfolders. Applied per-field so a stray `/` inside a value
/// (e.g. a camera model) does not accidentally create folders.
fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::FileMeta;

    fn meta() -> FileMeta {
        let mut m = FileMeta::default();
        m.name = "IMG_0042".into();
        m.ext = "JPG".into();
        m.camera_model = Some("Pixel 8".into());
        m
    }

    #[test]
    fn literal_and_field() {
        let t = Template::parse("photo_{name}");
        assert_eq!(t.render(&meta(), 1).unwrap(), "photo_IMG_0042");
    }

    #[test]
    fn counter_padding() {
        let t = Template::parse("{n:03}_{name}");
        assert_eq!(t.render(&meta(), 7).unwrap(), "007_IMG_0042");
    }

    #[test]
    fn missing_field_errors() {
        let t = Template::parse("{date_taken:%Y}");
        assert!(t.render(&meta(), 1).is_err());
    }

    #[test]
    fn slash_in_value_sanitized_but_template_slash_kept() {
        let mut m = meta();
        m.camera_model = Some("a/b".into());
        let t = Template::parse("{parent}/{camera_model}");
        // template slash preserved, value slash -> underscore
        assert!(t.render(&m, 1).unwrap().ends_with("/a_b"));
    }

    #[test]
    fn unclosed_brace_is_literal() {
        let t = Template::parse("{name");
        assert_eq!(t.render(&meta(), 1).unwrap(), "{name");
    }
}
