//! Decode benchmarks: building columnar batches from PostgreSQL binary values.
//! Run with `cargo bench -p switchyard-db`.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use postgres_types::Type;
use switchyard_db::pg::decode::{data_type, push_value};
use switchyard_db::{BatchList, ColumnMeta, DataType, RowBatchBuilder};

fn numeric_bytes() -> Vec<u8> {
    // 4812.40 → ndigits 2, weight 0, sign 0, dscale 2, digits [4812, 4000]
    let mut raw = Vec::new();
    for v in [2u16, 0, 0, 2, 4812, 4000] {
        raw.extend_from_slice(&v.to_be_bytes());
    }
    raw
}

fn decode_rows(c: &mut Criterion) {
    let types = [
        Type::INT8,
        Type::TEXT,
        Type::NUMERIC,
        Type::TIMESTAMPTZ,
        Type::BOOL,
    ];
    let cols: Vec<ColumnMeta> = types
        .iter()
        .map(|t| ColumnMeta::new(t.name(), t.name(), data_type(t)))
        .collect();
    let int8 = 1_204_871i64.to_be_bytes();
    let text = b"user30814@example.com".to_vec();
    let numeric = numeric_bytes();
    let ts = 812_345_678_000_000i64.to_be_bytes();
    let boolean = [1u8];
    let values: [&[u8]; 5] = [&int8, &text, &numeric, &ts, &boolean];
    let mut group = c.benchmark_group("decode");
    group.throughput(Throughput::Elements(1000));
    group.bench_function("1000 rows x 5 mixed columns", |b| {
        let mut scratch = String::new();
        b.iter(|| {
            let mut builder = RowBatchBuilder::for_columns(&cols, 1000);
            for _ in 0..1000 {
                for (i, t) in types.iter().enumerate() {
                    push_value(&mut builder, data_type(t), t, values[i], &mut scratch);
                }
            }
            builder.finish()
        })
    });
    group.finish();
}

fn grid_lookup(c: &mut Criterion) {
    let cols = vec![ColumnMeta::new("id", "int8", DataType::Int64); 10];
    let mut list = BatchList::default();
    for b in 0..1000 {
        let mut builder = RowBatchBuilder::for_columns(&cols, 1000);
        for r in 0..1000i64 {
            for c in 0..10 {
                builder.push_i64(b * 1000 + r + c);
            }
        }
        list.push(builder.finish());
    }
    c.bench_function("format one screen (40 rows x 10 cols) at row 500k", |b| {
        let mut s = String::new();
        b.iter(|| {
            for r in 500_000..500_040 {
                for c in 0..10 {
                    s.clear();
                    if let Some(cell) = list.cell(r, c) {
                        cell.write_display(&mut s, 120);
                    }
                }
            }
        })
    });
}

fn grid_sort(c: &mut Criterion) {
    // A NUMERIC column arrives as text: the sort compares it as numbers.
    let cols = vec![ColumnMeta::new("amount", "numeric", DataType::Numeric)];
    let mut list = BatchList::default();
    let mut s = String::new();
    for b in 0..100u64 {
        let mut builder = RowBatchBuilder::for_columns(&cols, 1000);
        for r in 0..1000u64 {
            s.clear();
            let v = (b * 1000 + r).wrapping_mul(2_654_435_761) % 1_000_000;
            s.push_str(&format!("{}.{:02}", v, v % 100));
            builder.push_str(&s);
        }
        list.push(builder.finish());
    }
    c.bench_function("sort 100k numeric-text rows", |b| {
        b.iter(|| {
            let mut rows: Vec<u32> = (0..list.len() as u32).collect();
            list.sort_rows(&mut rows, 0, false);
            rows
        })
    });
    c.bench_function("filter 100k rows", |b| {
        b.iter(|| list.rows_containing("12.3"))
    });
}

/// 100k rows × 8 columns typical of a table scan: ids, names, amounts, timestamps.
fn mixed_list() -> BatchList {
    let cols = [
        ColumnMeta::new("id", "int8", DataType::Int64),
        ColumnMeta::new("account_id", "int4", DataType::Int32),
        ColumnMeta::new("email", "text", DataType::Text),
        ColumnMeta::new("status", "text", DataType::Text),
        ColumnMeta::new("amount", "float8", DataType::Float64),
        ColumnMeta::new("created_at", "timestamptz", DataType::TimestampTz),
        ColumnMeta::new("updated_at", "timestamp", DataType::Timestamp),
        ColumnMeta::new("active", "bool", DataType::Bool),
    ];
    let mut list = BatchList::default();
    let mut s = String::new();
    for b in 0..100i64 {
        let mut builder = RowBatchBuilder::for_columns(&cols, 1000);
        for r in 0..1000i64 {
            let n = b * 1000 + r;
            builder.push_i64(n);
            builder.push_i64(n % 997);
            s.clear();
            s.push_str(&format!("user{n}@example.com"));
            builder.push_str(&s);
            builder.push_str(if n % 3 == 0 { "open" } else { "closed" });
            builder.push_f64(n as f64 * 1.25);
            builder.push_i64(1_700_000_000_000_000 + n * 1_234_567);
            builder.push_i64(1_700_000_000_000_000 + n * 7_654_321);
            builder.push_bool(n % 2 == 0);
        }
        list.push(builder.finish());
    }
    list
}

fn grid_mixed(c: &mut Criterion) {
    let list = mixed_list();
    c.bench_function("format one screen (40 rows x 8 mixed cols)", |b| {
        let mut s = String::new();
        b.iter(|| {
            for r in 50_000..50_040 {
                for c in 0..8 {
                    s.clear();
                    if let Some(cell) = list.cell(r, c) {
                        cell.write_display(&mut s, 120);
                    }
                }
            }
        })
    });
    c.bench_function("filter 100k mixed rows (word)", |b| {
        b.iter(|| list.rows_containing("user4242"))
    });
    c.bench_function("filter 100k mixed rows (number)", |b| {
        b.iter(|| list.rows_containing("4242"))
    });
}

criterion_group!(benches, decode_rows, grid_lookup, grid_sort, grid_mixed);
criterion_main!(benches);
