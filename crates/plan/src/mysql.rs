//! MySQL / MariaDB plans → [`Plan`].
//!
//! * [`parse_json`]: `EXPLAIN FORMAT=JSON` (estimated, both servers) and MariaDB's
//!   `ANALYZE FORMAT=JSON` (actual: `r_rows`, `r_loops`, `r_*_time_ms`).
//! * [`parse_tree`]: MySQL's `EXPLAIN ANALYZE` text tree (8.0.18+, actual).
//!
//! MySQL's JSON lists the tables of a join in `nested_loop` order (a left-deep join); it
//! becomes nested `Nested Loop` nodes so the join rules see an outer and an inner side.
//! Costs are MySQL's own units; `prefix_cost` is the cost of the join up to that table.
//! The tree reports rows and times per loop (like PostgreSQL), so a node's subtree time
//! is the last-row time times its loops.

use serde_json::Value;

use crate::model::{Plan, PlanKind, PlanNode, PlanSource, Predicate, warn};
use crate::{PlanError, Result};

/// Parse `EXPLAIN FORMAT=JSON` (or MariaDB `ANALYZE FORMAT=JSON`) output for `sql`.
pub fn parse_json(json: &str, sql: &str) -> Result<Plan> {
    let doc: Value =
        serde_json::from_str(json.trim()).map_err(|e| PlanError::Parse(e.to_string()))?;
    let block = doc
        .get("query_block")
        .ok_or_else(|| PlanError::Parse("no \"query_block\" in EXPLAIN output".into()))?;
    let root = query_block(block);
    let actual = root.walk().iter().any(|n| n.actual_rows.is_some());
    let kind = if actual {
        PlanKind::Actual
    } else {
        PlanKind::Estimated
    };
    let mut plan = Plan::new(PlanSource::MySql, kind, sql, root);
    if actual {
        plan.execution_ms = num(&block["r_total_time_ms"]).or(plan.root.total_time_ms);
    }
    Ok(plan)
}

/// A number MySQL may write as a string (`"103.25"`) or a number.
fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(a) => Some(a.iter().filter_map(text).collect::<Vec<_>>().join(", ")),
        _ => None,
    }
}

/// Keys holding sub-queries (each an array of `{ query_block }`, or one object).
const SUBQUERY_KEYS: &[&str] = &[
    "attached_subqueries",
    "optimized_away_subqueries",
    "select_list_subqueries",
    "having_subqueries",
    "order_by_subqueries",
    "group_by_subqueries",
    "update_value_subqueries",
    // MariaDB.
    "subqueries",
];

/// One `query_block`: its operations, with the block's total cost on top.
fn query_block(v: &Value) -> PlanNode {
    let mut nodes = content(v);
    let mut top = if nodes.is_empty() {
        let mut n = PlanNode::op("Query Block");
        if let Some(m) = text(&v["message"]).or_else(|| text(&v["table"]["message"])) {
            n.details.insert("Message".into(), m);
        }
        n
    } else {
        let mut first = nodes.remove(0);
        first.children.extend(nodes);
        first
    };
    let cost = num(&v["cost_info"]["query_cost"]).or(num(&v["cost"]));
    if top.cost.is_none() {
        top.cost = cost;
    }
    if top.total_time_ms.is_none() {
        top.total_time_ms = num(&v["r_total_time_ms"]);
    }
    if let Some(id) = text(&v["select_id"]) {
        top.details.insert("Select Id".into(), id);
    }
    top
}

/// The plan nodes an object holds, in order: its operation, then any sub-queries.
fn content(v: &Value) -> Vec<PlanNode> {
    let mut out = Vec::new();
    if let Some(t) = v.get("table") {
        out.push(table(t));
    }
    if let Some(n) = v["nested_loop"].as_array().and_then(|i| nested_loop(i)) {
        out.push(n);
    }
    if let Some(b) = v.get("block-nl-join") {
        // MariaDB: the inner table of a join read through a join buffer.
        let mut n = table(&b["table"]);
        n.details.insert(
            "Join Buffer".into(),
            text(&b["join_type"]).unwrap_or_default(),
        );
        if let Some(c) = text(&b["attached_condition"]) {
            n.predicates.push(Predicate {
                kind: "Join Filter".into(),
                text: c,
            });
        }
        out.push(n);
    }
    if let Some(u) = v.get("union_result") {
        out.push(union(u));
    }
    for (key, op) in [
        ("ordering_operation", "Sort"),
        ("grouping_operation", "Aggregate"),
        ("duplicates_removal", "Remove Duplicates"),
        ("windowing", "Window"),
        ("buffer_result", "Buffer Result"),
        // MariaDB.
        ("filesort", "Sort"),
        ("temporary_table", "Temporary Table"),
        ("read_sorted_file", "Read Sorted File"),
    ] {
        if let Some(w) = v.get(key).filter(|w| w.is_object()) {
            out.extend(wrapper(key, op, w));
        }
    }
    if let Some(q) = v.get("query_block") {
        out.push(query_block(q));
    }
    for key in SUBQUERY_KEYS {
        out.extend(subqueries(&v[*key], key));
    }
    out
}

fn subqueries(v: &Value, key: &str) -> Vec<PlanNode> {
    let items: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![v],
        _ => return Vec::new(),
    };
    items
        .into_iter()
        .filter_map(|s| s.get("query_block").map(|q| (s, q)))
        .map(|(s, q)| {
            let mut n = query_block(q);
            n.details
                .insert("Parent Relationship".into(), key.replace('_', " "));
            if s["dependent"] == true {
                n.details.insert("Dependent".into(), "true".into());
            }
            n
        })
        .collect()
}

/// An operation wrapping other content (`ordering_operation`, `grouping_operation`, …).
/// An ORDER BY satisfied by an index adds nothing and is skipped.
fn wrapper(key: &str, op: &str, w: &Value) -> Vec<PlanNode> {
    let mut inner = content(w);
    let filesort = w["using_filesort"] == true || key == "filesort";
    let temp = w["using_temporary_table"] == true || key == "temporary_table";
    if key == "ordering_operation" && !filesort && !temp {
        return inner;
    }
    let mut n = PlanNode::op(op);
    if filesort {
        n.warnings.push(format!("{} (filesort)", warn::SORT));
    }
    if temp {
        n.warnings.push(warn::TEMP_TABLE.into());
    }
    if let Some(k) = text(&w["sort_key"]) {
        n.details.insert("Sort Key".into(), k);
    }
    let own = num(&w["cost_info"]["sort_cost"]);
    let kids: f64 = inner.iter().filter_map(|c| c.cost).sum();
    n.cost = own.map(|c| c + kids).or((kids > 0.0).then_some(kids));
    // MariaDB ANALYZE.
    n.loops = num(&w["r_loops"]);
    n.actual_rows = num(&w["r_output_rows"]);
    n.total_time_ms = num(&w["r_total_time_ms"]);
    if w["r_used_priority_queue"] == true {
        n.details
            .insert("Used Priority Queue".into(), "true".into());
    }
    if !inner.is_empty() {
        let first = inner.remove(0);
        // MySQL gives wrappers no row estimate; at most what comes in (an upper bound
        // for grouping) lets the rules size them. Not on measured (MariaDB) wrappers,
        // where it would read as a bad estimate.
        if n.actual_rows.is_none() {
            n.estimated_rows = first.estimated_rows;
        }
        n.children.push(first);
        n.children.extend(inner);
    }
    vec![n]
}

fn union(u: &Value) -> PlanNode {
    let mut n = PlanNode::op("Union");
    n.object = text(&u["table_name"]);
    if u["using_temporary_table"] == true {
        n.details
            .insert("Deduplicates".into(), "temporary table".into());
    }
    if let Some(specs) = u["query_specifications"].as_array() {
        for s in specs {
            if let Some(q) = s.get("query_block") {
                n.children.push(query_block(q));
            }
        }
    }
    let kids: f64 = n.children.iter().filter_map(|c| c.cost).sum();
    n.cost = (kids > 0.0).then_some(kids);
    n.loops = num(&u["r_loops"]);
    n.actual_rows = num(&u["r_rows"]);
    n
}

/// Operation name for an access type, matching `EXPLAIN ANALYZE`'s vocabulary.
fn access_operation(t: &Value) -> String {
    let covering = t["using_index"] == true;
    let access = t["access_type"].as_str().unwrap_or("");
    let op = match access {
        "ALL" => "Table scan",
        "index" if covering => "Covering index scan",
        "index" => "Index scan",
        "range" if covering => "Covering index range scan",
        "range" => "Index range scan",
        "ref" | "ref_or_null" | "index_subquery" if covering => "Covering index lookup",
        "ref" | "ref_or_null" | "index_subquery" => "Index lookup",
        "eq_ref" | "unique_subquery" => "Single-row index lookup",
        "const" | "system" => "Constant row",
        "fulltext" => "Full-text index search",
        "index_merge" => "Index merge",
        "" => return String::new(),
        other => return format!("Access ({other})"),
    };
    op.to_owned()
}

/// One table access.
fn table(t: &Value) -> PlanNode {
    let name = text(&t["table_name"]);
    let key = text(&t["key"]);
    let mut n = PlanNode::op(access_operation(t));
    if n.operation.is_empty() {
        n.operation = "Table".into();
    }
    n.object = match (&name, &key) {
        (Some(t), Some(k)) if n.operation != "Table scan" => Some(format!("{t}.{k}")),
        (Some(t), _) => Some(t.clone()),
        (None, _) => None,
    };
    // MySQL: rows read per scan, and the share kept by the attached condition.
    let read = num(&t["rows_examined_per_scan"]).or(num(&t["rows"]));
    let filtered = num(&t["filtered"]).map(|f| f / 100.0);
    n.estimated_rows = read.map(|r| r * filtered.unwrap_or(1.0));
    if let Some(r) = read {
        n.details.insert("Estimated Rows Read".into(), fmt(r));
    }
    if let Some(p) = num(&t["rows_produced_per_join"]) {
        n.details.insert("Rows Produced per Join".into(), fmt(p));
    }
    n.cost = match (
        num(&t["cost_info"]["read_cost"]),
        num(&t["cost_info"]["eval_cost"]),
    ) {
        (Some(r), Some(e)) => Some(r + e),
        _ => num(&t["cost_info"]["prefix_cost"]).or(num(&t["cost"])),
    };
    // MariaDB ANALYZE: rows read per loop, share kept, loops and time.
    if let Some(r_rows) = num(&t["r_rows"]) {
        let kept = num(&t["r_filtered"]).map_or(1.0, |f| f / 100.0);
        let loops = num(&t["r_loops"]).unwrap_or(1.0);
        n.loops = Some(loops);
        n.actual_rows = Some(r_rows * kept);
        n.details
            .insert("Actual Rows Read".into(), fmt(r_rows * loops));
        if kept < 1.0 {
            n.details
                .insert("Rows Removed by Filter".into(), fmt(r_rows * (1.0 - kept)));
        }
        n.total_time_ms = match (num(&t["r_table_time_ms"]), num(&t["r_other_time_ms"])) {
            (Some(a), b) => Some(a + b.unwrap_or(0.0)),
            (None, _) => num(&t["r_total_time_ms"]),
        };
        if loops == 0.0 {
            n.warnings.push("Never executed".into());
        }
    }
    for (k, label) in [
        ("access_type", "Access Type"),
        ("possible_keys", "Possible Keys"),
        ("key", "Key"),
        ("used_key_parts", "Used Key Parts"),
        ("filtered", "Filtered"),
        ("using_join_buffer", "Join Buffer"),
        ("using_MRR", "MRR"),
    ] {
        if let Some(v) = text(&t[k]) {
            n.details.insert(label.into(), v);
        }
    }
    // Index lookups: `key_part = ref`.
    if let (Some(parts), Some(refs)) = (t["used_key_parts"].as_array(), t["ref"].as_array()) {
        let cond: Vec<String> = parts
            .iter()
            .zip(refs)
            .filter_map(|(p, r)| Some(format!("{} = {}", p.as_str()?, r.as_str()?)))
            .collect();
        if !cond.is_empty() {
            n.predicates.push(Predicate {
                kind: "Index Cond".into(),
                text: cond.join(" AND "),
            });
        }
    }
    for (k, kind) in [
        ("index_condition", "Index Cond"),
        ("attached_condition", "Filter"),
    ] {
        if let Some(c) = text(&t[k]) {
            n.predicates.push(Predicate {
                kind: kind.into(),
                text: c,
            });
        }
    }
    // A derived table or view materialized from a sub-query reads it first.
    for key in ["materialized_from_subquery", "materialized"] {
        if let Some(q) = t[key].get("query_block") {
            let mut m = PlanNode::op("Materialize");
            m.children.push(query_block(q));
            m.cost = m.children[0].cost;
            n.children.push(m);
        }
    }
    for key in SUBQUERY_KEYS {
        n.children.extend(subqueries(&t[*key], key));
    }
    // DML: the write sits on top of the access that finds its rows.
    for dml in ["delete", "update", "insert", "replace"] {
        if t[dml] == true {
            let mut w = PlanNode::op(capitalize(dml));
            w.object = name.clone();
            w.cost = n.cost;
            if !n.operation.is_empty() && n.operation != "Table" {
                w.children.push(n);
            }
            return w;
        }
    }
    n
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn fmt(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

/// `nested_loop: [t1, t2, t3]` → `Nested Loop(Nested Loop(t1, t2), t3)`.
fn nested_loop(items: &[Value]) -> Option<PlanNode> {
    let mut steps = items.iter().map(|i| {
        let mut nodes = content(i);
        let hash = i["table"]["using_join_buffer"]
            .as_str()
            .is_some_and(|b| b.contains("hash"));
        let prefix = num(&i["table"]["cost_info"]["prefix_cost"]);
        let produced = num(&i["table"]["rows_produced_per_join"]);
        let first = if nodes.is_empty() {
            PlanNode::op("?")
        } else {
            let mut f = nodes.remove(0);
            f.children.extend(nodes);
            f
        };
        (first, hash, prefix, produced)
    });
    let (mut acc, _, _, _) = steps.next()?;
    for (inner, hash, prefix, produced) in steps {
        let mut j = PlanNode::op(if hash { "Hash Join" } else { "Nested Loop" });
        let kids = acc.cost.unwrap_or(0.0) + inner.cost.unwrap_or(0.0);
        j.cost = prefix.or((kids > 0.0).then_some(kids));
        j.estimated_rows =
            produced.or_else(|| Some(acc.estimated_rows? * inner.estimated_rows?.max(1.0)));
        let times: Vec<f64> = [&acc, &inner]
            .iter()
            .filter_map(|c| c.total_time_ms)
            .collect();
        if !times.is_empty() {
            j.total_time_ms = Some(times.iter().sum());
        }
        j.children = vec![acc, inner];
        acc = j;
    }
    Some(acc)
}

/// Parse MySQL's `EXPLAIN ANALYZE` tree for `sql`.
///
/// ```text
/// -> Limit: 10 row(s)  (cost=… rows=10) (actual time=2.45..2.45 rows=10 loops=1)
///     -> Table scan on o  (cost=101 rows=1000) (actual time=0.05..0.28 rows=1000 loops=1)
/// ```
pub fn parse_tree(tree: &str, sql: &str) -> Result<Plan> {
    // Join continuation lines (a condition with a newline in a literal) to their node.
    let mut lines: Vec<(usize, String)> = Vec::new();
    for line in tree.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("-> ") {
            lines.push((line.len() - trimmed.len(), rest.to_owned()));
        } else if let Some(last) = lines.last_mut()
            && !trimmed.is_empty()
        {
            last.1.push('\n');
            last.1.push_str(line);
        }
    }
    if lines.is_empty() {
        return Err(PlanError::Parse("EXPLAIN ANALYZE returned no plan".into()));
    }
    if lines.len() == 1 && lines[0].1.starts_with("<not executable") {
        return Err(PlanError::Unsupported(
            "MySQL cannot measure this statement (EXPLAIN ANALYZE: not executable by iterator \
             executor); use Explain for its estimated plan"
                .into(),
        ));
    }
    // A stack of (indent, node); a shallower line closes the deeper ones.
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    for (indent, raw) in lines {
        let node = tree_node(&raw);
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            close(&mut stack, &mut roots);
        }
        stack.push((indent, node));
    }
    while !stack.is_empty() {
        close(&mut stack, &mut roots);
    }
    let mut root = if roots.len() == 1 {
        roots.remove(0)
    } else {
        let mut r = PlanNode::op("Query");
        r.children = roots;
        r
    };
    fill(&mut root);
    let mut plan = Plan::new(PlanSource::MySql, PlanKind::Actual, sql, root);
    plan.execution_ms = plan.root.total_time_ms;
    Ok(plan)
}

fn close(stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>) {
    if let Some((_, n)) = stack.pop() {
        match stack.last_mut() {
            Some((_, parent)) => parent.children.push(n),
            None => roots.push(n),
        }
    }
}

/// After the tree is built: nodes without timing (`Hash`) take their children's, and a
/// `Filter` over one input records how many rows it removed (per loop).
fn fill(n: &mut PlanNode) {
    for c in &mut n.children {
        fill(c);
    }
    if n.total_time_ms.is_none() && n.loops != Some(0.0) {
        let times: Vec<f64> = n.children.iter().filter_map(|c| c.total_time_ms).collect();
        if !times.is_empty() {
            n.total_time_ms = Some(times.iter().sum());
        }
    }
    if n.operation == "Filter"
        && n.children.len() == 1
        && let (Some(input), Some(out)) = (n.children[0].actual_rows_total(), n.actual_rows_total())
    {
        let loops = n.loops.unwrap_or(1.0).max(1.0);
        let removed = ((input - out) / loops).max(0.0);
        n.details
            .insert("Rows Removed by Filter".into(), fmt(removed));
    }
}

/// Numbers after `key=` in a stats group: `rows=10`, `loops=798`, `963e-6`.
fn stat(group: &str, key: &str) -> Option<f64> {
    let at = group.find(&format!("{key}="))? + key.len() + 1;
    let v: String = group[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '-' | '+'))
        .collect();
    v.parse().ok()
}

/// The end of a `a..b` range (or the single value).
fn range_end(group: &str, key: &str) -> Option<f64> {
    let at = group.find(&format!("{key}="))? + key.len() + 1;
    let v: String = group[at..]
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ')')
        .collect();
    v.rsplit("..").next()?.parse().ok()
}

/// One tree line: description, `(cost=… rows=…)`, `(actual time=a..b rows=… loops=…)`.
fn tree_node(raw: &str) -> PlanNode {
    let mut desc = raw.trim_end();
    let mut actual = None;
    let mut never = false;
    if let Some(i) = desc.rfind("(actual time=") {
        actual = Some(&desc[i..]);
        desc = desc[..i].trim_end();
    } else if let Some(i) = desc.rfind("(never executed)") {
        never = true;
        desc = desc[..i].trim_end();
    }
    let mut cost = None;
    if desc.ends_with(')')
        && let Some(i) = desc.rfind("(cost=")
    {
        cost = Some(&desc[i..]);
        desc = desc[..i].trim_end();
    }
    let mut n = describe(desc);
    if let Some(c) = cost {
        n.cost = range_end(c, "cost");
        n.estimated_rows = stat(c, "rows");
    }
    if let Some(a) = actual {
        n.loops = stat(a, "loops");
        n.actual_rows = stat(a, "rows");
        if let Some(last) = range_end(a, "time") {
            n.total_time_ms = Some(last * n.loops.unwrap_or(1.0));
        }
    }
    if never {
        n.loops = Some(0.0);
        n.actual_rows = Some(0.0);
        n.warnings.push("Never executed".into());
    }
    n
}

/// Operation, object and conditions from a node description.
fn describe(desc: &str) -> PlanNode {
    let mut n = PlanNode::op(desc);
    n.details.insert("Description".into(), desc.to_owned());
    let lower = desc.to_ascii_lowercase();
    if lower.contains("using temporary table") || lower.starts_with("temporary table") {
        n.warnings.push(warn::TEMP_TABLE.into());
    }
    // Joins, named like PostgreSQL's.
    for (prefix, op) in [
        ("Nested loop inner join", "Nested Loop"),
        ("Nested loop left join", "Nested Loop Left Join"),
        ("Nested loop antijoin", "Nested Loop Anti Join"),
        ("Nested loop semijoin", "Nested Loop Semi Join"),
        ("Nested loop full outer join", "Nested Loop Full Join"),
        ("Inner hash join", "Hash Join"),
        ("Left hash join", "Hash Left Join"),
        ("Anti hash join", "Hash Anti Join"),
        ("Semi hash join", "Hash Semi Join"),
        ("Full hash join", "Hash Full Join"),
    ] {
        if let Some(rest) = desc.strip_prefix(prefix) {
            n.operation = op.into();
            let rest = rest.trim();
            if rest.starts_with('(') {
                n.predicates.push(Predicate {
                    kind: "Hash Cond".into(),
                    text: rest.to_owned(),
                });
            } else if !rest.is_empty() {
                n.details.insert("Info".into(), rest.to_owned());
            }
            return n;
        }
    }
    // `Table scan on o`, `Index lookup on c using PRIMARY (id=o.customer_id)`,
    // `Index range scan on a using PRIMARY over (id < 50)`.
    if let Some(at) = desc.find(" on ")
        && !desc[..at].contains(':')
        && !desc[..at].contains('(')
    {
        n.operation = desc[..at].to_owned();
        let rest = &desc[at + 4..];
        // `<temporary>`, `<union temporary>`: internal tables, possibly with a space.
        let (table, mut tail) = match rest.find('>') {
            Some(end) if rest.starts_with('<') => (&rest[..=end], rest[end + 1..].trim_start()),
            _ => split_word(rest),
        };
        let mut object = table.to_owned();
        if let Some(after) = tail.strip_prefix("using ") {
            let (index, t) = split_word(after);
            object = format!("{table}.{index}");
            tail = t;
        }
        n.object = Some(object);
        let tail = tail.trim();
        if let Some(over) = tail.strip_prefix("over ") {
            n.predicates.push(Predicate {
                kind: "Index Range".into(),
                text: over.to_owned(),
            });
        } else if tail.starts_with('(') {
            n.predicates.push(Predicate {
                kind: "Index Cond".into(),
                text: tail.to_owned(),
            });
        } else if !tail.is_empty() {
            n.details.insert("Info".into(), tail.to_owned());
        }
        return n;
    }
    // `Filter: (cond)`, `Sort: t DESC`, `Limit: 10 row(s)`, `Aggregate: count(0)`.
    if let Some((name, rest)) = desc.split_once(": ") {
        n.operation = match name {
            "Limit/Offset" => "Limit".into(),
            other => other.to_owned(),
        };
        match name {
            "Filter" => n.predicates.push(Predicate {
                kind: "Filter".into(),
                text: rest.to_owned(),
            }),
            "Sort" => {
                n.details.insert("Sort Key".into(), rest.to_owned());
            }
            _ => {
                n.details.insert("Info".into(), rest.to_owned());
            }
        }
    }
    if n.operation == "Sort" {
        n.warnings.push(format!("{} (filesort)", warn::SORT));
    }
    if n.operation.starts_with("Aggregate using temporary table") {
        n.operation = "Aggregate".into();
    }
    n
}

/// The first word (to a space) and the rest.
fn split_word(s: &str) -> (&str, &str) {
    match s.find(' ') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_lines_parse_into_nodes() {
        let n = tree_node(
            "Index range scan on a using PRIMARY over (id < 50)  (cost=0.35..10.1 rows=49) \
             (actual time=963e-6..0.0282 rows=49 loops=2)",
        );
        assert_eq!(n.operation, "Index range scan");
        assert_eq!(n.object.as_deref(), Some("a.PRIMARY"));
        assert_eq!(n.cost, Some(10.1));
        assert_eq!(n.estimated_rows, Some(49.0));
        assert_eq!(n.loops, Some(2.0));
        assert_eq!(n.total_time_ms, Some(0.0564));
        assert_eq!(n.predicates[0].text, "(id < 50)");

        let f = tree_node("Filter: (o.total > 100.00)  (never executed)");
        assert_eq!(f.operation, "Filter");
        assert_eq!(f.loops, Some(0.0));
        assert_eq!(f.predicates[0].text, "(o.total > 100.00)");

        let j = tree_node("Inner hash join (b.k = a.k)  (cost=99455 rows=99421)");
        assert_eq!(j.operation, "Hash Join");
        assert_eq!(j.predicates[0].kind, "Hash Cond");
        assert_eq!(j.actual_rows, None);

        let s = tree_node("Sort: t DESC, limit input to 10 row(s) per chunk");
        assert_eq!(s.operation, "Sort");
        assert!(s.warnings[0].starts_with(warn::SORT));
    }

    #[test]
    fn not_executable_is_unsupported() {
        let e = parse_tree("-> <not executable by iterator executor>\n", "delete").unwrap_err();
        assert!(matches!(e, PlanError::Unsupported(_)), "{e}");
        assert!(parse_tree("", "x").is_err());
    }

    #[test]
    fn mariadb_analyze_json() {
        let json = r#"{"query_block": {"select_id": 1, "r_loops": 1, "r_total_time_ms": 2.5,
            "filesort": {"sort_key": "t1.b", "r_loops": 1, "r_total_time_ms": 0.4,
              "r_output_rows": 10, "temporary_table": {"nested_loop": [
                {"table": {"table_name": "t1", "access_type": "ALL", "r_loops": 1,
                  "rows": 1000, "r_rows": 1000, "filtered": 100, "r_filtered": 10,
                  "r_table_time_ms": 1.0, "r_other_time_ms": 0.5,
                  "attached_condition": "t1.a < 10"}},
                {"block-nl-join": {"table": {"table_name": "t2", "access_type": "ALL",
                  "r_loops": 1, "rows": 50, "r_rows": 50, "filtered": 100, "r_filtered": 100,
                  "r_table_time_ms": 0.1}, "join_type": "BNL",
                  "attached_condition": "t2.a = t1.a"}}]}}}}"#;
        let plan = parse_json(json, "select").unwrap();
        assert_eq!(plan.kind, PlanKind::Actual);
        assert_eq!(plan.execution_ms, Some(2.5));
        let ops: Vec<&str> = plan.nodes().iter().map(|n| n.operation.as_str()).collect();
        assert_eq!(
            ops,
            [
                "Sort",
                "Temporary Table",
                "Nested Loop",
                "Table scan",
                "Table scan"
            ]
        );
        let t1 = plan.node(3).unwrap();
        assert_eq!(t1.actual_rows, Some(100.0));
        assert_eq!(t1.total_time_ms, Some(1.5));
        assert_eq!(
            t1.details.get("Rows Removed by Filter").map(String::as_str),
            Some("900")
        );
        assert_eq!(plan.node(4).unwrap().predicates[0].kind, "Join Filter");
    }
}
