use std::{
    borrow::Cow,
    fmt,
    fs::File,
    io::{self, Read, BufRead, BufReader},
    path::{Path, PathBuf},
    str::FromStr,
};

use grep_searcher::LineIter;
use encoding_rs_io::DecodeReaderBytesBuilder;

use crate::{
    config::Config,
    language::syntax::SyntaxCounter,
    stats::Stats,
    utils::{ext::SliceExt, fs as fsutils},
};

use self::LanguageType::*;

include!(concat!(env!("OUT_DIR"), "/language_type.rs"));

/// a struct representing summarised information for a language chunk - e.g. Stats
pub trait LanguageSummary {
    /// construct a new summary
    fn new(name: PathBuf) -> Self;
    /// handle 'blank' files, where all lines treated as code
    fn unprocessed_lines(&mut self, lines:LineIter);
    /// handle a line of code
    fn code_line(&mut self, line:&[u8]);
    /// handle a comment line
    fn comment_line(&mut self, line:&[u8]);
    /// handle a blank line
    fn blank_line(&mut self, line:&[u8]);
    /// processing after aggregation - e.g. for calculating totals
    fn postprocess(&mut self);
}

impl LanguageSummary for Stats {
    fn new(name: PathBuf) -> Self {
        Stats::new(name)
    }
    fn unprocessed_lines(&mut self, lines:LineIter) {
        let count = lines.count();
        self.code += count;
        self.lines += count;
    }
    fn code_line(&mut self, _line:&[u8]) {
        self.code += 1;
        trace!("Code No.{}", self.code);
    }
    fn comment_line(&mut self, _line:&[u8]) {
        self.comments += 1;
        trace!("Comment No.{}", self.comments);
    }
    fn blank_line(&mut self, _line:&[u8]) {
        self.blanks += 1;
        trace!("Blank No.{}", self.blanks);
    }
    fn postprocess(&mut self) {
        self.lines = self.blanks + self.code + self.comments;
    }
}

impl LanguageType {
    /// Parses a given `Path` using the `LanguageType`. Returning `Stats`
    /// on success and giving back ownership of PathBuf on error.
    pub fn parse<T:LanguageSummary>(self, path: PathBuf, config: &Config) -> Result<T, (io::Error, PathBuf)> {
        let text = {
            let f = match File::open(&path) {
                Ok(f) => f,
                Err(e) => return Err((e, path)),
            };
            let mut s = Vec::new();
            let mut reader = DecodeReaderBytesBuilder::new()
                                .build(f);

            if let Err(e) = reader.read_to_end(&mut s) {
                return Err((e, path));
            }
            s
        };

        Ok(self.parse_from_slice(path, &text, config))
    }

    /// Parses the text provided. Returns `Stats` on success.
    pub fn parse_from_str<T: LanguageSummary, A: AsRef<str>>(self,
                          path: PathBuf,
                          text: A,
                          config: &Config)
        -> T
    {
        self.parse_from_slice(path, text.as_ref().as_bytes(), config)
    }

    /// Parses the text provided. Returning `Stats` on success.
    pub fn parse_from_slice<T: LanguageSummary, A: AsRef<[u8]>>(self,
                          path: PathBuf,
                          text: A,
                          config: &Config)
        -> T
    {
        let lines = LineIter::new(b'\n', text.as_ref());
        let mut summary = T::new(path);

        if self.is_blank() {
            summary.unprocessed_lines(lines);
            summary
        } else {
            self.parse_lines(config, lines, summary)
        }
    }

    /// Attempts to parse the line as simply as possible if there are no multi
    /// line comments or quotes. Returns `bool` indicating whether it was
    /// successful or not.
    #[inline]
    fn parse_basic<T: LanguageSummary>(self, syntax: &SyntaxCounter, raw_line: &[u8], line: &[u8], stats: &mut T)
        -> bool
    {
        if syntax.quote.is_some() ||
           !syntax.stack.is_empty() ||
           syntax.important_syntax().any(|s| line.contains_slice(s.as_bytes()))
        {
            return false;
        }

        if syntax.line_comments.iter()
                               .any(|s| line.starts_with(s.as_bytes()))
        {
            stats.comment_line(raw_line);
        } else {
            stats.code_line(raw_line);
        }

        trace!("{}", String::from_utf8_lossy(line));
        trace!("^ Skippable.");

        true
    }

    #[inline]
    fn parse_lines<'a, T: LanguageSummary>(self,
                       config: &Config,
                       lines: impl IntoIterator<Item=&'a [u8]>,
                       mut stats: T)
        -> T
    {
        let mut syntax = SyntaxCounter::new(self);

        for raw_line in lines {

            if raw_line.trim().is_empty() {
                stats.blank_line(raw_line);
                continue;
            }

            // FORTRAN has a rule where it only counts as a comment if it's the
            // first character in the column, so removing starting whitespace
            // could cause a miscount.
            let line = if syntax.is_fortran { raw_line } else { raw_line.trim() };
            let had_multi_line = !syntax.stack.is_empty();
            let mut ended_with_comments = false;
            let mut skip = 0;
            macro_rules! skip {
                ($skip:expr) => {{
                    skip = $skip - 1;
                }}
            }

            if self.parse_basic(&syntax, raw_line, line, &mut stats) {
                continue;
            }

            'window: for i in 0..line.len() {
                if skip != 0 {
                    skip -= 1;
                    continue;
                }

                ended_with_comments = false;
                let window = &line[i..];

                let is_end_of_quote_or_multi_line =
                    syntax.parse_end_of_quote(window)
                    .or_else(|| syntax.parse_end_of_multi_line(window));

                if let Some(skip_amount) = is_end_of_quote_or_multi_line {
                    ended_with_comments = true;
                    skip!(skip_amount);
                    continue;
                } else if syntax.quote.is_some() {
                    continue;
                }

                let is_quote_or_multi_line = syntax.parse_quote(window)
                    .or_else(|| syntax.parse_multi_line_comment(window));

                if let Some(skip_amount) = is_quote_or_multi_line {
                    skip!(skip_amount);
                    continue;
                }

                if syntax.parse_line_comment(window) {
                    ended_with_comments = true;
                    break 'window;
                }

            }

            trace!("{}", String::from_utf8_lossy(line));

            let is_comments =
                (
                    (!syntax.stack.is_empty() || ended_with_comments) &&
                     had_multi_line
                ) ||
                (
                    // If we're currently in a comment or we just ended
                    // with one.
                    syntax.start_of_comments().any(|comment| {
                        line.starts_with(comment.as_bytes())
                    }) &&
                    syntax.quote.is_none()
                ) ||
                (
                    (
                        // If we're currently in a doc string or we just ended
                        // with one.
                        syntax.quote.is_some() ||
                        syntax.doc_quotes.iter().any(|(s, _)| line.starts_with(s.as_bytes()))
                    ) &&
                    // `Some(true)` is import in order to respect the current
                    // configuration.
                    config.treat_doc_strings_as_comments == Some(true) &&
                    syntax.quote_is_doc_quote
                );


            if is_comments {
                stats.comment_line(raw_line);
                trace!("Was the Comment stack empty?: {}", !had_multi_line);
            } else {
                stats.code_line(raw_line);
            }
        }

        stats.postprocess();
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_allows_nested() {
        assert!(LanguageType::Rust.allows_nested());
    }
}
