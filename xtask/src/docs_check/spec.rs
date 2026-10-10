//! Reading `docs/spec.md`: section numbers and the §11 shortcut table.

use std::collections::BTreeSet;

/// A row of the §11 table.
#[derive(Debug, PartialEq, Eq)]
pub struct ShortcutRow {
    /// First cell: the command name(s), e.g. `Next / previous tab`.
    pub commands: String,
    /// Second cell: the default shortcut(s), e.g. `Ctrl+Tab, Ctrl+PgDn / Ctrl+Shift+Tab`.
    pub shortcuts: String,
}

/// `2` for `## 2. Editor`, `2.1` for `### 2.1 Live rendering`; `None` for other lines.
fn heading_number(line: &str) -> Option<String> {
    let rest = line.strip_prefix("##")?;
    let rest = rest.strip_prefix('#').unwrap_or(rest);
    let rest = rest.strip_prefix(' ')?;
    let token = rest.split_whitespace().next()?;
    let token = token.strip_suffix('.').unwrap_or(token);
    let numeric = token
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    numeric.then(|| token.to_owned())
}

/// The lines of `spec` outside fenced code blocks.
fn prose_lines(spec: &str) -> impl Iterator<Item = &str> {
    let mut in_fence = false;
    spec.lines().filter(move |line| {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            return false;
        }
        !in_fence
    })
}

/// Every numbered section heading (`##` and `###`) of the spec.
pub fn section_numbers(spec: &str) -> BTreeSet<String> {
    prose_lines(spec).filter_map(heading_number).collect()
}

fn table_cells(line: &str) -> Vec<&str> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect()
}

/// The data rows of the table in section 11 (header and separator rows skipped).
pub fn shortcut_rows(spec: &str) -> Vec<ShortcutRow> {
    let mut in_section = false;
    let mut rows = Vec::new();
    for line in prose_lines(spec) {
        if line.starts_with("## ") {
            in_section = heading_number(line).as_deref() == Some("11");
            continue;
        }
        if !in_section || !line.trim_start().starts_with('|') {
            continue;
        }
        let cells = table_cells(line);
        let [first, second] = cells.as_slice() else {
            continue;
        };
        let separator = first.chars().all(|c| matches!(c, '-' | ':' | ' '));
        if separator || *first == "Command" {
            continue;
        }
        rows.push(ShortcutRow {
            commands: (*first).to_owned(),
            shortcuts: (*second).to_owned(),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = "\
# Title

## 1. Window

```
## 9. Not a heading (inside a fence)
```

### 1.1 Sub

#### 1.1.1 Too deep

## 2. Editor
text with ## 5. in the middle

## 11. Keyboard
- bullet

| Command | Default |
|---|---|
| New file / New window | Ctrl+N / Ctrl+Shift+N |
| Zoom in / out / reset | Ctrl+= / Ctrl+- / Ctrl+0 |
| Three | cells | here |

## 12. After
| Command | Default |
| Not | included |
";

    #[test]
    fn section_numbers_include_h2_and_h3_but_not_fenced_or_deeper_headings() {
        let numbers: Vec<String> = section_numbers(SPEC).into_iter().collect();
        assert_eq!(numbers, vec!["1", "1.1", "11", "12", "2"]);
    }

    #[test]
    fn shortcut_rows_are_the_two_cell_data_rows_of_section_11() {
        assert_eq!(
            shortcut_rows(SPEC),
            vec![
                ShortcutRow {
                    commands: "New file / New window".to_owned(),
                    shortcuts: "Ctrl+N / Ctrl+Shift+N".to_owned()
                },
                ShortcutRow {
                    commands: "Zoom in / out / reset".to_owned(),
                    shortcuts: "Ctrl+= / Ctrl+- / Ctrl+0".to_owned()
                },
            ]
        );
    }

    #[test]
    fn the_real_spec_has_the_expected_sections_and_table() {
        let workspace = crate::workspace::Workspace::locate().unwrap();
        let spec = crate::fsx::read_text(&workspace.path("docs/spec.md")).unwrap();
        let numbers = section_numbers(&spec);
        for wanted in ["1", "2", "2.1", "3.3", "4.6", "11", "12", "13", "14"] {
            assert!(numbers.contains(wanted), "section {wanted}");
        }
        assert!(shortcut_rows(&spec).len() >= 20);
    }
}
