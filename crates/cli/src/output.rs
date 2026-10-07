//! Result output: an aligned text table, CSV (RFC 4180) or JSON lines of objects.

use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use switchyard_core::cell_json;
use switchyard_core::db::{ColumnMeta, RowBatch};

/// Output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Aligned columns for reading.
    Table,
    /// Comma-separated values with a header row.
    Csv,
    /// One JSON object per row (JSON Lines).
    Json,
}

/// Writes one or more result sets as they stream in.
pub struct ResultWriter<W: Write> {
    out: W,
    format: Format,
    /// Rows shown at most per result set in a table (CSV / JSON are unlimited).
    table_limit: usize,
    cols: Option<Arc<[ColumnMeta]>>,
    /// Buffered cells for the table.
    table: Vec<Vec<String>>,
    rows: usize,
    sets: usize,
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

impl<W: Write> ResultWriter<W> {
    /// A writer to `out`.
    pub fn new(out: W, format: Format, table_limit: usize) -> Self {
        Self {
            out,
            format,
            table_limit,
            cols: None,
            table: Vec::new(),
            rows: 0,
            sets: 0,
        }
    }

    /// A new result set starts.
    pub fn columns(&mut self, cols: Arc<[ColumnMeta]>) -> Result<()> {
        self.finish_set()?;
        if self.sets > 0 && self.format != Format::Json {
            writeln!(self.out)?;
        }
        self.sets += 1;
        if self.format == Format::Csv {
            let header: Vec<String> = cols.iter().map(|c| csv_field(&c.name)).collect();
            writeln!(self.out, "{}", header.join(","))?;
        }
        self.cols = Some(cols);
        self.rows = 0;
        Ok(())
    }

    /// Rows of the current result set.
    pub fn rows(&mut self, batch: &RowBatch) -> Result<()> {
        let Some(cols) = self.cols.clone() else {
            return Ok(());
        };
        for r in 0..batch.len() {
            self.rows += 1;
            match self.format {
                Format::Table => {
                    if self.table.len() < self.table_limit {
                        self.table.push(
                            (0..cols.len())
                                .map(|c| {
                                    let cell = batch.cell(r, c);
                                    if cell.is_null() {
                                        "NULL".to_owned()
                                    } else {
                                        cell.to_display()
                                    }
                                })
                                .collect(),
                        );
                    }
                }
                Format::Csv => {
                    let line: Vec<String> = (0..cols.len())
                        .map(|c| {
                            let cell = batch.cell(r, c);
                            if cell.is_null() {
                                String::new()
                            } else {
                                csv_field(&cell.to_display())
                            }
                        })
                        .collect();
                    writeln!(self.out, "{}", line.join(","))?;
                }
                Format::Json => {
                    let mut obj = serde_json::Map::new();
                    for (c, meta) in cols.iter().enumerate() {
                        obj.insert(
                            meta.name.clone(),
                            cell_json(&batch.cell(r, c).to_value(meta.data_type)),
                        );
                    }
                    writeln!(self.out, "{}", serde_json::Value::Object(obj))?;
                }
            }
        }
        Ok(())
    }

    fn finish_set(&mut self) -> Result<()> {
        let Some(cols) = self.cols.take() else {
            return Ok(());
        };
        if self.format != Format::Table {
            return Ok(());
        }
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        let mut widths: Vec<usize> = names.iter().map(|n| n.chars().count()).collect();
        for row in &self.table {
            for (w, cell) in widths.iter_mut().zip(row) {
                *w = (*w).max(cell.chars().count()).min(60);
            }
        }
        let numeric: Vec<bool> = cols.iter().map(|c| c.data_type.is_numeric()).collect();
        let fit = |s: &str, w: usize, right: bool| {
            let s: String = if s.chars().count() > w {
                s.chars().take(w.saturating_sub(1)).chain(['…']).collect()
            } else {
                s.to_owned()
            };
            if right {
                format!("{s:>w$}")
            } else {
                format!("{s:<w$}")
            }
        };
        let line = |cells: &[String]| -> String { cells.join("  ").trim_end().to_owned() };
        let header: Vec<String> = names
            .iter()
            .zip(&widths)
            .map(|(n, w)| fit(n, *w, false))
            .collect();
        writeln!(self.out, "{}", line(&header))?;
        let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        writeln!(self.out, "{}", line(&rule))?;
        for row in self.table.drain(..) {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .zip(&numeric)
                .map(|((c, w), n)| fit(c, *w, *n))
                .collect();
            writeln!(self.out, "{}", line(&cells))?;
        }
        if self.rows > self.table_limit {
            writeln!(
                self.out,
                "({} rows, first {} shown; use --format csv or json for all)",
                self.rows, self.table_limit
            )?;
        } else {
            writeln!(
                self.out,
                "({} row{})",
                self.rows,
                if self.rows == 1 { "" } else { "s" }
            )?;
        }
        Ok(())
    }

    /// Flush the last result set.
    pub fn finish(mut self) -> Result<W> {
        self.finish_set()?;
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::db::{DataType, RowBatchBuilder};

    fn data() -> (Arc<[ColumnMeta]>, RowBatch) {
        let cols: Arc<[ColumnMeta]> = Arc::from(vec![
            ColumnMeta::new("id", "int8", DataType::Int64),
            ColumnMeta::new("note", "text", DataType::Text),
        ]);
        let mut b = RowBatchBuilder::for_columns(&cols, 2);
        b.push_i64(1);
        b.push_str("a, \"b\"");
        b.push_i64(20);
        b.push_null();
        (cols, b.finish())
    }

    fn render(format: Format) -> String {
        let (cols, batch) = data();
        let mut w = ResultWriter::new(Vec::new(), format, 100);
        w.columns(cols).unwrap();
        w.rows(&batch).unwrap();
        String::from_utf8(w.finish().unwrap()).unwrap()
    }

    #[test]
    fn csv_quotes_and_leaves_nulls_empty() {
        assert_eq!(render(Format::Csv), "id,note\n1,\"a, \"\"b\"\"\"\n20,\n");
    }

    #[test]
    fn json_lines_keep_types() {
        assert_eq!(
            render(Format::Json),
            "{\"id\":1,\"note\":\"a, \\\"b\\\"\"}\n{\"id\":20,\"note\":null}\n"
        );
    }

    #[test]
    fn table_aligns_numbers_right() {
        assert_eq!(
            render(Format::Table),
            "id  note\n--  ------\n 1  a, \"b\"\n20  NULL\n(2 rows)\n"
        );
    }
}
