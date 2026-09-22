use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct Cell {
    pub cell_type: String,
    pub source: String,
    pub outputs: Vec<Value>,
    pub execution_count: Option<i64>,
    pub metadata: Value,
    /// Server-rendered HTML for this cell's *source* (syntax-highlighted
    /// code or rendered markdown) -- `None` means "needs (re)computing".
    /// Populated lazily by the `/api/notebook` handler and carried over on
    /// re-sync whenever the source text didn't actually change, so a
    /// debounced live-typing sync only re-highlights the one cell that was
    /// actually edited, not the whole notebook every ~120ms.
    pub rendered_html: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Notebook {
    pub cells: Vec<Cell>,
    pub metadata: Value,
    pub nbformat: i64,
    pub nbformat_minor: i64,
}

impl Cell {
    fn from_ipynb_value(v: &Value) -> Self {
        let cell_type = v
            .get("cell_type")
            .and_then(|x| x.as_str())
            .unwrap_or("code")
            .to_string();
        let source = match v.get("source") {
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(""),
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let outputs = v
            .get("outputs")
            .and_then(|o| o.as_array())
            .cloned()
            .unwrap_or_default();
        let execution_count = v.get("execution_count").and_then(|x| x.as_i64());
        let metadata = v.get("metadata").cloned().unwrap_or_else(|| json!({}));
        Cell {
            cell_type,
            source,
            outputs,
            execution_count,
            metadata,
            rendered_html: None,
        }
    }

    fn to_ipynb_value(&self) -> Value {
        let lines: Vec<String> = if self.source.is_empty() {
            vec![]
        } else {
            let parts: Vec<&str> = self.source.split('\n').collect();
            let n = parts.len();
            parts
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    if i + 1 < n {
                        format!("{}\n", p)
                    } else {
                        p.to_string()
                    }
                })
                .collect()
        };
        let mut obj = json!({
            "cell_type": self.cell_type,
            "source": lines,
            "metadata": self.metadata,
        });
        if self.cell_type == "code" {
            obj["outputs"] = Value::Array(self.outputs.clone());
            obj["execution_count"] = self
                .execution_count
                .map(Value::from)
                .unwrap_or(Value::Null);
        }
        obj
    }
}

impl Notebook {
    pub fn empty() -> Self {
        let welcome = Cell {
            cell_type: "code".to_string(),
            source: "# Welcome to Jupyvim!\nprint(\"Hello from Neovim & Rust!\")".to_string(),
            outputs: vec![],
            execution_count: None,
            metadata: json!({}),
            rendered_html: None,
        };
        Notebook {
            cells: vec![welcome],
            metadata: json!({}),
            nbformat: 4,
            nbformat_minor: 5,
        }
    }

    pub fn from_ipynb_value(v: &Value) -> Self {
        let nbformat = v.get("nbformat").and_then(|x| x.as_i64()).unwrap_or(4);
        let nbformat_minor = v
            .get("nbformat_minor")
            .and_then(|x| x.as_i64())
            .unwrap_or(5);
        let metadata = v.get("metadata").cloned().unwrap_or_else(|| json!({}));
        let cells = v
            .get("cells")
            .and_then(|c| c.as_array())
            .map(|arr| arr.iter().map(Cell::from_ipynb_value).collect())
            .unwrap_or_default();
        Notebook {
            cells,
            metadata,
            nbformat,
            nbformat_minor,
        }
    }

    pub fn to_ipynb_value(&self) -> Value {
        json!({
            "cells": self.cells.iter().map(Cell::to_ipynb_value).collect::<Vec<_>>(),
            "metadata": self.metadata,
            "nbformat": self.nbformat,
            "nbformat_minor": self.nbformat_minor,
        })
    }

    /// Renders the notebook as a Jupytext-compatible "percent" format script.
    pub fn to_percent(&self) -> String {
        self.to_percent_with_line_starts().0
    }

    /// How many lines a single cell occupies in the percent-format text --
    /// shared by `to_percent_with_line_starts` (which needs the running
    /// total) and `line_starts` (which needs only the totals, no string).
    fn percent_line_count(cell: &Cell) -> usize {
        if cell.cell_type == "markdown" {
            // 1 marker line + 1 line per source line (each always ends in
            // '\n' when emitted, see to_percent_with_line_starts) + 1 blank
            // separator line.
            2 + cell.source.split('\n').count()
        } else {
            // 1 marker line + however many newlines the source itself has
            // + 1 forced newline if the source doesn't already end in one
            // + 1 blank separator line.
            let trailing = usize::from(!cell.source.is_empty() && !cell.source.ends_with('\n'));
            2 + cell.source.matches('\n').count() + trailing
        }
    }

    /// 1-based line number of each cell's `# %%` marker in the percent-
    /// format text, without building the text itself -- used by
    /// `/api/notebook`, which is fetched far more often (every live-typing
    /// sync) than the mirror file is actually regenerated.
    pub fn line_starts(&self) -> Vec<usize> {
        let mut line_starts = Vec::with_capacity(self.cells.len());
        let mut current_line = 1;
        for cell in &self.cells {
            line_starts.push(current_line);
            current_line += Self::percent_line_count(cell);
        }
        line_starts
    }

    /// Same as `to_percent`, but also returns the 1-based line number of
    /// each cell's `# %%` marker in the generated text -- lets the browser
    /// jump Neovim's cursor to the right line on click. Line numbers are
    /// tracked incrementally (not by rescanning the growing string on every
    /// cell, which would be quadratic in the number of cells) so this and
    /// `line_starts` share the exact same per-cell counting logic.
    pub fn to_percent_with_line_starts(&self) -> (String, Vec<usize>) {
        let mut out = String::new();
        let mut line_starts = Vec::with_capacity(self.cells.len());
        let mut current_line = 1;
        for cell in &self.cells {
            line_starts.push(current_line);
            current_line += Self::percent_line_count(cell);
            if cell.cell_type == "markdown" {
                out.push_str("# %% [markdown]\n");
                for line in cell.source.split('\n') {
                    if line.is_empty() {
                        out.push_str("#\n");
                    } else {
                        out.push_str("# ");
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            } else {
                if cell.cell_type == "raw" {
                    out.push_str("# %% [raw]\n");
                } else {
                    out.push_str("# %%\n");
                }
                out.push_str(&cell.source);
                if !cell.source.ends_with('\n') {
                    out.push('\n');
                }
            }
            out.push('\n');
        }
        (out, line_starts)
    }

    /// Parses a Jupytext-"percent" formatted script back into a Notebook.
    /// Outputs/execution_count are carried over from `previous` by matching
    /// cell index + type, since the plain-text mirror carries no execution
    /// state of its own.
    ///
    /// A plain Python script with no `# %%` markers at all (e.g. a fresh
    /// `uv init` scaffold the user is bootstrapping into a notebook) is
    /// treated as a single leading code cell rather than being dropped.
    pub fn from_percent(text: &str, previous: Option<&Notebook>) -> Self {
        let mut cells: Vec<Cell> = Vec::new();
        let mut current: Option<(String, Vec<String>)> = Some(("code".to_string(), Vec::new()));
        let mut seen_marker = false;

        fn flush(current: Option<(String, Vec<String>)>, cells: &mut Vec<Cell>) {
            let Some((cell_type, lines)) = current else {
                return;
            };
            let source = if cell_type == "markdown" {
                lines
                    .into_iter()
                    .map(|l| {
                        if l == "#" {
                            String::new()
                        } else if let Some(stripped) = l.strip_prefix("# ") {
                            stripped.to_string()
                        } else if let Some(stripped) = l.strip_prefix('#') {
                            stripped.to_string()
                        } else {
                            l
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                lines.join("\n")
            };
            let source = source.trim_end().to_string();
            cells.push(Cell {
                cell_type,
                source,
                outputs: vec![],
                execution_count: None,
                metadata: json!({}),
                rendered_html: None,
            });
        }

        fn has_content(current: &Option<(String, Vec<String>)>) -> bool {
            current
                .as_ref()
                .is_some_and(|(_, lines)| lines.iter().any(|l| !l.trim().is_empty()))
        }

        for raw_line in text.lines() {
            let trimmed = raw_line.trim_end();
            if trimmed.starts_with("# %%") {
                if seen_marker || has_content(&current) {
                    flush(current.take(), &mut cells);
                }
                seen_marker = true;
                let cell_type = if trimmed.contains("[markdown]") {
                    "markdown"
                } else if trimmed.contains("[raw]") {
                    "raw"
                } else {
                    "code"
                };
                current = Some((cell_type.to_string(), Vec::new()));
            } else if let Some((_, lines)) = current.as_mut() {
                lines.push(raw_line.to_string());
            }
        }
        if !seen_marker && !has_content(&current) {
            current = None;
        }
        flush(current.take(), &mut cells);

        if let Some(prev) = previous {
            for (i, cell) in cells.iter_mut().enumerate() {
                if let Some(old) = prev.cells.get(i) {
                    if old.cell_type == cell.cell_type {
                        cell.outputs = old.outputs.clone();
                        cell.execution_count = old.execution_count;
                        // Source text unchanged -> the syntax-highlighted/markdown
                        // HTML we already computed for it is still valid, so a
                        // debounced live-typing sync only pays the syntect/
                        // pulldown-cmark cost for the one cell that actually
                        // changed, not the whole notebook every ~120ms.
                        if old.source == cell.source {
                            cell.rendered_html = old.rendered_html.clone();
                        }
                    }
                }
            }
        }

        let (metadata, nbformat, nbformat_minor) = previous
            .map(|p| (p.metadata.clone(), p.nbformat, p.nbformat_minor))
            .unwrap_or((json!({}), 4, 5));

        Notebook {
            cells,
            metadata,
            nbformat,
            nbformat_minor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_code_and_markdown() {
        let nb = Notebook {
            cells: vec![
                Cell {
                    cell_type: "markdown".to_string(),
                    source: "# Title\n\nSome text".to_string(),
                    outputs: vec![],
                    execution_count: None,
                    metadata: json!({}),
                    rendered_html: None,
                },
                Cell {
                    cell_type: "code".to_string(),
                    source: "import numpy as np\nprint(np.arange(3))".to_string(),
                    outputs: vec![json!({"output_type": "stream", "text": ["0 1 2"]})],
                    execution_count: Some(1),
                    metadata: json!({}),
                    rendered_html: None,
                },
            ],
            metadata: json!({}),
            nbformat: 4,
            nbformat_minor: 5,
        };

        let percent = nb.to_percent();
        let parsed = Notebook::from_percent(&percent, Some(&nb));

        assert_eq!(parsed.cells.len(), 2);
        assert_eq!(parsed.cells[0].cell_type, "markdown");
        assert_eq!(parsed.cells[0].source, "# Title\n\nSome text");
        assert_eq!(parsed.cells[1].cell_type, "code");
        assert_eq!(
            parsed.cells[1].source,
            "import numpy as np\nprint(np.arange(3))"
        );
        assert_eq!(parsed.cells[1].execution_count, Some(1));
        assert_eq!(parsed.cells[1].outputs.len(), 1);
    }

    #[test]
    fn line_starts_matches_actual_text() {
        let nb = Notebook {
            cells: vec![
                Cell {
                    cell_type: "markdown".to_string(),
                    source: "Title\n\nmulti\nline\nbody".to_string(),
                    outputs: vec![],
                    execution_count: None,
                    metadata: json!({}),
                    rendered_html: None,
                },
                Cell {
                    cell_type: "code".to_string(),
                    source: "a = 1\nb = 2\nc = 3".to_string(),
                    outputs: vec![],
                    execution_count: None,
                    metadata: json!({}),
                    rendered_html: None,
                },
                Cell {
                    cell_type: "code".to_string(),
                    source: "no_trailing_newline_here".to_string(),
                    outputs: vec![],
                    execution_count: None,
                    metadata: json!({}),
                    rendered_html: None,
                },
            ],
            metadata: json!({}),
            nbformat: 4,
            nbformat_minor: 5,
        };

        let (text, line_starts_from_text) = nb.to_percent_with_line_starts();
        assert_eq!(nb.line_starts(), line_starts_from_text);

        let lines: Vec<&str> = text.lines().collect();
        for (i, &start) in line_starts_from_text.iter().enumerate() {
            assert!(
                lines[start - 1].starts_with("# %%"),
                "cell {} line_start {} doesn't point at a marker line: {:?}",
                i,
                start,
                lines.get(start - 1)
            );
        }
    }
}
