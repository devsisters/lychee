//! Locate SQL code blocks in Markdown documents.

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

use crate::extract::markdown::md_extensions;

/// Info string attribute which excludes a code block from SQL checks,
/// e.g. ```` ```sql ignore ```` or ```` ```sql,ignore ````.
const IGNORE_ATTRIBUTE: &str = "ignore";

/// A fenced SQL code block of a Markdown document.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SqlBlock {
    /// The code between the fences.
    pub(crate) text: String,
    /// Byte offset of the opening fence in the document.
    start: usize,
    /// Maps byte offsets in `text` back to byte offsets in the document.
    ///
    /// Each entry `(text offset, document offset)` marks the start of a piece
    /// of `text` that was copied verbatim from the document. Pieces are
    /// separated by markup which is not part of the code, such as the
    /// indentation of a code block nested in a list item.
    pieces: Vec<(usize, usize)>,
}

impl SqlBlock {
    /// Returns the byte offset in the document of a position in this block.
    ///
    /// `line` and `column` are 1-based, and columns are counted in characters,
    /// matching the locations reported by `sqlparser`.
    pub(crate) fn document_offset(&self, line: u64, column: u64) -> usize {
        let preceding_lines = usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX);
        let line_start: usize = self
            .text
            .split_inclusive('\n')
            .take(preceding_lines)
            .map(str::len)
            .sum();
        let column_offset: usize = self.text[line_start..]
            .chars()
            .take_while(|&c| c != '\n')
            .take(usize::try_from(column.saturating_sub(1)).unwrap_or(usize::MAX))
            .map(char::len_utf8)
            .sum();
        let offset = line_start + column_offset;

        let piece = self
            .pieces
            .partition_point(|&(text_offset, _)| text_offset <= offset);
        match piece.checked_sub(1).map(|i| self.pieces[i]) {
            Some((text_offset, document_offset)) => document_offset + (offset - text_offset),
            None => self.start,
        }
    }
}

/// Returns the fenced code blocks of a Markdown document whose language is
/// one of `languages` (compared case-insensitively).
///
/// Blocks with an [`IGNORE_ATTRIBUTE`] in their info string are skipped.
pub(crate) fn find_sql_blocks(input: &str, languages: &[String]) -> Vec<SqlBlock> {
    let mut blocks = Vec::new();
    let mut current: Option<SqlBlock> = None;

    for (event, range) in Parser::new_ext(input, md_extensions()).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                if is_checked_sql_block(&info, languages) =>
            {
                current = Some(SqlBlock {
                    start: range.start,
                    ..SqlBlock::default()
                });
            }
            Event::Text(text) => {
                if let Some(block) = current.as_mut() {
                    block.pieces.push((block.text.len(), range.start));
                    block.text.push_str(&text);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(block) = current.take() {
                    blocks.push(block);
                }
            }
            _ => {}
        }
    }

    blocks
}

/// Whether a code block with the given info string should be checked.
///
/// The first attribute is the language. Attributes can be separated by
/// whitespace or commas and may use Pandoc's `{.sql .ignore}` style.
fn is_checked_sql_block(info: &str, languages: &[String]) -> bool {
    let mut attributes = info
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '{' | '}'))
        .map(|attribute| attribute.trim_start_matches('.'))
        .filter(|attribute| !attribute.is_empty());

    let Some(language) = attributes.next() else {
        return false;
    };

    languages
        .iter()
        .any(|sql_language| sql_language.eq_ignore_ascii_case(language))
        && !attributes.any(|attribute| attribute.eq_ignore_ascii_case(IGNORE_ATTRIBUTE))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn sql_languages() -> Vec<String> {
        vec!["sql".to_string()]
    }

    fn texts(input: &str) -> Vec<String> {
        find_sql_blocks(input, &sql_languages())
            .into_iter()
            .map(|block| block.text)
            .collect()
    }

    #[test]
    fn test_finds_fenced_sql_blocks_only() {
        let input = "\
# Title

```sql
SELECT 1;
```

```python
print(1)
```

    SELECT 'indented code blocks have no language';

~~~SQL
SELECT 2;
~~~

```
SELECT 'no language';
```
";
        assert_eq!(texts(input), ["SELECT 1;\n", "SELECT 2;\n"]);
    }

    #[test]
    fn test_info_string_attributes() {
        let input = "\
```sql title=\"query.sql\"
SELECT 1;
```

```sql ignore
SELECT 2;
```

```sql,ignore
SELECT 3;
```

```{.sql .ignore}
SELECT 4;
```

```{sql, connection=con}
SELECT 5;
```
";
        assert_eq!(texts(input), ["SELECT 1;\n", "SELECT 5;\n"]);
    }

    #[test]
    fn test_custom_languages() {
        let input = "```databricks\nSELECT 1;\n```\n\n```sql\nSELECT 2;\n```\n";
        let blocks = find_sql_blocks(input, &["databricks".to_string()]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "SELECT 1;\n");
    }

    #[test]
    fn test_document_offset_top_level_block() {
        let input = "Intro\n\n```sql\nSELECT a\nFROM t\n```\n";
        let block = &find_sql_blocks(input, &sql_languages())[0];

        assert_eq!(&input[block.document_offset(1, 1)..][..6], "SELECT");
        assert_eq!(&input[block.document_offset(2, 6)..][..1], "t");
    }

    #[test]
    fn test_document_offset_nested_block() {
        // The list item indentation and blockquote markers are not part of
        // the code, so each line is mapped back separately.
        let input = "\
- Item

  ```sql
  SELECT a
  FROM t
  ```

> ```sql
> SELECT b
> FROM u
> ```
";
        let blocks = find_sql_blocks(input, &sql_languages());
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "SELECT a\nFROM t\n");
        assert_eq!(blocks[1].text, "SELECT b\nFROM u\n");

        assert_eq!(&input[blocks[0].document_offset(1, 8)..][..1], "a");
        assert_eq!(&input[blocks[0].document_offset(2, 6)..][..1], "t");
        assert_eq!(&input[blocks[1].document_offset(1, 8)..][..1], "b");
        assert_eq!(&input[blocks[1].document_offset(2, 6)..][..1], "u");
    }

    #[test]
    fn test_document_offset_crlf() {
        let input = "Intro\r\n\r\n```sql\r\nSELECT a\r\nFROM t\r\n```\r\n";
        let block = &find_sql_blocks(input, &sql_languages())[0];

        assert_eq!(&input[block.document_offset(1, 8)..][..1], "a");
        assert_eq!(&input[block.document_offset(2, 6)..][..1], "t");
    }

    #[test]
    fn test_document_offset_counts_characters() {
        let input = "```sql\nSELECT '한글', x\n```\n";
        let block = &find_sql_blocks(input, &sql_languages())[0];
        assert_eq!(&input[block.document_offset(1, 14)..][..1], "x");
    }
}
