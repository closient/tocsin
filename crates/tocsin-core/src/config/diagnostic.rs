// SPDX-License-Identifier: AGPL-3.0-only

//! A configuration problem and where it is.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// One configuration error, located as precisely as the problem allows.
///
/// Serialized with every key present (`null` when unknown), so a reader can
/// rely on the shape: `{file, line, column, path, message, hint}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Diagnostic {
    /// The file, as the loader named it; absent for a problem with the
    /// folder as a whole, such as a missing setting no file mentions.
    pub file: Option<String>,
    /// 1-based line.
    pub line: Option<usize>,
    /// 1-based column, counted in characters.
    pub column: Option<usize>,
    /// Where in the file's data the problem is, such as `check[2].timeout`.
    pub path: Option<String>,
    /// What is wrong.
    pub message: String,
    /// How to fix it, when there is something more to say.
    pub hint: Option<String>,
}

impl Diagnostic {
    /// A problem with a file that has no finer location.
    #[must_use]
    pub fn in_file(file: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            file: Some(file.into()),
            line: None,
            column: None,
            path: None,
            message: message.into(),
            hint: None,
        }
    }

    /// `file:line:column`, or as much of it as is known.
    #[must_use]
    pub fn location(&self) -> Option<String> {
        let file = self.file.as_ref()?;
        Some(match (self.line, self.column) {
            (Some(line), Some(column)) => format!("{file}:{line}:{column}"),
            (Some(line), None) => format!("{file}:{line}"),
            _ => file.clone(),
        })
    }
}

impl fmt::Display for Diagnostic {
    /// `file:line:column: message (path)`, then the hint on its own line.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(location) = self.location() {
            write!(formatter, "{location}: ")?;
        }
        formatter.write_str(&self.message)?;
        if let Some(path) = &self.path {
            write!(formatter, " ({path})")?;
        }
        if let Some(hint) = &self.hint {
            write!(formatter, "\n  hint: {hint}")?;
        }
        Ok(())
    }
}

/// Converts byte offsets in one source text to 1-based lines and columns.
pub(crate) struct LineIndex {
    /// Byte offset at which each line starts.
    starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        LineIndex { starts }
    }

    /// The 1-based line and character column of a byte offset.
    pub(crate) fn locate(&self, text: &str, offset: usize) -> (usize, usize) {
        let mut offset = offset.min(text.len());
        // An offset inside a multi-byte character counts as that character.
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.starts.partition_point(|start| *start <= offset);
        let start = self.starts[line - 1];
        let column = text[start..offset].chars().count();
        (line, column + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::LineIndex;

    #[test]
    fn offsets_become_one_based_lines_and_character_columns() {
        let text = "a = 1\nbé = 2\n\nc";
        let index = LineIndex::new(text);
        assert_eq!(index.locate(text, 0), (1, 1));
        assert_eq!(index.locate(text, 4), (1, 5));
        // The newline ending a line belongs to that line.
        assert_eq!(index.locate(text, 5), (1, 6));
        assert_eq!(index.locate(text, 6), (2, 1));
        // `é` is two bytes but one column.
        assert_eq!(index.locate(text, 10), (2, 4));
        // Inside `é` is still `é`, not the rest of the text.
        assert_eq!(index.locate(text, 8), (2, 2));
        assert_eq!(index.locate(text, 13), (2, 7));
        assert_eq!(index.locate(text, 14), (3, 1));
        assert_eq!(index.locate(text, 15), (4, 1));
        assert_eq!(index.locate(text, 99), (4, 2));
    }
}
