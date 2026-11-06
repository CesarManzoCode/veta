//! The physical tree of a plan as the JSON object of contract §11.6.
//!
//! Each node carries its `op`, the `kind` of a join, `est_rows`, `actual_rows`, the base
//! `relations` of its subtree, the `predicates` applied in its subtree and its `children`.
//! A predicate is written over the base relations, so a column that a `Project` computes
//! appears as its expression and the text stands on its own in a `WHERE` over `relations`.
//!
//! Decisions within the contract, labelled as design:
//!
//! - The one row without columns of a `SELECT` without `FROM` is a `Scan` with no relations.
//! - The `table` of a Parquet relation is `read_parquet('path')`, what `FROM` takes.
//! - Without an execution (`EXPLAIN` without `ANALYZE`), `actual_rows` is `null`.
//! - A predicate keeps its outermost parentheses, as §11.6 asks ("todo con paréntesis
//!   explícitos") although its example omits them. An estimate that overflowed is printed
//!   as the largest finite number, since JSON has no infinity.

use std::fmt::Write;

use crate::display::write_string;
use crate::expr::BoundExpr;
use crate::plan::LogicalPlan;

/// `est_rows` and `actual_rows` hold one value per node of `plan` in preorder (a node, then
/// its children left to right), as [`crate::Estimator::estimate_tree`] gives them.
pub fn explain_json(plan: &LogicalPlan, est_rows: &[f64], actual_rows: Option<&[u64]>) -> String {
    let mut writer = Writer {
        est_rows,
        actual_rows,
        next: 0,
    };
    writer.node(plan).json
}

struct Writer<'a> {
    est_rows: &'a [f64],
    actual_rows: Option<&'a [u64]>,
    next: usize,
}

/// One node written, with what its parent adds to its own lists.
struct Written {
    json: String,
    relations: Vec<(String, String)>,
    predicates: Vec<String>,
}

fn physical_op(plan: &LogicalPlan) -> &'static str {
    match plan {
        LogicalPlan::Scan { .. } | LogicalPlan::SingleRow => "Scan",
        LogicalPlan::ParquetScan { .. } => "ParquetScan",
        LogicalPlan::Filter { .. } => "Filter",
        LogicalPlan::Project { .. } => "Project",
        LogicalPlan::Aggregate { .. } => "HashAggregate",
        LogicalPlan::Join { keys, .. } => {
            if keys.is_empty() {
                "NestedLoopJoin"
            } else {
                "HashJoin"
            }
        }
        LogicalPlan::Sort { .. } => "Sort",
        LogicalPlan::TopN { .. } => "TopN",
        LogicalPlan::Limit { .. } => "Limit",
    }
}

/// `(alias, table)` when the node reads a base relation.
fn base_relation(plan: &LogicalPlan) -> Option<(String, String)> {
    match plan {
        LogicalPlan::Scan { table, alias, .. } => Some((alias.clone(), table.clone())),
        LogicalPlan::ParquetScan { path, alias, .. } => {
            let mut table = String::from("read_parquet(");
            let _ = write_string(&mut table, path);
            table.push(')');
            Some((alias.clone(), table))
        }
        _ => None,
    }
}

fn conjuncts_sql(predicate: &BoundExpr, columns: &[String]) -> Vec<String> {
    predicate
        .clone()
        .into_conjuncts()
        .iter()
        .map(|c| c.to_sql_over(columns))
        .collect()
}

/// The predicates the node itself applies.
fn own_predicates(plan: &LogicalPlan) -> Vec<String> {
    match plan {
        LogicalPlan::Filter { input, predicate } => conjuncts_sql(predicate, &input.output_sql()),
        LogicalPlan::Join {
            left,
            right,
            keys,
            residual,
            ..
        } => {
            let mut columns = left.output_sql();
            let right_columns = right.output_sql();
            let mut predicates: Vec<String> = keys
                .iter()
                .map(|(l, r)| {
                    format!(
                        "({} = {})",
                        l.to_sql_over(&columns),
                        r.to_sql_over(&right_columns)
                    )
                })
                .collect();
            if let Some(residual) = residual {
                columns.extend(right_columns);
                predicates.extend(conjuncts_sql(residual, &columns));
            }
            predicates
        }
        _ => Vec::new(),
    }
}

/// `text` as a JSON string.
fn write_json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_estimate(out: &mut String, estimate: f64) {
    let finite = if estimate.is_nan() {
        0.0
    } else {
        estimate.clamp(-f64::MAX, f64::MAX)
    };
    let _ = write!(out, "{finite:?}");
}

impl Writer<'_> {
    fn node(&mut self, plan: &LogicalPlan) -> Written {
        let slot = self.next;
        self.next += 1;
        let children: Vec<Written> = plan.children().into_iter().map(|c| self.node(c)).collect();

        let mut relations: Vec<(String, String)> = base_relation(plan).into_iter().collect();
        let mut predicates = own_predicates(plan);
        for child in &children {
            relations.extend(child.relations.iter().cloned());
            predicates.extend(child.predicates.iter().cloned());
        }

        let mut json = String::from("{\"op\":");
        write_json_string(&mut json, physical_op(plan));
        if let LogicalPlan::Join { kind, .. } = plan {
            json.push_str(",\"kind\":");
            write_json_string(&mut json, kind.name());
        }
        json.push_str(",\"est_rows\":");
        write_estimate(&mut json, self.est_rows.get(slot).copied().unwrap_or(0.0));
        json.push_str(",\"actual_rows\":");
        match self.actual_rows.and_then(|rows| rows.get(slot)) {
            Some(rows) => {
                let _ = write!(json, "{rows}");
            }
            None => json.push_str("null"),
        }
        json.push_str(",\"relations\":[");
        for (i, (alias, table)) in relations.iter().enumerate() {
            json.push_str(if i > 0 { ",{\"alias\":" } else { "{\"alias\":" });
            write_json_string(&mut json, alias);
            json.push_str(",\"table\":");
            write_json_string(&mut json, table);
            json.push('}');
        }
        json.push_str("],\"predicates\":[");
        for (i, predicate) in predicates.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            write_json_string(&mut json, predicate);
        }
        json.push_str("],\"children\":[");
        for (i, child) in children.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&child.json);
        }
        json.push_str("]}");
        Written {
            json,
            relations,
            predicates,
        }
    }
}

#[cfg(test)]
mod tests {
    use veta_core::{DataType, Field, Schema};

    use super::*;
    use crate::ast::Statement;
    use crate::binder::bind_query;
    use crate::catalog::MemoryCatalog;
    use crate::optimizer::{OptimizerOptions, optimize};
    use crate::parser::{parse_expr, parse_statement};

    #[derive(Debug, Clone, PartialEq)]
    enum Json {
        Null,
        Number(f64),
        Text(String),
        Array(Vec<Json>),
        Object(Vec<(String, Json)>),
    }

    impl Json {
        fn get(&self, key: &str) -> Option<&Json> {
            match self {
                Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        fn items(&self, key: &str) -> &[Json] {
            match self.get(key) {
                Some(Json::Array(items)) => items,
                other => panic!("{key} is not an array: {other:?}"),
            }
        }

        fn text(&self) -> &str {
            match self {
                Json::Text(text) => text,
                other => panic!("not a string: {other:?}"),
            }
        }
    }

    /// A strict reader of what [`explain_json`] may print.
    struct Reader<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl Reader<'_> {
        fn parse(text: &str) -> Json {
            let mut reader = Reader {
                bytes: text.as_bytes(),
                pos: 0,
            };
            let value = reader.value();
            assert_eq!(reader.pos, text.len(), "text after the object: {text}");
            value
        }

        fn eat(&mut self, byte: u8) -> bool {
            let found = self.bytes.get(self.pos) == Some(&byte);
            if found {
                self.pos += 1;
            }
            found
        }

        fn expect(&mut self, byte: u8) {
            assert!(self.eat(byte), "expected {} at {}", byte as char, self.pos);
        }

        fn value(&mut self) -> Json {
            match self.bytes.get(self.pos) {
                Some(b'{') => self.object(),
                Some(b'[') => self.array(),
                Some(b'"') => Json::Text(self.string()),
                Some(b'n') => {
                    assert_eq!(&self.bytes[self.pos..self.pos + 4], b"null");
                    self.pos += 4;
                    Json::Null
                }
                _ => self.number(),
            }
        }

        fn object(&mut self) -> Json {
            self.expect(b'{');
            let mut fields = Vec::new();
            if !self.eat(b'}') {
                loop {
                    let key = self.string();
                    self.expect(b':');
                    fields.push((key, self.value()));
                    if !self.eat(b',') {
                        break;
                    }
                }
                self.expect(b'}');
            }
            Json::Object(fields)
        }

        fn array(&mut self) -> Json {
            self.expect(b'[');
            let mut items = Vec::new();
            if !self.eat(b']') {
                loop {
                    items.push(self.value());
                    if !self.eat(b',') {
                        break;
                    }
                }
                self.expect(b']');
            }
            Json::Array(items)
        }

        fn string(&mut self) -> String {
            self.expect(b'"');
            let mut out = Vec::new();
            loop {
                let byte = self.bytes[self.pos];
                self.pos += 1;
                match byte {
                    b'"' => return String::from_utf8(out).unwrap(),
                    b'\\' => {
                        let escaped = self.bytes[self.pos];
                        self.pos += 1;
                        out.push(match escaped {
                            b'"' => b'"',
                            b'\\' => b'\\',
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            other => panic!("unknown escape {}", other as char),
                        });
                    }
                    other => {
                        assert!(other >= 0x20, "raw control byte in a string");
                        out.push(other);
                    }
                }
            }
        }

        fn number(&mut self) -> Json {
            let start = self.pos;
            while self
                .bytes
                .get(self.pos)
                .is_some_and(|b| b.is_ascii_digit() || b"+-.eE".contains(b))
            {
                self.pos += 1;
            }
            let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
            Json::Number(text.parse().unwrap_or_else(|_| panic!("number {text:?}")))
        }
    }

    fn catalog() -> MemoryCatalog {
        use DataType::*;
        let fields = |cols: &[(&str, DataType)]| {
            Schema::new(
                cols.iter()
                    .map(|(n, t)| Field::new(*n, *t, false))
                    .collect(),
            )
        };
        let mut c = MemoryCatalog::new();
        c.add_table(
            "orders",
            fields(&[
                ("o_orderkey", Int64),
                ("o_custkey", Int64),
                ("o_orderdate", Date),
                ("o_stamp", Timestamp),
                ("o_comment", Varchar),
            ]),
            vec![0],
        );
        c.add_table(
            "lineitem",
            fields(&[
                ("l_orderkey", Int64),
                (
                    "l_price",
                    Decimal {
                        precision: 15,
                        scale: 2,
                    },
                ),
                ("l_note", Varchar),
            ]),
            vec![0],
        );
        c.add_parquet("/data/o'x.parquet", fields(&[("u", Int64)]));
        c
    }

    fn plan_of(sql: &str) -> LogicalPlan {
        let Statement::Query(query) = parse_statement(sql).unwrap() else {
            panic!("not a query: {sql}");
        };
        let catalog = catalog();
        let bound = bind_query(&catalog, &query).unwrap_or_else(|e| panic!("{sql}: {e}"));
        optimize(bound, OptimizerOptions::default(), &catalog)
    }

    fn node_count(plan: &LogicalPlan) -> usize {
        1 + plan.children().into_iter().map(node_count).sum::<usize>()
    }

    fn check_node(node: &Json) {
        let op = node.get("op").unwrap().text();
        assert!(
            [
                "Scan",
                "ParquetScan",
                "Filter",
                "Project",
                "HashAggregate",
                "HashJoin",
                "NestedLoopJoin",
                "Sort",
                "TopN",
                "Limit"
            ]
            .contains(&op),
            "{op}"
        );
        assert_eq!(node.get("kind").is_some(), op.ends_with("Join"), "{op}");
        assert!(matches!(node.get("est_rows"), Some(Json::Number(_))));
        for relation in node.items("relations") {
            assert!(!relation.get("alias").unwrap().text().is_empty());
            assert!(!relation.get("table").unwrap().text().is_empty());
        }
        for predicate in node.items("predicates") {
            let sql = predicate.text();
            parse_expr(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        node.items("children").iter().for_each(check_node);
    }

    fn explained(sql: &str) -> Json {
        let plan = plan_of(sql);
        let est = vec![1.5; node_count(&plan)];
        let actual = vec![7; node_count(&plan)];
        let json = Reader::parse(&explain_json(&plan, &est, Some(&actual)));
        check_node(&json);
        json
    }

    fn strings(items: &[Json]) -> Vec<String> {
        items.iter().map(|i| i.text().to_string()).collect()
    }

    /// `SELECT count(*) FROM <relations> WHERE <predicates>` of a node, as the harness builds
    /// it for the cardinality check (§17).
    fn count_sql(node: &Json) -> String {
        let from: Vec<String> = node
            .items("relations")
            .iter()
            .map(|r| {
                format!(
                    "{} AS {}",
                    r.get("table").unwrap().text(),
                    r.get("alias").unwrap().text()
                )
            })
            .collect();
        let predicates = strings(node.items("predicates"));
        let mut sql = format!("SELECT count(*) FROM {}", from.join(", "));
        if !predicates.is_empty() {
            sql.push_str(&format!(" WHERE {}", predicates.join(" AND ")));
        }
        sql
    }

    fn assert_binds(sql: &str) {
        let Statement::Query(query) = parse_statement(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
        else {
            panic!("not a query: {sql}");
        };
        bind_query(&catalog(), &query).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    #[test]
    fn the_json_parses_and_its_predicates_parse_again() {
        let json = explained(
            "SELECT o.o_custkey, count(*) AS c FROM orders AS o, lineitem AS l \
             WHERE o.o_orderkey = l.l_orderkey \
             AND o.o_orderdate < DATE '1995-03-15' \
             AND o.o_stamp >= CAST('1995-03-15 01:02:03' AS TIMESTAMP) \
             AND o.o_comment = 'it''s \"quoted\"' \
             AND l.l_note LIKE '%x%' \
             AND l.l_price BETWEEN 1.50 AND 2.50 \
             AND o.o_custkey IN (1, 2, 3) \
             AND extract(year FROM o.o_orderdate) = 1995 \
             AND (o.o_custkey > l.l_orderkey OR l.l_note IS NULL) \
             GROUP BY o.o_custkey ORDER BY c DESC LIMIT 5",
        );
        assert_eq!(json.get("actual_rows"), Some(&Json::Number(7.0)));
        let predicates = strings(json.items("predicates"));
        // The cost-based reorder picks the sides, so the join key may come either way round.
        assert!(
            predicates
                .iter()
                .any(|p| p == "(o.o_orderkey = l.l_orderkey)"
                    || p == "(l.l_orderkey = o.o_orderkey)"),
            "join key not in {predicates:?}"
        );
        for expected in [
            "(o.o_orderdate < CAST('1995-03-15' AS DATE))",
            "(o.o_stamp >= CAST('1995-03-15 01:02:03' AS TIMESTAMP))",
            "(o.o_comment = 'it''s \"quoted\"')",
        ] {
            assert!(
                predicates.iter().any(|p| p == expected),
                "{expected} not in {predicates:?}"
            );
        }
        let relations: Vec<(String, String)> = json
            .items("relations")
            .iter()
            .map(|r| {
                (
                    r.get("alias").unwrap().text().to_string(),
                    r.get("table").unwrap().text().to_string(),
                )
            })
            .collect();
        assert_eq!(relations.len(), 2);
        assert!(relations.contains(&("o".into(), "orders".into())));
        assert!(relations.contains(&("l".into(), "lineitem".into())));
    }

    /// The first node of the kind, top-down and left to right.
    fn find<'j>(node: &'j Json, op: &str) -> Option<&'j Json> {
        if node.get("op").unwrap().text() == op {
            return Some(node);
        }
        node.items("children").iter().find_map(|c| find(c, op))
    }

    #[test]
    fn a_join_node_gives_a_count_query_over_its_relations() {
        let json = explained(
            "SELECT o.o_custkey FROM orders AS o, lineitem AS l \
             WHERE o.o_orderkey = l.l_orderkey AND o.o_orderdate < DATE '1995-03-15'",
        );
        let join = find(&json, "HashJoin").unwrap();
        assert_eq!(join.get("kind").unwrap().text(), "inner");
        assert_binds(&count_sql(join));
    }

    #[test]
    fn a_computed_column_is_written_over_the_base_relations() {
        let json = explained(
            "SELECT * FROM (SELECT o_orderkey + 1 AS k FROM orders) AS s, lineitem AS l \
             WHERE s.k = l.l_orderkey",
        );
        let join = find(&json, "HashJoin").unwrap();
        let predicates = strings(join.items("predicates"));
        assert!(
            predicates
                .iter()
                .any(|p| p.contains("(orders.o_orderkey + 1)") && p.contains("l.l_orderkey")),
            "{predicates:?}"
        );
        assert!(
            !predicates.iter().any(|p| p.contains("s.k")),
            "{predicates:?}"
        );
        assert_binds(&count_sql(join));
    }

    #[test]
    fn values_follow_the_preorder_of_the_plan() {
        let plan = plan_of(
            "SELECT o.o_custkey FROM orders AS o, lineitem AS l WHERE o.o_orderkey = l.l_orderkey",
        );
        let n = node_count(&plan);
        let est: Vec<f64> = (0..n).map(|i| i as f64 + 0.5).collect();
        let actual: Vec<u64> = (0..n).map(|i| i as u64 * 10).collect();
        let json = Reader::parse(&explain_json(&plan, &est, Some(&actual)));
        fn walk(node: &Json, out: &mut Vec<(Json, Json)>) {
            out.push((
                node.get("est_rows").unwrap().clone(),
                node.get("actual_rows").unwrap().clone(),
            ));
            for child in node.items("children") {
                walk(child, out);
            }
        }
        let mut seen = Vec::new();
        walk(&json, &mut seen);
        assert_eq!(seen.len(), n);
        for (i, (e, a)) in seen.iter().enumerate() {
            assert_eq!(e, &Json::Number(i as f64 + 0.5));
            assert_eq!(a, &Json::Number(i as f64 * 10.0));
        }
    }

    #[test]
    fn without_an_execution_actual_rows_is_null() {
        let plan = plan_of("SELECT o_custkey FROM orders");
        let json = Reader::parse(&explain_json(&plan, &[1.0, 2.0], None));
        assert_eq!(json.get("actual_rows"), Some(&Json::Null));
        assert_eq!(
            json.items("children")[0].get("actual_rows"),
            Some(&Json::Null)
        );
    }

    #[test]
    fn parquet_relations_and_rows_without_from_stay_in_the_list_of_ops() {
        let json = explained("SELECT u FROM read_parquet('/data/o''x.parquet') AS p WHERE u > 1");
        let scan = find(&json, "ParquetScan").unwrap();
        assert_eq!(
            scan.items("relations")[0].get("table").unwrap().text(),
            "read_parquet('/data/o''x.parquet')"
        );
        assert_binds(&count_sql(find(&json, "Filter").unwrap()));

        let json = explained("SELECT 1 + 1");
        let scan = find(&json, "Scan").unwrap();
        assert!(scan.items("relations").is_empty());
    }

    #[test]
    fn join_ops_follow_the_keys() {
        let json = explained("SELECT * FROM orders AS o, lineitem AS l");
        let join = find(&json, "NestedLoopJoin").unwrap();
        assert_eq!(join.get("kind").unwrap().text(), "cross");
        let json = explained(
            "SELECT o.o_custkey, l.l_price FROM orders AS o LEFT JOIN lineitem AS l \
             ON o.o_orderkey = l.l_orderkey",
        );
        let join = find(&json, "HashJoin").unwrap();
        assert_eq!(join.get("kind").unwrap().text(), "left");
    }

    #[test]
    fn estimates_are_always_json_numbers() {
        let mut out = String::new();
        write_estimate(&mut out, f64::INFINITY);
        assert_eq!(Reader::parse(&out), Json::Number(f64::MAX));
        out.clear();
        write_estimate(&mut out, f64::NAN);
        assert_eq!(Reader::parse(&out), Json::Number(0.0));
        out.clear();
        write_estimate(&mut out, 1234.5);
        assert_eq!(out, "1234.5");
    }

    #[test]
    fn json_strings_escape_what_they_must() {
        let mut out = String::new();
        write_json_string(&mut out, "a\"b\\c\nd\te\u{1}");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\te\\u0001\"");
    }
}
