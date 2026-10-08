//! Optional shared regex preprocessing. Original strings and byte offsets stay
//! intact, while recognized values become atomic, typed comparison tokens.

use super::{Row, Token, drain2, tokenize};

pub(super) fn prepare_rows(rows: &[Row], enabled: bool) -> Vec<Row> {
    rows.iter()
        .map(|source| {
            let mut row = source.clone();
            row.tokens.clear();
            row.masked.clear();
            if !enabled {
                row.tokens = tokenize(&row.message);
                return row;
            }
            let mut cursor = 0;
            for (start, end, kind) in drain2::spans(&row.message) {
                // Tokenize only literal regions. Regex-matched values are replaced
                // conceptually before tokenization and keep their original spans.
                for token in tokenize(&row.message[cursor..start]) {
                    row.tokens.push(Token {
                        start: cursor + token.start,
                        end: cursor + token.end,
                    });
                    row.masked.push(None);
                }
                row.tokens.push(Token { start, end });
                row.masked.push(Some(kind));
                cursor = end;
            }
            for token in tokenize(&row.message[cursor..]) {
                row.tokens.push(Token {
                    start: cursor + token.start,
                    end: cursor + token.end,
                });
                row.masked.push(None);
            }
            row
        })
        .collect()
}
