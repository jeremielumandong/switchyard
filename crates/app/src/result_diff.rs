//! Compare two result sets by row hash (DBX-4d).
//!
//! Rows are hashed straight from the columnar batches (no per-cell strings). Rows whose
//! hash and values match on the common columns are unchanged; the rest are paired by the
//! first common column (the key): a removed and an added row with the same key count as
//! one changed row. Text is only formatted for the diff lines on screen.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};

use switchyard_core::db::{BatchList, CellRef, ColumnMeta};

/// One differing row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowChange {
    /// Only in the second result (row index in B).
    Added(u32),
    /// Only in the first result (row index in A).
    Removed(u32),
    /// Same key, other values: (row in A, row in B).
    Changed(u32, u32),
}

/// The difference between result A and result B.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultDiff {
    /// Columns both results have, by name: (index in A, index in B, name).
    pub columns: Vec<(usize, usize, String)>,
    /// Columns only A has.
    pub only_a: Vec<String>,
    /// Columns only B has.
    pub only_b: Vec<String>,
    /// Differing rows: removed and changed in A order, then added in B order.
    pub rows: Vec<RowChange>,
    /// Rows only in B.
    pub added: usize,
    /// Rows only in A.
    pub removed: usize,
    /// Rows whose key matched but values differ.
    pub changed: usize,
    /// Rows present in both.
    pub unchanged: usize,
}

fn hash_cell(h: &mut DefaultHasher, cell: CellRef<'_>) {
    match cell {
        CellRef::Null => 0u8.hash(h),
        CellRef::Bool(b) => (1u8, b).hash(h),
        CellRef::Int(i) => (2u8, i).hash(h),
        CellRef::Float(f) => (3u8, f.to_bits()).hash(h),
        CellRef::Date(d) => (4u8, d).hash(h),
        CellRef::Time(t) => (5u8, t).hash(h),
        CellRef::Timestamp(t) => (6u8, t).hash(h),
        CellRef::TimestampTz(t) => (7u8, t).hash(h),
        CellRef::Uuid(u) => (8u8, u).hash(h),
        CellRef::Text(s) => (9u8, s).hash(h),
        CellRef::Bytes(b) => (10u8, b).hash(h),
    }
}

/// Whether two cells hold the same value (floats by bit pattern, like the hash).
pub fn same_cell(a: CellRef<'_>, b: CellRef<'_>) -> bool {
    match (a, b) {
        (CellRef::Float(x), CellRef::Float(y)) => x.to_bits() == y.to_bits(),
        (x, y) => x == y,
    }
}

/// One hash per row over `cols`, reading each batch column by column.
fn row_hashes(data: &BatchList, cols: &[usize]) -> Vec<u64> {
    let mut out = Vec::with_capacity(data.len());
    for batch in data.batches() {
        let mut hashers: Vec<DefaultHasher> =
            (0..batch.len()).map(|_| DefaultHasher::new()).collect();
        for &c in cols {
            for (r, h) in hashers.iter_mut().enumerate() {
                hash_cell(h, batch.cell(r, c));
            }
        }
        out.extend(hashers.into_iter().map(|h| h.finish()));
    }
    out
}

fn rows_equal(a: &BatchList, ra: u32, b: &BatchList, rb: u32, cols: &[(usize, usize)]) -> bool {
    cols.iter().all(
        |&(ca, cb)| match (a.cell(ra as usize, ca), b.cell(rb as usize, cb)) {
            (Some(x), Some(y)) => same_cell(x, y),
            (x, y) => x.is_none() && y.is_none(),
        },
    )
}

/// Compare the loaded rows of two results.
pub fn diff(
    a_cols: &[ColumnMeta],
    a: &BatchList,
    b_cols: &[ColumnMeta],
    b: &BatchList,
) -> ResultDiff {
    let mut columns = Vec::new();
    let mut only_a = Vec::new();
    for (ia, ca) in a_cols.iter().enumerate() {
        match b_cols.iter().position(|cb| cb.name == ca.name) {
            Some(ib) => columns.push((ia, ib, ca.name.clone())),
            None => only_a.push(ca.name.clone()),
        }
    }
    let only_b = b_cols
        .iter()
        .filter(|cb| !a_cols.iter().any(|ca| ca.name == cb.name))
        .map(|c| c.name.clone())
        .collect();
    let pairs: Vec<(usize, usize)> = columns.iter().map(|&(x, y, _)| (x, y)).collect();
    let a_ix: Vec<usize> = pairs.iter().map(|p| p.0).collect();
    let b_ix: Vec<usize> = pairs.iter().map(|p| p.1).collect();

    // Exact matches by full-row hash (a multiset: duplicates pair up one to one).
    let ha = row_hashes(a, &a_ix);
    let hb = row_hashes(b, &b_ix);
    let mut by_hash: HashMap<u64, Vec<u32>> = HashMap::with_capacity(ha.len());
    for (r, h) in ha.iter().enumerate().rev() {
        by_hash.entry(*h).or_default().push(r as u32);
    }
    let mut matched_a = vec![false; ha.len()];
    let mut unmatched_b = Vec::new();
    let mut unchanged = 0;
    for (rb, h) in hb.iter().enumerate() {
        let rb = rb as u32;
        let hit = by_hash.get_mut(h).and_then(|cands| {
            let pos = cands
                .iter()
                .rposition(|&ra| rows_equal(a, ra, b, rb, &pairs))?;
            Some(cands.remove(pos))
        });
        match hit {
            Some(ra) => {
                matched_a[ra as usize] = true;
                unchanged += 1;
            }
            None => unmatched_b.push(rb),
        }
    }

    // Pair what is left by the key column.
    let key = pairs.first().copied();
    let key_hash = |data: &BatchList, row: u32, col: usize| {
        let mut h = DefaultHasher::new();
        hash_cell(
            &mut h,
            data.cell(row as usize, col).unwrap_or(CellRef::Null),
        );
        h.finish()
    };
    let mut by_key: HashMap<u64, Vec<u32>> = HashMap::new();
    if let Some((ka, _)) = key {
        for ra in (0..ha.len() as u32)
            .rev()
            .filter(|&r| !matched_a[r as usize])
        {
            by_key.entry(key_hash(a, ra, ka)).or_default().push(ra);
        }
    }
    let mut changed_pairs = Vec::new();
    let mut added = Vec::new();
    for rb in unmatched_b {
        let pair = key.and_then(|(ka, kb)| {
            let cands = by_key.get_mut(&key_hash(b, rb, kb))?;
            let kb_cell = b.cell(rb as usize, kb).unwrap_or(CellRef::Null);
            let pos = cands.iter().rposition(|&ra| {
                same_cell(a.cell(ra as usize, ka).unwrap_or(CellRef::Null), kb_cell)
            })?;
            Some(cands.remove(pos))
        });
        match pair {
            Some(ra) => {
                matched_a[ra as usize] = true;
                changed_pairs.push((ra, rb));
            }
            None => added.push(rb),
        }
    }

    let mut rows: Vec<RowChange> = Vec::new();
    let removed: Vec<u32> = (0..ha.len() as u32)
        .filter(|&r| !matched_a[r as usize])
        .collect();
    let changed = changed_pairs.len();
    let (n_removed, n_added) = (removed.len(), added.len());
    let mut in_a: Vec<(u32, RowChange)> = removed
        .into_iter()
        .map(|r| (r, RowChange::Removed(r)))
        .chain(
            changed_pairs
                .into_iter()
                .map(|(x, y)| (x, RowChange::Changed(x, y))),
        )
        .collect();
    in_a.sort_by_key(|(r, _)| *r);
    rows.extend(in_a.into_iter().map(|(_, c)| c));
    rows.extend(added.into_iter().map(RowChange::Added));
    ResultDiff {
        columns,
        only_a,
        only_b,
        rows,
        added: n_added,
        removed: n_removed,
        changed,
        unchanged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::db::{DataType, RowBatchBuilder};

    fn result(
        names: &[&str],
        rows: &[(i64, Option<&str>, f64)],
        split: usize,
    ) -> (Vec<ColumnMeta>, BatchList) {
        let all = [
            ColumnMeta::new("id", "int8", DataType::Int64),
            ColumnMeta::new("name", "text", DataType::Text),
            ColumnMeta::new("score", "float8", DataType::Float64),
        ];
        let cols: Vec<ColumnMeta> = all
            .into_iter()
            .filter(|c| names.contains(&c.name.as_str()))
            .collect();
        let mut list = BatchList::default();
        // Several batches, so rows cross batch boundaries.
        for chunk in rows.chunks(split.max(1)) {
            let mut b = RowBatchBuilder::for_columns(&cols, chunk.len());
            for (id, name, score) in chunk {
                for c in &cols {
                    match c.name.as_str() {
                        "id" => b.push_i64(*id),
                        "name" => match name {
                            Some(n) => b.push_str(n),
                            None => b.push_null(),
                        },
                        _ => b.push_f64(*score),
                    }
                }
            }
            list.push(b.finish());
        }
        (cols, list)
    }

    const ALL: &[&str] = &["id", "name", "score"];

    #[test]
    fn identical_results_have_no_differences() {
        let rows = [(1, Some("a"), 1.0), (2, None, 2.5), (3, Some("c"), 3.0)];
        let (ca, a) = result(ALL, &rows, 2);
        let (cb, b) = result(ALL, &rows, 1);
        let d = diff(&ca, &a, &cb, &b);
        assert_eq!((d.added, d.removed, d.changed, d.unchanged), (0, 0, 0, 3));
        assert!(d.rows.is_empty());
    }

    #[test]
    fn counts_added_removed_and_changed_rows() {
        let (ca, a) = result(
            ALL,
            &[
                (1, Some("a"), 1.0),
                (2, Some("b"), 2.0),
                (3, Some("c"), 3.0),
                (4, None, 4.0),
            ],
            3,
        );
        // 1 unchanged, 2 changed (name), 3 removed, 4 changed (NULL → value), 5 added;
        // order in B does not matter.
        let (cb, b) = result(
            ALL,
            &[
                (5, Some("e"), 5.0),
                (4, Some("d"), 4.0),
                (2, Some("B"), 2.0),
                (1, Some("a"), 1.0),
            ],
            2,
        );
        let d = diff(&ca, &a, &cb, &b);
        assert_eq!((d.added, d.removed, d.changed, d.unchanged), (1, 1, 2, 1));
        assert_eq!(
            d.rows,
            vec![
                RowChange::Changed(1, 2),
                RowChange::Removed(2),
                RowChange::Changed(3, 1),
                RowChange::Added(0),
            ]
        );
    }

    #[test]
    fn duplicates_pair_one_to_one() {
        let (ca, a) = result(ALL, &[(1, Some("a"), 1.0), (1, Some("a"), 1.0)], 5);
        let (cb, b) = result(ALL, &[(1, Some("a"), 1.0)], 5);
        let d = diff(&ca, &a, &cb, &b);
        assert_eq!((d.added, d.removed, d.changed, d.unchanged), (0, 1, 0, 1));
        assert_eq!(d.rows, vec![RowChange::Removed(1)]);
    }

    #[test]
    fn compares_common_columns_by_name() {
        let (ca, a) = result(ALL, &[(1, Some("a"), 1.0), (2, Some("b"), 2.0)], 5);
        let (cb, b) = result(
            &["id", "name"],
            &[(1, Some("a"), 0.0), (2, Some("x"), 0.0)],
            5,
        );
        let d = diff(&ca, &a, &cb, &b);
        assert_eq!(d.only_a, vec!["score".to_owned()]);
        assert!(d.only_b.is_empty());
        assert_eq!(d.columns.len(), 2);
        assert_eq!((d.added, d.removed, d.changed, d.unchanged), (0, 0, 1, 1));
    }
}
