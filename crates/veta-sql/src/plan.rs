//! Logical plan nodes of contract §11.1 and the output schema of each one.
//!
//! Every expression of a node is bound over the output schema of its input (for joins,
//! over `left ++ right`; join keys over their own side). Besides the nodes of §11.1 there
//! is `SingleRow`, the one-row, zero-column input of a `SELECT` without `FROM` (§10.2).

use std::fmt::Write;
use std::ops::Bound;

use veta_core::predicate::ScanFilter;
use veta_core::{Field, Schema};

use crate::display::write_ident;
use crate::expr::{AggregateExpr, BoundExpr, value_to_sql};

/// Join kinds of §11.1. `Semi`, `Anti`, `NullAwareAnti` and `Single` come from the
/// unnesting of subqueries (§10.4); a left row matches a right row when every key pair is
/// equal (NULL never equals) and the residual is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    /// Left rows with at least one match, once each; output: the left columns.
    Semi,
    /// Left rows without any match; output: the left columns.
    Anti,
    /// `x NOT IN (subquery)` with the key `x = col`: a left row passes when the right
    /// input is empty, or when its `x` is not NULL, no right row has `col = x` and no right
    /// row has a NULL `col`. Output: the left columns.
    NullAwareAnti,
    /// Scalar subquery: every left row with the columns of its only match, or NULLs
    /// without a match; a left row with more than one match is an error. Without keys the
    /// right input is evaluated once.
    Single,
    Cross,
}

impl JoinKind {
    /// Name used by EXPLAIN ANALYZE (§11.6).
    pub fn name(self) -> &'static str {
        match self {
            JoinKind::Inner => "inner",
            JoinKind::Left => "left",
            JoinKind::Semi => "semi",
            JoinKind::Anti => "anti",
            JoinKind::NullAwareAnti => "null_aware_anti",
            JoinKind::Single => "single",
            JoinKind::Cross => "cross",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    pub expr: BoundExpr,
    pub desc: bool,
    /// Resolved NULL placement: last in ascending order and first in descending order
    /// unless `NULLS FIRST|LAST` says otherwise (§4).
    pub nulls_first: bool,
}

impl SortKey {
    pub fn to_sql(&self) -> String {
        format!(
            "{} {} NULLS {}",
            self.expr.to_sql(),
            if self.desc { "DESC" } else { "ASC" },
            if self.nulls_first { "FIRST" } else { "LAST" }
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    /// Reads `columns` (indices into `table_schema`) of a stored table. `filters` (over
    /// `table_schema` indices) only prune parts and granules (§11.2 rule 4, §11.4); the
    /// predicate they come from is still evaluated above the scan.
    Scan {
        table: String,
        /// Alias in the query, or the table name.
        alias: String,
        table_schema: Schema,
        columns: Vec<usize>,
        filters: Vec<ScanFilter>,
        schema: Schema,
    },
    /// Reads `columns` of `read_parquet('path')`; `filters` (over `file_schema` indices)
    /// skip row groups.
    ParquetScan {
        path: String,
        alias: String,
        file_schema: Schema,
        columns: Vec<usize>,
        filters: Vec<ScanFilter>,
        schema: Schema,
    },
    /// One row without columns.
    SingleRow,
    Filter {
        input: Box<LogicalPlan>,
        predicate: BoundExpr,
    },
    Project {
        input: Box<LogicalPlan>,
        exprs: Vec<BoundExpr>,
        schema: Schema,
    },
    /// Output: the group expressions, then the aggregates.
    Aggregate {
        input: Box<LogicalPlan>,
        groups: Vec<BoundExpr>,
        aggregates: Vec<AggregateExpr>,
        schema: Schema,
    },
    /// Keys are `(left expression, right expression)` pairs compared with `=`; the
    /// residual is bound over `left ++ right`.
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        kind: JoinKind,
        keys: Vec<(BoundExpr, BoundExpr)>,
        residual: Option<BoundExpr>,
        schema: Schema,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<SortKey>,
    },
    TopN {
        input: Box<LogicalPlan>,
        keys: Vec<SortKey>,
        limit: u64,
        offset: u64,
    },
    Limit {
        input: Box<LogicalPlan>,
        limit: Option<u64>,
        offset: u64,
    },
}

impl LogicalPlan {
    /// A scan of the whole table, every column in schema order.
    pub fn scan(table: &str, alias: &str, table_schema: Schema) -> LogicalPlan {
        LogicalPlan::Scan {
            table: table.to_string(),
            alias: alias.to_string(),
            columns: (0..table_schema.len()).collect(),
            filters: Vec::new(),
            schema: table_schema.clone(),
            table_schema,
        }
    }

    pub fn parquet_scan(path: &str, alias: &str, file_schema: Schema) -> LogicalPlan {
        LogicalPlan::ParquetScan {
            path: path.to_string(),
            alias: alias.to_string(),
            columns: (0..file_schema.len()).collect(),
            filters: Vec::new(),
            schema: file_schema.clone(),
            file_schema,
        }
    }

    pub fn filter(input: LogicalPlan, predicate: BoundExpr) -> LogicalPlan {
        LogicalPlan::Filter {
            input: Box::new(input),
            predicate,
        }
    }

    /// A projection whose output columns are named `names`.
    pub fn project(input: LogicalPlan, exprs: Vec<BoundExpr>, names: Vec<String>) -> LogicalPlan {
        let schema = Schema::new(
            exprs
                .iter()
                .zip(names)
                .map(|(e, name)| Field::new(name, e.data_type, e.nullable))
                .collect(),
        );
        LogicalPlan::Project {
            input: Box::new(input),
            exprs,
            schema,
        }
    }

    /// Group columns are named by their SQL text and aggregates by the call text.
    pub fn aggregate(
        input: LogicalPlan,
        groups: Vec<BoundExpr>,
        aggregates: Vec<AggregateExpr>,
    ) -> LogicalPlan {
        let mut fields: Vec<Field> = groups
            .iter()
            .map(|g| Field::new(g.to_sql(), g.data_type, g.nullable))
            .collect();
        fields.extend(
            aggregates
                .iter()
                .map(|a| Field::new(a.to_sql(), a.data_type, a.nullable)),
        );
        LogicalPlan::Aggregate {
            input: Box::new(input),
            groups,
            aggregates,
            schema: Schema::new(fields),
        }
    }

    pub fn join(
        left: LogicalPlan,
        right: LogicalPlan,
        kind: JoinKind,
        keys: Vec<(BoundExpr, BoundExpr)>,
        residual: Option<BoundExpr>,
    ) -> LogicalPlan {
        let mut fields = left.schema().fields;
        match kind {
            JoinKind::Inner | JoinKind::Cross => fields.extend(right.schema().fields),
            JoinKind::Left | JoinKind::Single => {
                fields.extend(right.schema().fields.into_iter().map(|mut f| {
                    f.nullable = true;
                    f
                }))
            }
            JoinKind::Semi | JoinKind::Anti | JoinKind::NullAwareAnti => {}
        }
        LogicalPlan::Join {
            left: Box::new(left),
            right: Box::new(right),
            kind,
            keys,
            residual,
            schema: Schema::new(fields),
        }
    }

    pub fn sort(input: LogicalPlan, keys: Vec<SortKey>) -> LogicalPlan {
        LogicalPlan::Sort {
            input: Box::new(input),
            keys,
        }
    }

    pub fn limit(input: LogicalPlan, limit: Option<u64>, offset: u64) -> LogicalPlan {
        LogicalPlan::Limit {
            input: Box::new(input),
            limit,
            offset,
        }
    }

    /// Output schema of the node.
    pub fn schema(&self) -> Schema {
        match self {
            LogicalPlan::Scan { schema, .. }
            | LogicalPlan::ParquetScan { schema, .. }
            | LogicalPlan::Project { schema, .. }
            | LogicalPlan::Aggregate { schema, .. }
            | LogicalPlan::Join { schema, .. } => schema.clone(),
            LogicalPlan::SingleRow => Schema::default(),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::TopN { input, .. }
            | LogicalPlan::Limit { input, .. } => input.schema(),
        }
    }

    pub fn children(&self) -> Vec<&LogicalPlan> {
        match self {
            LogicalPlan::Scan { .. } | LogicalPlan::ParquetScan { .. } | LogicalPlan::SingleRow => {
                vec![]
            }
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Project { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::TopN { input, .. }
            | LogicalPlan::Limit { input, .. } => vec![&**input],
            LogicalPlan::Join { left, right, .. } => vec![&**left, &**right],
        }
    }

    /// SQL text of each output column over the base relations below (§11.6): `alias.col` for
    /// a scanned column, the expression for a computed one.
    pub fn output_sql(&self) -> Vec<String> {
        match self {
            LogicalPlan::Scan { alias, schema, .. }
            | LogicalPlan::ParquetScan { alias, schema, .. } => {
                let mut qualifier = String::new();
                let _ = write_ident(&mut qualifier, alias);
                schema
                    .fields
                    .iter()
                    .map(|f| {
                        let mut column = format!("{qualifier}.");
                        let _ = write_ident(&mut column, &f.name);
                        column
                    })
                    .collect()
            }
            LogicalPlan::SingleRow => Vec::new(),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::TopN { input, .. }
            | LogicalPlan::Limit { input, .. } => input.output_sql(),
            LogicalPlan::Project { input, exprs, .. } => {
                let columns = input.output_sql();
                exprs.iter().map(|e| e.to_sql_over(&columns)).collect()
            }
            LogicalPlan::Aggregate {
                input,
                groups,
                aggregates,
                ..
            } => {
                let columns = input.output_sql();
                groups
                    .iter()
                    .map(|g| g.to_sql_over(&columns))
                    .chain(aggregates.iter().map(|a| a.to_sql_over(&columns)))
                    .collect()
            }
            LogicalPlan::Join {
                left, right, kind, ..
            } => {
                let mut columns = left.output_sql();
                if !matches!(
                    kind,
                    JoinKind::Semi | JoinKind::Anti | JoinKind::NullAwareAnti
                ) {
                    columns.extend(right.output_sql());
                }
                columns
            }
        }
    }

    /// Name of the node kind.
    pub fn op(&self) -> &'static str {
        match self {
            LogicalPlan::Scan { .. } => "Scan",
            LogicalPlan::ParquetScan { .. } => "ParquetScan",
            LogicalPlan::SingleRow => "SingleRow",
            LogicalPlan::Filter { .. } => "Filter",
            LogicalPlan::Project { .. } => "Project",
            LogicalPlan::Aggregate { .. } => "Aggregate",
            LogicalPlan::Join { .. } => "Join",
            LogicalPlan::Sort { .. } => "Sort",
            LogicalPlan::TopN { .. } => "TopN",
            LogicalPlan::Limit { .. } => "Limit",
        }
    }

    /// One line per node, children indented by two spaces.
    pub fn display_tree(&self) -> String {
        let mut out = String::new();
        self.write_tree(&mut out, 0);
        out
    }

    fn write_tree(&self, out: &mut String, depth: usize) {
        for _ in 0..depth {
            out.push_str("  ");
        }
        let list = |items: Vec<String>| items.join(", ");
        let _ = match self {
            LogicalPlan::Scan {
                table,
                alias,
                table_schema,
                columns,
                filters,
                ..
            } => {
                let _ = write!(out, "Scan {table} AS {alias}");
                write_scan_details(out, table_schema, columns, filters)
            }
            LogicalPlan::ParquetScan {
                path,
                alias,
                file_schema,
                columns,
                filters,
                ..
            } => {
                let _ = write!(out, "ParquetScan '{path}' AS {alias}");
                write_scan_details(out, file_schema, columns, filters)
            }
            LogicalPlan::SingleRow => write!(out, "SingleRow"),
            LogicalPlan::Filter { predicate, .. } => write!(out, "Filter {}", predicate.to_sql()),
            LogicalPlan::Project { exprs, .. } => write!(
                out,
                "Project {}",
                list(exprs.iter().map(BoundExpr::to_sql).collect())
            ),
            LogicalPlan::Aggregate {
                groups, aggregates, ..
            } => write!(
                out,
                "Aggregate groups=[{}] aggregates=[{}]",
                list(groups.iter().map(BoundExpr::to_sql).collect()),
                list(aggregates.iter().map(AggregateExpr::to_sql).collect())
            ),
            LogicalPlan::Join {
                kind,
                keys,
                residual,
                ..
            } => {
                let _ = write!(out, "Join {}", kind.name());
                if !keys.is_empty() {
                    let _ = write!(
                        out,
                        " keys=[{}]",
                        list(
                            keys.iter()
                                .map(|(l, r)| format!("{} = {}", l.to_sql(), r.to_sql()))
                                .collect()
                        )
                    );
                }
                match residual {
                    Some(r) => write!(out, " residual={}", r.to_sql()),
                    None => Ok(()),
                }
            }
            LogicalPlan::Sort { keys, .. } => write!(
                out,
                "Sort {}",
                list(keys.iter().map(SortKey::to_sql).collect())
            ),
            LogicalPlan::TopN {
                keys,
                limit,
                offset,
                ..
            } => write!(
                out,
                "TopN {} limit={limit} offset={offset}",
                list(keys.iter().map(SortKey::to_sql).collect())
            ),
            LogicalPlan::Limit { limit, offset, .. } => match limit {
                Some(n) => write!(out, "Limit {n} offset={offset}"),
                None => write!(out, "Limit offset={offset}"),
            },
        };
        out.push('\n');
        for child in self.children() {
            child.write_tree(out, depth + 1);
        }
    }
}

/// The columns of a scan when they are not all of them, and its scan filters.
fn write_scan_details(
    out: &mut String,
    schema: &Schema,
    columns: &[usize],
    filters: &[ScanFilter],
) -> std::fmt::Result {
    let name = |c: usize| schema.fields.get(c).map_or("?", |f| f.name.as_str());
    if columns.len() != schema.len() || columns.iter().enumerate().any(|(i, &c)| i != c) {
        let names: Vec<&str> = columns.iter().map(|&c| name(c)).collect();
        write!(out, " columns=[{}]", names.join(", "))?;
    }
    if !filters.is_empty() {
        let text: Vec<String> = filters
            .iter()
            .map(|f| scan_filter_text(f, name(f.column())))
            .collect();
        write!(out, " filters=[{}]", text.join(", "))?;
    }
    Ok(())
}

/// `column >= lo AND column < hi`, `column = v` or `column IN (…)`.
pub fn scan_filter_text(filter: &ScanFilter, column: &str) -> String {
    match filter {
        ScanFilter::Range { lower, upper, .. } => {
            if let (Bound::Included(l), Bound::Included(u)) = (lower, upper)
                && l == u
            {
                return format!("{column} = {}", value_to_sql(l));
            }
            let mut parts = Vec::new();
            match lower {
                Bound::Included(v) => parts.push(format!("{column} >= {}", value_to_sql(v))),
                Bound::Excluded(v) => parts.push(format!("{column} > {}", value_to_sql(v))),
                Bound::Unbounded => {}
            }
            match upper {
                Bound::Included(v) => parts.push(format!("{column} <= {}", value_to_sql(v))),
                Bound::Excluded(v) => parts.push(format!("{column} < {}", value_to_sql(v))),
                Bound::Unbounded => {}
            }
            if parts.is_empty() {
                format!("{column} IS NOT NULL")
            } else {
                parts.join(" AND ")
            }
        }
        ScanFilter::In { values, .. } => {
            let list: Vec<String> = values.iter().map(value_to_sql).collect();
            format!("{column} IN ({})", list.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::BinaryOp;
    use crate::expr::BoundExprKind;
    use veta_core::DataType;

    fn table(cols: &[(&str, DataType)]) -> Schema {
        Schema::new(
            cols.iter()
                .map(|(n, t)| Field::new(*n, *t, false))
                .collect(),
        )
    }

    #[test]
    fn join_schemas_follow_the_kind() {
        let l = LogicalPlan::scan("a", "a", table(&[("x", DataType::Int64)]));
        let r = LogicalPlan::scan("b", "b", table(&[("y", DataType::Varchar)]));
        let inner = LogicalPlan::join(l.clone(), r.clone(), JoinKind::Inner, vec![], None);
        assert_eq!(inner.schema().len(), 2);
        assert!(!inner.schema().field(1).nullable);
        let left = LogicalPlan::join(l.clone(), r.clone(), JoinKind::Left, vec![], None);
        assert!(left.schema().field(1).nullable);
        assert!(!left.schema().field(0).nullable);
        let semi = LogicalPlan::join(l, r, JoinKind::Semi, vec![], None);
        assert_eq!(semi.schema().len(), 1);
    }

    #[test]
    fn tree_display_and_pass_through_schemas() {
        let scan = LogicalPlan::scan("a", "t", table(&[("x", DataType::Int64)]));
        let x = BoundExpr::column(0, "t.x", DataType::Int64, false);
        let pred = BoundExpr::new(
            BoundExprKind::Binary {
                op: BinaryOp::Gt,
                left: Box::new(x.clone()),
                right: Box::new(BoundExpr::literal(veta_core::Value::Int64(1))),
            },
            DataType::Boolean,
            false,
        );
        let plan = LogicalPlan::limit(
            LogicalPlan::project(LogicalPlan::filter(scan, pred), vec![x], vec!["x".into()]),
            Some(10),
            0,
        );
        assert_eq!(plan.schema().field(0).name, "x");
        assert_eq!(
            plan.display_tree(),
            "Limit 10 offset=0\n  Project t.x\n    Filter (t.x > 1)\n      Scan a AS t\n"
        );
    }
}
