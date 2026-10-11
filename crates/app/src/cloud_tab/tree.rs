//! Keys grouped by prefix: `App:Db:Host` sits in folder `App` › `Db`. A folder holding a
//! single key is not shown; the key is listed with the rest of its name instead.

use std::collections::{BTreeMap, HashSet};

/// One row of the grouped list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// A folder: every key under `path` (which ends with the delimiter).
    Group {
        /// Full prefix, delimiter included (`App:Db:`).
        path: String,
        /// Last segment (`Db`).
        name: String,
        /// Nesting level (0 at the top).
        depth: usize,
        /// Keys under it, at any depth.
        count: usize,
        /// Children are listed.
        open: bool,
    },
    /// A key (index into the item list).
    Item {
        /// Index into the item list.
        ix: usize,
        /// Nesting level.
        depth: usize,
        /// Name below the parent folder.
        name: String,
    },
}

/// Folders and keys for `keys`, with the folders in `expanded` (or all, with `all_open`)
/// opened.
pub fn rows(keys: &[&str], delim: &str, expanded: &HashSet<String>, all_open: bool) -> Vec<Row> {
    let entries: Vec<(usize, &str)> = keys.iter().copied().enumerate().collect();
    let mut out = Vec::new();
    build(&entries, "", 0, delim, expanded, all_open, &mut out);
    out
}

/// Under one segment: keys ending there, and keys going deeper (index, rest of the key).
type Bucket<'a> = (Vec<(usize, &'a str)>, Vec<(usize, &'a str)>);

fn build(
    entries: &[(usize, &str)],
    prefix: &str,
    depth: usize,
    delim: &str,
    expanded: &HashSet<String>,
    all_open: bool,
    out: &mut Vec<Row>,
) {
    let mut by_segment: BTreeMap<String, Bucket<'_>> = BTreeMap::new();
    for &(ix, rest) in entries {
        // A leading delimiter (`/app/x`) is part of the first segment.
        let split = rest
            .get(1..)
            .and_then(|r| r.find(delim))
            .map(|p| p + 1)
            .filter(|_| !delim.is_empty());
        match split {
            Some(p) if p + delim.len() < rest.len() => {
                let b = by_segment.entry(rest[..p].to_lowercase()).or_default();
                b.1.push((ix, rest));
            }
            _ => by_segment
                .entry(rest.to_lowercase())
                .or_default()
                .0
                .push((ix, rest)),
        }
    }
    for (_, (exact, deeper)) in by_segment {
        for (ix, name) in exact {
            out.push(Row::Item {
                ix,
                depth,
                name: name.to_owned(),
            });
        }
        let Some(&(_, first)) = deeper.first() else {
            continue;
        };
        let seg_len = first
            .get(1..)
            .and_then(|r| r.find(delim))
            .map_or(first.len(), |p| p + 1);
        if deeper.len() == 1 {
            out.push(Row::Item {
                ix: deeper[0].0,
                depth,
                name: first.to_owned(),
            });
            continue;
        }
        let segment = &first[..seg_len];
        let path = format!("{prefix}{segment}{delim}");
        let open = all_open || expanded.contains(&path);
        out.push(Row::Group {
            path: path.clone(),
            name: segment.to_owned(),
            depth,
            count: deeper.len(),
            open,
        });
        if open {
            let children: Vec<(usize, &str)> = deeper
                .iter()
                .map(|&(ix, rest)| (ix, &rest[seg_len + delim.len()..]))
                .collect();
            build(&children, &path, depth + 1, delim, expanded, all_open, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::Group {
                    name,
                    depth,
                    count,
                    open,
                    ..
                } => format!(
                    "{}{}{name} ({count})",
                    "  ".repeat(*depth),
                    if *open { "v " } else { "> " }
                ),
                Row::Item { ix, depth, name } => format!("{}{name} #{ix}", "  ".repeat(*depth)),
            })
            .collect()
    }

    #[test]
    fn groups_by_prefix() {
        let keys = [
            "APPLICATIONINSIGHTS",
            "App:Db:Host",
            "App:Db:Port",
            "App:Name",
            "Azure:SignalR:Conn",
            "Cache:Redis",
            "Cache:Source",
        ];
        let closed = rows(&keys, ":", &HashSet::new(), false);
        assert_eq!(
            show(&closed),
            [
                "> App (3)",
                "APPLICATIONINSIGHTS #0",
                "Azure:SignalR:Conn #4",
                "> Cache (2)",
            ]
        );
        let open: HashSet<String> = ["App:".to_owned(), "App:Db:".to_owned()].into();
        assert_eq!(
            show(&rows(&keys, ":", &open, false)),
            [
                "v App (3)",
                "  v Db (2)",
                "    Host #1",
                "    Port #2",
                "  Name #3",
                "APPLICATIONINSIGHTS #0",
                "Azure:SignalR:Conn #4",
                "> Cache (2)",
            ]
        );
    }

    #[test]
    fn multi_char_delimiter_and_labels() {
        // The same key under two labels is two items in one folder.
        let keys = ["Conn--Db", "Conn--Db", "Conn--Redis", "Token"];
        assert_eq!(
            show(&rows(&keys, "--", &HashSet::new(), true)),
            ["v Conn (3)", "  Db #0", "  Db #1", "  Redis #2", "Token #3"]
        );
    }

    #[test]
    fn leading_delimiter_paths() {
        let keys = ["/app/db/host", "/app/db/port"];
        assert_eq!(
            show(&rows(&keys, "/", &HashSet::new(), true)),
            ["v /app (2)", "  v db (2)", "    host #0", "    port #1"]
        );
    }
}
