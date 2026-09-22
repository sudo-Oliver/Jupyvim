use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};
use syntect::parsing::SyntaxSet;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

/// Renders Python source into syntax-highlighted HTML (inline-styled `<span>`s).
pub fn highlight_python(source: &str) -> String {
    let ss = syntax_set();
    let ts = theme_set();
    let syntax = ss
        .find_syntax_by_token("python")
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let theme = &ts.themes["base16-ocean.dark"];
    let mut highlighter = HighlightLines::new(syntax, theme);

    let mut out = String::new();
    for line in source.split_inclusive('\n') {
        let Ok(ranges) = highlighter.highlight_line(line, ss) else {
            continue;
        };
        if let Ok(html) = styled_line_to_highlighted_html(&ranges[..], IncludeBackground::No) {
            out.push_str(&html);
        }
    }
    out
}

/// Renders Markdown source into HTML via pulldown-cmark.
pub fn render_markdown(source: &str) -> String {
    let parser = pulldown_cmark::Parser::new(source);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_python_keywords_as_spans() {
        let html = highlight_python("def foo():\n    return 1\n");
        assert!(html.contains("<span"), "expected syntax-highlighted spans, got: {html}");
    }

    #[test]
    fn renders_markdown_headings() {
        let html = render_markdown("# Title\n");
        assert!(html.contains("<h1>Title</h1>"));
    }
}
