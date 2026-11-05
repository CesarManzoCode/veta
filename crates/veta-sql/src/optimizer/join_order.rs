//! Rule 3 of contract §11.2: join graph reordering by estimated cost.
//!
//! Every maximal tree of `Inner`/`Cross` joins is flattened into relations and predicates.
//! The tree reaches through `Filter`s and through `Project`s that only pick columns; anything
//! else is a relation, including `Left`, `Semi`, `Anti`, `NullAwareAnti` and `Single` joins,
//! whose inputs are optimized on their own and never reordered or swapped.
//!
//! Up to [`DPCCP_MAX_RELATIONS`] relations the enumeration is DPccp (Moerkotte & Neumann,
//! VLDB 2006): every connected subgraph with every connected complement, so no plan of a
//! connected graph holds a cross product. Above that, GOO: repeatedly join the two trees with
//! the fewest estimated output rows. The cost is `C = Σ (2·|build| + |probe| + |out|)` with
//! the estimates of §11.5; the side with fewer estimated rows goes on the right and builds.
//!
//! Decisions within the contract, labelled as design:
//!
//! - `EnumerateCmp` extends a complement with the excluded set `X ∪ (B_i ∩ N)`. The listing in
//!   the paper says `X ∪ N`, which never emits a complement holding two neighbors of `S1`
//!   (in a triangle, `{0}` with `{1, 2}`); the counts of its own Figure 3 need the former.
//! - A conjunct over exactly two relations is an edge of the graph, whatever its shape. A
//!   conjunct over three or more is not: it applies at the lowest join that covers it.
//! - The estimate of a set of relations is that of its cheapest plan, so a set may be costed
//!   from whichever subplans won; the plan keeps the same numbers when it is estimated again.
//! - Connected components (only possible without an edge between them) are joined by GOO,
//!   always preferring a pair that some predicate connects; the rest are cross products.
//! - A tree with more than [`MAX_RELATIONS`] relations keeps the order the binder built.
//! - Equal estimates keep the first side in `FROM` order on the left.

use std::collections::{BTreeSet, HashMap};

use veta_core::DataType;

use super::{map_children, with_filter};
use crate::cardinality::{Estimator, JoinEstimate};
use crate::catalog::Catalog;
use crate::expr::{BinaryOp, BoundExpr, BoundExprKind};
use crate::plan::{JoinKind, LogicalPlan};

/// Relations up to which the enumeration is DPccp (§11.2).
pub const DPCCP_MAX_RELATIONS: usize = 12;
/// Relations a set can hold, one bit each.
pub const MAX_RELATIONS: usize = 64;

/// Reorders every join tree of `plan`; the output of each tree keeps its columns.
pub fn reorder_joins(plan: LogicalPlan, catalog: &dyn Catalog) -> LogicalPlan {
    reorder(plan, &Estimator::new(catalog))
}

fn reorder(plan: LogicalPlan, estimator: &Estimator<'_>) -> LogicalPlan {
    if !is_tree_node(&plan) {
        map_children(plan, &mut |child| reorder(child, estimator))
    } else if relation_count(&plan) <= MAX_RELATIONS {
        reorder_tree(plan, estimator)
    } else {
        keep_tree(plan, estimator)
    }
}

/// Leaves a tree with too many relations as it is, reordering only inside its relations.
fn keep_tree(plan: LogicalPlan, estimator: &Estimator<'_>) -> LogicalPlan {
    if is_tree_node(&plan) {
        map_children(plan, &mut |child| keep_tree(child, estimator))
    } else {
        reorder(plan, estimator)
    }
}

/// The column a `Project` output picks, if that is all it does.
fn picked_column(e: &BoundExpr) -> Option<(usize, &str)> {
    match &e.kind {
        BoundExprKind::Column { index, name } => Some((*index, name.as_str())),
        _ => None,
    }
}

/// Whether `plan` belongs to a join tree: an inner or cross join, or a `Filter` or a `Project`
/// of columns over one.
fn is_tree_node(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Join {
            kind: JoinKind::Inner | JoinKind::Cross,
            ..
        } => true,
        LogicalPlan::Filter { input, .. } => is_tree_node(input),
        LogicalPlan::Project { input, exprs, .. } => {
            exprs.iter().all(|e| picked_column(e).is_some()) && is_tree_node(input)
        }
        _ => false,
    }
}

fn relation_count(plan: &LogicalPlan) -> usize {
    if is_tree_node(plan) {
        plan.children().into_iter().map(relation_count).sum()
    } else {
        1
    }
}

// ---------------------------------------------------------------------------------------
// Flattening

/// An output column of a tree node: a column of a relation and its SQL text.
#[derive(Debug, Clone)]
struct OutputColumn {
    /// The column numbered across all relations (`offset` of the relation + its index).
    global: usize,
    name: String,
}

#[derive(Debug)]
struct Relation {
    plan: LogicalPlan,
    offset: usize,
    width: usize,
}

struct Flattener<'e, 'c> {
    estimator: &'e Estimator<'c>,
    relations: Vec<Relation>,
    width: usize,
    /// Conjuncts over global columns.
    conjuncts: Vec<BoundExpr>,
}

fn over_globals(mut e: BoundExpr, output: &[OutputColumn]) -> BoundExpr {
    e.map_columns(&mut |c| output[c].global);
    e
}

fn equals(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    let nullable = left.nullable || right.nullable;
    BoundExpr::new(
        BoundExprKind::Binary {
            op: BinaryOp::Eq,
            left: Box::new(left),
            right: Box::new(right),
        },
        DataType::Boolean,
        nullable,
    )
}

impl Flattener<'_, '_> {
    fn add_conjuncts(&mut self, predicate: BoundExpr, output: &[OutputColumn]) {
        for conjunct in predicate.into_conjuncts() {
            self.conjuncts.push(over_globals(conjunct, output));
        }
    }

    fn add_relation(&mut self, plan: LogicalPlan) -> Vec<OutputColumn> {
        let plan = reorder(plan, self.estimator);
        let names = plan.output_sql();
        let offset = self.width;
        let width = names.len();
        self.width += width;
        self.relations.push(Relation {
            plan,
            offset,
            width,
        });
        names
            .into_iter()
            .enumerate()
            .map(|(i, name)| OutputColumn {
                global: offset + i,
                name,
            })
            .collect()
    }

    /// Takes the relations and predicates of `plan`; returns its output columns.
    fn flatten(&mut self, plan: LogicalPlan) -> Vec<OutputColumn> {
        let in_tree = is_tree_node(&plan);
        match plan {
            LogicalPlan::Filter { input, predicate } if in_tree => {
                let output = self.flatten(*input);
                self.add_conjuncts(predicate, &output);
                output
            }
            LogicalPlan::Project { input, exprs, .. } if in_tree => {
                let output = self.flatten(*input);
                exprs
                    .iter()
                    .map(|e| {
                        let (index, name) =
                            picked_column(e).expect("a flattened Project only picks columns");
                        OutputColumn {
                            global: output[index].global,
                            name: name.to_string(),
                        }
                    })
                    .collect()
            }
            LogicalPlan::Join {
                left,
                right,
                keys,
                residual,
                ..
            } if in_tree => {
                let left = self.flatten(*left);
                let right = self.flatten(*right);
                for (l, r) in keys {
                    self.conjuncts
                        .push(equals(over_globals(l, &left), over_globals(r, &right)));
                }
                let mut output = left;
                output.extend(right);
                if let Some(residual) = residual {
                    self.add_conjuncts(residual, &output);
                }
                output
            }
            leaf => self.add_relation(leaf),
        }
    }
}

/// The relations `e` reads, as a set.
fn relations_of(e: &BoundExpr, relation_of: &[usize]) -> u64 {
    e.columns()
        .into_iter()
        .fold(0, |set, c| set | 1u64 << relation_of[c])
}

/// Edges of the join graph: conjuncts over exactly two relations.
fn join_graph(conjuncts: &[BoundExpr], relation_of: &[usize], relations: usize) -> Vec<u64> {
    let mut neighbors = vec![0u64; relations];
    for conjunct in conjuncts {
        let set = relations_of(conjunct, relation_of);
        if set.count_ones() == 2 {
            let a = set.trailing_zeros() as usize;
            let b = 63 - set.leading_zeros() as usize;
            neighbors[a] |= 1 << b;
            neighbors[b] |= 1 << a;
        }
    }
    neighbors
}

/// The vertices in breadth-first order, one connected component after the other: DPccp needs
/// this numbering (§3.2 of the paper).
fn breadth_first_order(neighbors: &[u64]) -> Vec<usize> {
    let mut order = Vec::with_capacity(neighbors.len());
    let mut seen = 0u64;
    for start in 0..neighbors.len() {
        if seen >> start & 1 == 1 {
            continue;
        }
        seen |= 1 << start;
        let mut next = order.len();
        order.push(start);
        while next < order.len() {
            let mut fresh = neighbors[order[next]] & !seen;
            seen |= fresh;
            while fresh != 0 {
                order.push(fresh.trailing_zeros() as usize);
                fresh &= fresh - 1;
            }
            next += 1;
        }
    }
    order
}

/// The vertex sets of the connected components.
fn connected_components(neighbors: &[u64]) -> Vec<u64> {
    let mut components = Vec::new();
    let mut seen = 0u64;
    for start in 0..neighbors.len() {
        if seen >> start & 1 == 1 {
            continue;
        }
        let mut component = 1u64 << start;
        loop {
            let grown = component | neighborhood(neighbors, component);
            if grown == component {
                break;
            }
            component = grown;
        }
        seen |= component;
        components.push(component);
    }
    components
}

// ---------------------------------------------------------------------------------------
// DPccp enumeration (names as in the paper)

/// `N(S)`: the vertices outside `set` with an edge into it.
fn neighborhood(neighbors: &[u64], set: u64) -> u64 {
    let mut out = 0;
    let mut rest = set;
    while rest != 0 {
        out |= neighbors[rest.trailing_zeros() as usize];
        rest &= rest - 1;
    }
    out & !set
}

/// `B_i = {v_j | j ≤ i}`.
fn up_to(i: u32) -> u64 {
    if i >= 63 {
        u64::MAX
    } else {
        (1u64 << (i + 1)) - 1
    }
}

/// The non-empty subsets of `set`, ascending as numbers: each after its own subsets.
fn subsets(set: u64) -> impl Iterator<Item = u64> {
    let mut sub = 0u64;
    std::iter::from_fn(move || {
        sub = sub.wrapping_sub(set) & set;
        (sub != 0).then_some(sub)
    })
}

/// `EnumerateCsgRec`: emits every connected extension of `set` that avoids `excluded`.
fn enumerate_csg_rec(neighbors: &[u64], set: u64, excluded: u64, emit: &mut dyn FnMut(u64)) {
    let around = neighborhood(neighbors, set) & !excluded;
    for sub in subsets(around) {
        emit(set | sub);
    }
    for sub in subsets(around) {
        enumerate_csg_rec(neighbors, set | sub, excluded | around, emit);
    }
}

/// `EnumerateCmp`: emits `(s1, s2)` for every connected complement `s2` of `s1`.
fn enumerate_cmp(neighbors: &[u64], s1: u64, emit: &mut dyn FnMut(u64, u64)) {
    let excluded = up_to(s1.trailing_zeros()) | s1;
    let around = neighborhood(neighbors, s1) & !excluded;
    let mut rest = around;
    while rest != 0 {
        let i = 63 - rest.leading_zeros();
        let start = 1u64 << i;
        rest &= !start;
        emit(s1, start);
        enumerate_csg_rec(
            neighbors,
            start,
            excluded | (up_to(i) & around),
            &mut |s2| emit(s1, s2),
        );
    }
}

/// Every csg-cmp pair of a breadth-first numbered graph, once and in an order valid for
/// dynamic programming: when `(s1, s2)` comes, every connected subset of each was a union of
/// an earlier pair.
fn for_each_csg_cmp(neighbors: &[u64], emit: &mut dyn FnMut(u64, u64)) {
    for i in (0..neighbors.len() as u32).rev() {
        let start = 1u64 << i;
        let mut on_csg = |s1: u64| enumerate_cmp(neighbors, s1, &mut *emit);
        on_csg(start);
        enumerate_csg_rec(neighbors, start, up_to(i), &mut on_csg);
    }
}

// ---------------------------------------------------------------------------------------
// Dynamic programming over the estimates

/// `left = right` with each side over relations the other does not read.
#[derive(Debug)]
struct Equality {
    left: BoundExpr,
    left_relations: u64,
    right: BoundExpr,
    right_relations: u64,
}

#[derive(Debug)]
struct Predicate {
    expr: BoundExpr,
    relations: u64,
    equality: Option<Equality>,
}

impl Predicate {
    fn new(expr: BoundExpr, relation_of: &[usize]) -> Predicate {
        let relations = relations_of(&expr, relation_of);
        let equality = match &expr.kind {
            BoundExprKind::Binary {
                op: BinaryOp::Eq,
                left,
                right,
            } => {
                let left_relations = relations_of(left, relation_of);
                let right_relations = relations_of(right, relation_of);
                (left_relations != 0
                    && right_relations != 0
                    && left_relations & right_relations == 0)
                    .then(|| Equality {
                        left: (**left).clone(),
                        left_relations,
                        right: (**right).clone(),
                        right_relations,
                    })
            }
            _ => None,
        };
        Predicate {
            expr,
            relations,
            equality,
        }
    }

    /// Whether joining the sets `left` and `right` is the lowest join that covers it.
    fn applies(&self, left: u64, right: u64) -> bool {
        self.relations & !(left | right) == 0
            && self.relations & !left != 0
            && self.relations & !right != 0
    }

    /// `(left side, right side)` when it can be a key of the join of `left` and `right`.
    fn key(&self, left: u64, right: u64) -> Option<(&BoundExpr, &BoundExpr)> {
        let e = self.equality.as_ref()?;
        if e.left_relations & !left == 0 && e.right_relations & !right == 0 {
            Some((&e.left, &e.right))
        } else if e.left_relations & !right == 0 && e.right_relations & !left == 0 {
            Some((&e.right, &e.left))
        } else {
            None
        }
    }
}

/// `e` with its global columns as positions in `columns`.
fn rebound(e: &BoundExpr, columns: &[usize]) -> BoundExpr {
    let mut e = e.clone();
    e.map_columns(&mut |global| {
        columns
            .iter()
            .position(|&c| c == global)
            .expect("a predicate column is an input of its join")
    });
    e
}

/// The best plan found for a set of relations; `right == 0` for one relation.
#[derive(Debug)]
struct BestPlan {
    left: u64,
    right: u64,
    cost: f64,
    estimate: JoinEstimate,
    /// The global column of each column of `estimate`.
    columns: Vec<usize>,
}

struct Solver<'e, 'c> {
    estimator: &'e Estimator<'c>,
    predicates: Vec<Predicate>,
    neighbors: Vec<u64>,
    best: HashMap<u64, BestPlan>,
}

impl Solver<'_, '_> {
    /// The keys and the residual of the join of the sets `left` and `right`, bound over their
    /// `left_columns` and `right_columns`.
    fn bind(
        &self,
        left: u64,
        right: u64,
        left_columns: &[usize],
        right_columns: &[usize],
    ) -> (Vec<(BoundExpr, BoundExpr)>, Option<BoundExpr>) {
        let mut keys = Vec::new();
        let mut residual = Vec::new();
        let mut both: Option<Vec<usize>> = None;
        for predicate in &self.predicates {
            if !predicate.applies(left, right) {
                continue;
            }
            match predicate.key(left, right) {
                Some((l, r)) => keys.push((rebound(l, left_columns), rebound(r, right_columns))),
                None => {
                    let both = both.get_or_insert_with(|| {
                        let mut all = left_columns.to_vec();
                        all.extend_from_slice(right_columns);
                        all
                    });
                    residual.push(rebound(&predicate.expr, both));
                }
            }
        }
        (keys, BoundExpr::and_all(residual))
    }

    /// The join of the best plans of `a` and `b`, with the smaller one on the right.
    fn join(&self, a: u64, b: u64) -> BestPlan {
        let (plan_a, plan_b) = (&self.best[&a], &self.best[&b]);
        let ((left, l), (right, r)) = if plan_b.estimate.rows() > plan_a.estimate.rows() {
            ((b, plan_b), (a, plan_a))
        } else {
            ((a, plan_a), (b, plan_b))
        };
        let (keys, residual) = self.bind(left, right, &l.columns, &r.columns);
        let estimate =
            self.estimator
                .inner_join(&l.estimate, &r.estimate, &keys, residual.as_ref());
        let (build, probe, out) = (r.estimate.rows(), l.estimate.rows(), estimate.rows());
        let mut columns = l.columns.clone();
        columns.extend_from_slice(&r.columns);
        BestPlan {
            left,
            right,
            cost: l.cost + r.cost + 2.0 * build + probe + out,
            estimate,
            columns,
        }
    }

    fn consider(&mut self, a: u64, b: u64) {
        let candidate = self.join(a, b);
        let set = a | b;
        if self
            .best
            .get(&set)
            .is_none_or(|known| candidate.cost < known.cost)
        {
            self.best.insert(set, candidate);
        }
    }

    /// GOO over `trees`: joins the pair with the fewest output rows, preferring a pair some
    /// predicate connects, until one tree is left.
    fn greedy(&mut self, mut trees: Vec<u64>) -> u64 {
        while trees.len() > 1 {
            let mut chosen: Option<(bool, usize, usize, BestPlan)> = None;
            for i in 0..trees.len() {
                for j in i + 1..trees.len() {
                    let candidate = self.join(trees[i], trees[j]);
                    let connected = self
                        .predicates
                        .iter()
                        .any(|p| p.applies(trees[i], trees[j]));
                    let better = chosen.as_ref().is_none_or(|(known, _, _, plan)| {
                        (connected && !known)
                            || (connected == *known
                                && candidate.estimate.rows() < plan.estimate.rows())
                    });
                    if better {
                        chosen = Some((connected, i, j, candidate));
                    }
                }
            }
            let (_, i, j, plan) = chosen.expect("two trees make a pair");
            let set = trees[i] | trees[j];
            self.best.insert(set, plan);
            trees[i] = set;
            trees.remove(j);
        }
        trees[0]
    }

    /// Fills `best` and returns the set of all the relations.
    fn solve(&mut self) -> u64 {
        let relations = self.neighbors.len();
        if relations > DPCCP_MAX_RELATIONS {
            return self.greedy((0..relations).map(|i| 1u64 << i).collect());
        }
        let neighbors = self.neighbors.clone();
        for_each_csg_cmp(&neighbors, &mut |a, b| self.consider(a, b));
        self.greedy(connected_components(&neighbors))
    }

    /// The plan of `set` and the global column of each of its output columns.
    fn build(&self, set: u64, relations: &mut [Option<Relation>]) -> (LogicalPlan, Vec<usize>) {
        let best = &self.best[&set];
        if best.right == 0 {
            let relation = relations[set.trailing_zeros() as usize]
                .take()
                .expect("a plan uses each relation once");
            let columns = (relation.offset..relation.offset + relation.width).collect();
            return (relation.plan, columns);
        }
        let (left, left_columns) = self.build(best.left, relations);
        let (right, right_columns) = self.build(best.right, relations);
        let (keys, residual) = self.bind(best.left, best.right, &left_columns, &right_columns);
        let kind = if keys.is_empty() && residual.is_none() {
            JoinKind::Cross
        } else {
            JoinKind::Inner
        };
        let mut columns = left_columns;
        columns.extend(right_columns);
        (
            LogicalPlan::join(left, right, kind, keys, residual),
            columns,
        )
    }
}

/// Reorders the tree whose root is `plan`. A `Project` on top gives back the columns of
/// `plan`, in its order and with its schema, when the new tree has them otherwise.
fn reorder_tree(plan: LogicalPlan, estimator: &Estimator<'_>) -> LogicalPlan {
    let schema = plan.schema();
    let mut flat = Flattener {
        estimator,
        relations: Vec::new(),
        width: 0,
        conjuncts: Vec::new(),
    };
    let output = flat.flatten(plan);
    let Flattener {
        relations,
        conjuncts,
        width,
        ..
    } = flat;

    // Number the relations breadth-first over the graph of the original numbering.
    let mut relation_of = vec![0usize; width];
    for (r, relation) in relations.iter().enumerate() {
        relation_of[relation.offset..relation.offset + relation.width].fill(r);
    }
    let order = breadth_first_order(&join_graph(&conjuncts, &relation_of, relations.len()));
    let mut label = vec![0usize; relations.len()];
    for (new, &old) in order.iter().enumerate() {
        label[old] = new;
    }
    relation_of.iter_mut().for_each(|r| *r = label[*r]);
    let mut slots: Vec<Option<Relation>> = relations.into_iter().map(Some).collect();
    let mut relations: Vec<Relation> = order
        .iter()
        .map(|&old| {
            slots[old]
                .take()
                .expect("an order visits each relation once")
        })
        .collect();

    // Sort the conjuncts: constants stay on top, one relation filters it, the rest are joins.
    let mut constants = Vec::new();
    let mut joining = Vec::new();
    let mut filters: Vec<Vec<BoundExpr>> = relations.iter().map(|_| Vec::new()).collect();
    for conjunct in conjuncts {
        let set = relations_of(&conjunct, &relation_of);
        match set.count_ones() {
            0 => constants.push(conjunct),
            1 => {
                let r = set.trailing_zeros() as usize;
                let mut local = conjunct;
                let offset = relations[r].offset;
                local.map_columns(&mut |c| c - offset);
                filters[r].push(local);
            }
            _ => joining.push(conjunct),
        }
    }
    relations = relations
        .into_iter()
        .zip(filters)
        .map(|(relation, filter)| Relation {
            plan: with_filter(relation.plan, filter),
            ..relation
        })
        .collect();

    let mut read = BTreeSet::new();
    for conjunct in &joining {
        read.extend(conjunct.columns());
    }
    let neighbors = join_graph(&joining, &relation_of, relations.len());
    estimator.reset();
    let mut best = HashMap::with_capacity(relations.len());
    for (r, relation) in relations.iter().enumerate() {
        let columns: Vec<usize> = read
            .range(relation.offset..relation.offset + relation.width)
            .copied()
            .collect();
        let local: Vec<usize> = columns.iter().map(|c| c - relation.offset).collect();
        best.insert(
            1u64 << r,
            BestPlan {
                left: 0,
                right: 0,
                cost: 0.0,
                estimate: estimator.join_input(&relation.plan, &local),
                columns,
            },
        );
    }
    let mut solver = Solver {
        estimator,
        predicates: joining
            .into_iter()
            .map(|expr| Predicate::new(expr, &relation_of))
            .collect(),
        neighbors,
        best,
    };
    let root = solver.solve();
    let mut slots: Vec<Option<Relation>> = relations.into_iter().map(Some).collect();
    let (plan, columns) = solver.build(root, &mut slots);
    let plan = with_filter(plan, constants);

    let plan_schema = plan.schema();
    let exprs: Vec<BoundExpr> = output
        .into_iter()
        .map(|column| {
            let position = columns
                .iter()
                .position(|&c| c == column.global)
                .expect("the tree holds every column of its relations");
            let field = plan_schema.field(position);
            BoundExpr::column(position, column.name, field.data_type, field.nullable)
        })
        .collect();
    let same_columns = exprs.len() == plan_schema.len()
        && exprs
            .iter()
            .enumerate()
            .all(|(i, e)| picked_column(e).is_some_and(|(index, _)| index == i));
    if same_columns && plan_schema == schema {
        plan
    } else {
        LogicalPlan::Project {
            input: Box::new(plan),
            exprs,
            schema,
        }
    }
}

#[cfg(test)]
mod tests;
