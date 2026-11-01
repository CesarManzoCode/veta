//! Logical plan nodes of contract §11.1 and the output schema of each one.
//!
//! Every expression of a node is bound over the output schema of its input (for joins,
//! over `left ++ right`; join keys over their own side). Besides the nodes of §11.1 there
//! is `SingleRow`, the one-row, zero-column input of a `SELECT` without `FROM` (§10.2).

use std::fmt::Write;

use veta_core::{Field, Schema};

use crate::expr::{AggregateExpr, BoundExpr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Semi,
    Anti,
    NullAwareAnti,
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
    /// Reads `columns` (indices into `table_schema`) of a stored table.
    Scan {
        table: String,
        /// Alias in the query, or the table name.
        alias: String,
        table_schema: Schema,
        columns: Vec<usize>,
        schema: Schema,
    },
    /// Reads `columns` of `read_parquet('path')`.
    ParquetScan {
        path: String,
        alias: String,
        file_schema: Schema,
        columns: Vec<usize>,
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
            schema: table_schema.clone(),
            table_schema,
        }
    }

    pub fn parquet_scan(path: &str, alias: &str, file_schema: Schema) -> LogicalPlan {
        LogicalPlan::ParquetScan {
            path: path.to_string(),
            alias: alias.to_string(),
            columns: (0..file_schema.len()).collect(),
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
            LogicalPlan::Scan { table, alias, .. } => write!(out, "Scan {table} AS {alias}"),
            LogicalPlan::ParquetScan { path, alias, .. } => {
                write!(out, "ParquetScan '{path}' AS {alias}")
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
