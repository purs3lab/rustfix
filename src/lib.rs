//! Library for applying diagnostic suggestions to source code.
//!
//! This is a low-level library. You pass it the [JSON output] from `rustc`,
//! and you can then use it to apply suggestions to in-memory strings.
//! This library doesn't execute commands, or read or write from the filesystem.
//!
//! If you are looking for the [`cargo fix`] implementation, the core of it is
//! located in [`cargo::ops::fix`].
//!
//! [`cargo fix`]: https://doc.rust-lang.org/cargo/commands/cargo-fix.html
//! [`cargo::ops::fix`]: https://github.com/rust-lang/cargo/blob/master/src/cargo/ops/fix.rs
//! [JSON output]: diagnostics
//!
//! The general outline of how to use this library is:
//!
//! 1. Call `rustc` and collect the JSON data.
//! 2. Pass the json data to [`get_suggestions_from_json`].
//! 3. Create a [`CodeFix`] with the source of a file to modify.
//! 4. Call [`CodeFix::apply`] to apply a change.
//! 5. Call [`CodeFix::finish`] to get the result and write it back to disk.

use std::collections::HashSet;
use std::ops::Range;

pub mod diagnostics;
mod error;
mod replace;

use diagnostics::Diagnostic;
use diagnostics::DiagnosticSpan;
pub use error::Error;
use serde::Deserialize;
use serde::Serialize;

/// A filter to control which suggestion should be applied.
#[derive(Debug, Clone, Copy)]
pub enum Filter {
    /// For [`diagnostics::Applicability::MachineApplicable`] only.
    MachineApplicableOnly,
    /// Everything is included. YOLO!
    Everything,
}

/// Collects code [`Suggestion`]s from one or more compiler diagnostic lines.
///
/// Fails if any of diagnostic line `input` is not a valid [`Diagnostic`] JSON.
///
/// * `only` --- only diagnostics with code in a set of error codes would be collected.
pub fn get_suggestions_from_json<S: ::std::hash::BuildHasher>(
    input: &str,
    only: &HashSet<String, S>,
    filter: Filter,
) -> serde_json::error::Result<Vec<Suggestion>> {
    let mut result = Vec::new();
    for cargo_msg in serde_json::Deserializer::from_str(input).into_iter::<Diagnostic>() {
        // One diagnostic line might have multiple suggestions
        result.extend(collect_suggestions(&cargo_msg?, only, filter));
    }
    Ok(result)
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinePosition {
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for LinePosition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: LinePosition,
    pub end: LinePosition,
}

impl std::fmt::Display for LineRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

/// An error/warning and possible solutions for fixing it
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    pub message: String,
    pub snippets: Vec<Snippet>,
    pub solutions: Vec<Solution>,
}

/// Solution to a diagnostic item.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Solution {
    /// The error message of the diagnostic item.
    pub message: String,
    /// Possible solutions to fix the error.
    pub replacements: Vec<Replacement>,
}

/// Represents code that will get replaced.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    pub file_name: String,
    pub line_range: LineRange,
    pub range: Range<usize>,
}

/// Represents a replacement of a `snippet`.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    /// Code snippet that gets replaced.
    pub snippet: Snippet,
    /// The replacement of the snippet.
    pub replacement: String,
}

/// Converts a [`DiagnosticSpan`] to a [`Snippet`].
pub fn span_to_snippet(span: &DiagnosticSpan) -> Snippet {
    Snippet {
        file_name: span.file_name.clone(),
        line_range: LineRange {
            start: LinePosition {
                line: span.line_start,
                column: span.column_start,
            },
            end: LinePosition {
                line: span.line_end,
                column: span.column_end,
            },
        },
        range: (span.byte_start as usize)..(span.byte_end as usize),
    }
}

/// Converts a [`DiagnosticSpan`] into a [`Replacement`].
pub fn collect_span(span: &DiagnosticSpan) -> Option<Replacement> {
    let snippet = span_to_snippet(span);
    let replacement = span.suggested_replacement.clone()?;
    Some(Replacement {
        snippet,
        replacement,
    })
}

/// Collects code [`Suggestion`]s from a single compiler diagnostic line.
///
/// * `only` --- only diagnostics with code in a set of error codes would be collected.
pub fn collect_suggestions<S: ::std::hash::BuildHasher>(
    diagnostic: &Diagnostic,
    only: &HashSet<String, S>,
    filter: Filter,
) -> Option<Suggestion> {
    tracing::debug!("Hello -- from forked rustfix");
    if !only.is_empty() {
        if let Some(ref code) = diagnostic.code {
            if !only.contains(&code.code) {
                // This is not the code we are looking for
                return None;
            }
        } else {
            // No code, probably a weird builtin warning/error
            return None;
        }
    }

    let snippets = diagnostic.spans.iter().map(span_to_snippet).collect();

    let solutions: Vec<_> = diagnostic
        .children
        .iter()
        .filter_map(|child| {
            let replacements: Vec<_> = child
                .spans
                .iter()
                .filter(|span| {
                    use crate::diagnostics::Applicability::*;
                    use crate::Filter::*;

                    match (filter, &span.suggestion_applicability) {
                        (MachineApplicableOnly, Some(MachineApplicable)) => true,
                        (MachineApplicableOnly, _) => false,
                        (Everything, _) => true,
                    }
                })
                .filter_map(collect_span)
                .collect();
            if !replacements.is_empty() {
                Some(Solution {
                    message: child.message.clone(),
                    replacements,
                })
            } else {
                None
            }
        })
        .collect();

    if solutions.is_empty() {
        None
    } else {
        Some(Suggestion {
            message: diagnostic.message.clone(),
            snippets,
            solutions,
        })
    }
}

/// Represents a code fix. This doesn't write to disks but is only in memory.
///
/// The general way to use this is:
///
/// 1. Feeds the source of a file to [`CodeFix::new`].
/// 2. Calls [`CodeFix::apply`] to apply suggestions to the source code.
/// 3. Calls [`CodeFix::finish`] to get the "fixed" code.
pub struct CodeFix {
    data: replace::Data,
    /// Whether or not the data has been modified.
    modified: bool,
}

impl CodeFix {
    /// Creates a `CodeFix` with the source of a file to modify.
    pub fn new(s: &str) -> CodeFix {
        CodeFix {
            data: replace::Data::new(s.as_bytes()),
            modified: false,
        }
    }

    /// Applies a suggestion to the code.
    pub fn apply(&mut self, suggestion: &Suggestion) -> Result<(), Error> {
        for sol in &suggestion.solutions {
            for r in &sol.replacements {
                self.data
                    .replace_range(r.snippet.range.clone(), r.replacement.as_bytes())?;
                self.modified = true;
            }
        }
        Ok(())
    }

    /// Gets the result of the "fixed" code.
    pub fn finish(&self) -> Result<String, Error> {
        Ok(String::from_utf8(self.data.to_vec())?)
    }

    /// Returns whether or not the data has been modified.
    pub fn modified(&self) -> bool {
        self.modified
    }
}

/// Applies multiple `suggestions` to the given `code`.
pub fn apply_suggestions(code: &str, suggestions: &[Suggestion]) -> Result<String, Error> {
    let mut already_applied = HashSet::new();
    let mut fix = CodeFix::new(code);
    for suggestion in suggestions.iter().rev() {
        // This assumes that if any of the machine applicable fixes in
        // a diagnostic suggestion is a duplicate, we should see the
        // entire suggestion as a duplicate.
        if suggestion
            .solutions
            .iter()
            .any(|sol| !already_applied.insert(sol))
        {
            continue;
        }
        fix.apply(suggestion)?;
    }
    fix.finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeStatus {
    Modified,
    NotModified,
}

impl CodeStatus {
    pub fn or(&self, other: Self) -> Self {
        match self {
            CodeStatus::Modified => CodeStatus::Modified,
            CodeStatus::NotModified => other,
        }
    }
}

impl From<bool> for CodeStatus {
    fn from(modified: bool) -> Self {
        if modified {
            Self::Modified
        } else {
            Self::NotModified
        }
    }
}

pub fn apply_suggestions_with_outcome(
    code: &str,
    suggestions: &[Suggestion],
) -> Result<(String, CodeStatus), Error> {
    let mut already_applied = HashSet::new();
    let mut fix = CodeFix::new(code);
    for suggestion in suggestions.iter().rev() {
        // This assumes that if any of the machine applicable fixes in
        // a diagnostic suggestion is a duplicate, we should see the
        // entire suggestion as a duplicate.
        if suggestion
            .solutions
            .iter()
            .any(|sol| !already_applied.insert(sol))
        {
            continue;
        }
        fix.apply(suggestion)?;
    }
    let final_code = fix.finish()?;
    Ok((final_code, fix.modified().into()))
}
