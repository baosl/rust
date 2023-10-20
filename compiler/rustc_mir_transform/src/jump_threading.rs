//! A jump threading optimization.
//!
//! This optimization seeks to replace join-then-switch control flow patterns by straight jumps
//!    X = 0                                      X = 0
//! ------------\      /--------              ------------
//!    X = 1     X----X SwitchInt(X)     =>       X = 1
//! ------------/      \--------              ------------
//!
//!
//! We proceed by walking the cfg backwards starting from each `SwitchInt` terminator,
//! looking for assignments that will turn the `SwitchInt` into a simple `Goto`.
//!
//! The algorithm maintains a set of replacement conditions:
//! - `conditions[place]` contains `Condition { value, polarity: Eq, target }`
//!   if assigning `value` to `place` turns the `SwitchInt` into `Goto { target }`.
//! - `conditions[place]` contains `Condition { value, polarity: Ne, target }`
//!   if assigning anything different from `value` to `place` turns the `SwitchInt`
//!   into `Goto { target }`.
//!
//! In this file, we denote as `place ?= value` the existence of a replacement condition
//! on `place` with given `value`, irrespective of the polarity and target of that
//! replacement condition.
//!
//! We then walk the CFG backwards transforming the set of conditions.
//! When we find a fulfilling assignment, we record a `ThreadingOpportunity`.
//! All `ThreadingOpportunity`s are applied to the body, by duplicating blocks if required.
//!
//! The optimization search can be very heavy, as it performs a DFS on MIR starting from
//! each `SwitchInt` terminator. To manage the complexity, we:
//! - bound the maximum depth by a constant `MAX_BACKTRACK`;
//! - we only traverse `Goto` terminators.
//!
//! We try to avoid creating irreducible control-flow by not threading through a loop header.
//!
//! Likewise, applying the optimisation can create a lot of new MIR, so we bound the instruction
//! cost by `MAX_COST`.

use rustc_arena::DroplessArena;
use rustc_data_structures::fx::FxHashSet;
use rustc_index::bit_set::BitSet;
use rustc_index::IndexVec;
use rustc_middle::mir::visit::Visitor;
use rustc_middle::mir::*;
use rustc_middle::ty::{self, ScalarInt, Ty, TyCtxt};
use rustc_mir_dataflow::value_analysis::{Map, PlaceIndex, State, TrackElem};

use crate::cost_checker::CostChecker;
use crate::MirPass;

pub struct JumpThreading;

const MAX_BACKTRACK: usize = 5;
const MAX_COST: usize = 100;
const MAX_PLACES: usize = 100;

impl<'tcx> MirPass<'tcx> for JumpThreading {
    fn is_enabled(&self, sess: &rustc_session::Session) -> bool {
        sess.mir_opt_level() >= 4
    }

    #[instrument(skip_all level = "debug")]
    fn run_pass(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        let def_id = body.source.def_id();
        debug!(?def_id);

        let param_env = tcx.param_env_reveal_all_normalized(def_id);
        let map = Map::new(tcx, body, Some(MAX_PLACES));
        let loop_headers = loop_headers(body);

        let arena = DroplessArena::default();
        let mut finder = TOFinder {
            tcx,
            param_env,
            body,
            arena: &arena,
            map: &map,
            loop_headers: &loop_headers,
            opportunities: Vec::new(),
        };

        for (bb, bbdata) in body.basic_blocks.iter_enumerated() {
            debug!(?bb, term = ?bbdata.terminator());
            if bbdata.is_cleanup || loop_headers.contains(bb) {
                continue;
            }
            let Some((discr, targets)) = bbdata.terminator().kind.as_switch() else { continue };
            let Some(discr) = discr.place() else { continue };
            debug!(?discr, ?bb);

            let discr_ty = discr.ty(body, tcx).ty;
            let Ok(discr_layout) = tcx.layout_of(param_env.and(discr_ty)) else { continue };

            let Some(discr) = finder.map.find(discr.as_ref()) else { continue };
            debug!(?discr);

            let cost = CostChecker::new(tcx, param_env, None, body);

            let mut state = State::new(ConditionSet::default(), &finder.map);

            let conds = if let Some((value, then, else_)) = targets.as_static_if() {
                let Some(value) = ScalarInt::try_from_uint(value, discr_layout.size) else {
                    continue;
                };
                arena.alloc_from_iter([
                    Condition { value, polarity: Polarity::Eq, target: then },
                    Condition { value, polarity: Polarity::Ne, target: else_ },
                ])
            } else {
                arena.alloc_from_iter(targets.iter().filter_map(|(value, target)| {
                    let value = ScalarInt::try_from_uint(value, discr_layout.size)?;
                    Some(Condition { value, polarity: Polarity::Eq, target })
                }))
            };
            let conds = ConditionSet(conds);
            state.insert_value_idx(discr, conds, &finder.map);

            finder.find_opportunity(bb, state, cost, 0);
        }

        let opportunities = finder.opportunities;
        debug!(?opportunities);
        if opportunities.is_empty() {
            return;
        }

        // Verify that we do not thread through a loop header.
        for to in opportunities.iter() {
            assert!(to.chain.iter().all(|&block| !loop_headers.contains(block)));
        }
        OpportunitySet::new(body, opportunities).apply(body);
    }
}

#[derive(Debug)]
struct ThreadingOpportunity {
    /// The list of `BasicBlock`s from the one that found the opportunity to the `SwitchInt`.
    chain: Vec<BasicBlock>,
    /// The `SwitchInt` will be replaced by `Goto { target }`.
    target: BasicBlock,
}

struct TOFinder<'tcx, 'a> {
    tcx: TyCtxt<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    body: &'a Body<'tcx>,
    map: &'a Map,
    loop_headers: &'a BitSet<BasicBlock>,
    /// We use an arena to avoid cloning the slices when cloning `state`.
    arena: &'a DroplessArena,
    opportunities: Vec<ThreadingOpportunity>,
}

/// Represent the following statement. If we can prove that the current local is equal/not-equal
/// to `value`, jump to `target`.
#[derive(Copy, Clone, Debug)]
struct Condition {
    value: ScalarInt,
    polarity: Polarity,
    target: BasicBlock,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Polarity {
    Ne,
    Eq,
}

impl Condition {
    fn matches(&self, value: ScalarInt) -> bool {
        (self.value == value) == (self.polarity == Polarity::Eq)
    }

    fn inv(mut self) -> Self {
        self.polarity = match self.polarity {
            Polarity::Eq => Polarity::Ne,
            Polarity::Ne => Polarity::Eq,
        };
        self
    }
}

#[derive(Copy, Clone, Debug, Default)]
struct ConditionSet<'a>(&'a [Condition]);

impl<'a> ConditionSet<'a> {
    fn iter(self) -> impl Iterator<Item = Condition> + 'a {
        self.0.iter().copied()
    }

    fn iter_matches(self, value: ScalarInt) -> impl Iterator<Item = Condition> + 'a {
        self.iter().filter(move |c| c.matches(value))
    }

    fn map(self, arena: &'a DroplessArena, f: impl Fn(Condition) -> Condition) -> ConditionSet<'a> {
        ConditionSet(arena.alloc_from_iter(self.iter().map(f)))
    }
}

impl<'tcx, 'a> TOFinder<'tcx, 'a> {
    fn is_empty(&self, state: &State<ConditionSet<'a>>) -> bool {
        state.all(|cs| cs.0.is_empty())
    }

    /// Recursion entry point to find threading opportunities.
    #[instrument(level = "trace", skip(self, cost), ret)]
    fn find_opportunity(
        &mut self,
        bb: BasicBlock,
        mut state: State<ConditionSet<'a>>,
        mut cost: CostChecker<'_, 'tcx>,
        depth: usize,
    ) {
        // Do not thread through loop headers.
        if self.loop_headers.contains(bb) {
            return;
        }

        debug!(cost = ?cost.cost());
        for (statement_index, stmt) in
            self.body.basic_blocks[bb].statements.iter().enumerate().rev()
        {
            if self.is_empty(&state) {
                return;
            }

            cost.visit_statement(stmt, Location { block: bb, statement_index });
            if cost.cost() > MAX_COST {
                return;
            }

            // Attempt to turn the `current_condition` on `lhs` into a condition on another place.
            self.process_statement(bb, stmt, &mut state);

            // When a statement mutates a place, assignments to that place that happen
            // above the mutation cannot fulfill a condition.
            //   _1 = 5 // Whatever happens here, it won't change the result of a `SwitchInt`.
            //   _1 = 6
            if let Some((lhs, tail)) = self.mutated_statement(stmt) {
                state.flood_with_tail_elem(lhs.as_ref(), tail, self.map, ConditionSet::default());
            }
        }

        if self.is_empty(&state) || depth >= MAX_BACKTRACK {
            return;
        }

        let last_non_rec = self.opportunities.len();

        let predecessors = &self.body.basic_blocks.predecessors()[bb];
        if let &[pred] = &predecessors[..] && bb != START_BLOCK {
            let term = self.body.basic_blocks[pred].terminator();
            match term.kind {
                TerminatorKind::SwitchInt { ref discr, ref targets } => {
                    self.process_switch_int(discr, targets, bb, &mut state);
                    self.find_opportunity(pred, state, cost, depth + 1);
                }
                _ => self.recurse_through_terminator(pred, &state, &cost, depth),
            }
        } else {
            for &pred in predecessors {
                self.recurse_through_terminator(pred, &state, &cost, depth);
            }
        }

        let new_tos = &mut self.opportunities[last_non_rec..];
        debug!(?new_tos);

        // Try to deduplicate threading opportunities.
        if new_tos.len() > 1
            && new_tos.len() == predecessors.len()
            && predecessors
                .iter()
                .zip(new_tos.iter())
                .all(|(&pred, to)| to.chain == &[pred] && to.target == new_tos[0].target)
        {
            // All predecessors have a threading opportunity, and they all point to the same block.
            debug!(?new_tos, "dedup");
            let first = &mut new_tos[0];
            *first = ThreadingOpportunity { chain: vec![bb], target: first.target };
            self.opportunities.truncate(last_non_rec + 1);
            return;
        }

        for op in self.opportunities[last_non_rec..].iter_mut() {
            op.chain.push(bb);
        }
    }

    /// Extract the mutated place from a statement.
    ///
    /// This method returns the `Place` so we can flood the state in case of a partial assignment.
    ///     (_1 as Ok).0 = _5;
    ///     (_1 as Err).0 = _6;
    /// We want to ensure that a `SwitchInt((_1 as Ok).0)` does not see the first assignment, as
    /// the value may have been mangled by the second assignment.
    ///
    /// In case we assign to a discriminant, we return `Some(TrackElem::Discriminant)`, so we can
    /// stop at flooding the discriminant, and preserve the variant fields.
    ///     (_1 as Some).0 = _6;
    ///     SetDiscriminant(_1, 1);
    ///     switchInt((_1 as Some).0)
    #[instrument(level = "trace", skip(self), ret)]
    fn mutated_statement(
        &self,
        stmt: &Statement<'tcx>,
    ) -> Option<(Place<'tcx>, Option<TrackElem>)> {
        match stmt.kind {
            StatementKind::Assign(box (place, _))
            | StatementKind::Deinit(box place) => Some((place, None)),
            StatementKind::SetDiscriminant { box place, variant_index: _ } => {
                Some((place, Some(TrackElem::Discriminant)))
            }
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                Some((Place::from(local), None))
            }
            StatementKind::Retag(..)
            | StatementKind::Intrinsic(box NonDivergingIntrinsic::Assume(..))
            // copy_nonoverlapping takes pointers and mutated the pointed-to value.
            | StatementKind::Intrinsic(box NonDivergingIntrinsic::CopyNonOverlapping(..))
            | StatementKind::AscribeUserType(..)
            | StatementKind::Coverage(..)
            | StatementKind::FakeRead(..)
            | StatementKind::ConstEvalCounter
            | StatementKind::PlaceMention(..)
            | StatementKind::Nop => None,
        }
    }

    #[instrument(level = "trace", skip(self))]
    fn process_operand(
        &mut self,
        bb: BasicBlock,
        lhs: PlaceIndex,
        rhs: &Operand<'tcx>,
        state: &mut State<ConditionSet<'a>>,
    ) -> Option<!> {
        let register_opportunity = |c: Condition| {
            debug!(?bb, ?c.target, "register");
            self.opportunities.push(ThreadingOpportunity { chain: vec![bb], target: c.target })
        };

        match rhs {
            // If we expect `lhs ?= A`, we have an opportunity if we assume `constant == A`.
            Operand::Constant(constant) => {
                let conditions = state.try_get_idx(lhs, self.map)?;
                let constant =
                    constant.const_.normalize(self.tcx, self.param_env).try_to_scalar_int()?;
                conditions.iter_matches(constant).for_each(register_opportunity);
            }
            // Transfer the conditions on the copied rhs.
            Operand::Move(rhs) | Operand::Copy(rhs) => {
                let rhs = self.map.find(rhs.as_ref())?;
                state.insert_place_idx(rhs, lhs, self.map);
            }
        }

        None
    }

    #[instrument(level = "trace", skip(self))]
    fn process_statement(
        &mut self,
        bb: BasicBlock,
        stmt: &Statement<'tcx>,
        state: &mut State<ConditionSet<'a>>,
    ) -> Option<!> {
        let register_opportunity = |c: Condition| {
            debug!(?bb, ?c.target, "register");
            self.opportunities.push(ThreadingOpportunity { chain: vec![bb], target: c.target })
        };

        // Below, `lhs` is the return value of `mutated_statement`,
        // the place to which `conditions` apply.

        let discriminant_for_variant = |enum_ty: Ty<'tcx>, variant_index| {
            let discr = enum_ty.discriminant_for_variant(self.tcx, variant_index)?;
            let discr_layout = self.tcx.layout_of(self.param_env.and(discr.ty)).ok()?;
            let scalar = ScalarInt::try_from_uint(discr.val, discr_layout.size)?;
            Some(Operand::const_from_scalar(
                self.tcx,
                discr.ty,
                scalar.into(),
                rustc_span::DUMMY_SP,
            ))
        };

        match &stmt.kind {
            // If we expect `discriminant(place) ?= A`,
            // we have an opportunity if `variant_index ?= A`.
            StatementKind::SetDiscriminant { box place, variant_index } => {
                let discr_target = self.map.find_discr(place.as_ref())?;
                let enum_ty = place.ty(self.body, self.tcx).ty;
                let discr = discriminant_for_variant(enum_ty, *variant_index)?;
                self.process_operand(bb, discr_target, &discr, state)?;
            }
            // If we expect `lhs ?= true`, we have an opportunity if we assume `lhs == true`.
            StatementKind::Intrinsic(box NonDivergingIntrinsic::Assume(
                Operand::Copy(place) | Operand::Move(place),
            )) => {
                let conditions = state.try_get(place.as_ref(), self.map)?;
                conditions.iter_matches(ScalarInt::TRUE).for_each(register_opportunity);
            }
            StatementKind::Assign(box (lhs_place, rhs)) => {
                if let Some(lhs) = self.map.find(lhs_place.as_ref()) {
                    match rhs {
                        Rvalue::Use(operand) => self.process_operand(bb, lhs, operand, state)?,
                        // Transfer the conditions on the copy rhs.
                        Rvalue::CopyForDeref(rhs) => {
                            self.process_operand(bb, lhs, &Operand::Copy(*rhs), state)?
                        }
                        Rvalue::Discriminant(rhs) => {
                            let rhs = self.map.find_discr(rhs.as_ref())?;
                            state.insert_place_idx(rhs, lhs, self.map);
                        }
                        // If we expect `lhs ?= A`, we have an opportunity if we assume `constant == A`.
                        Rvalue::Aggregate(box ref kind, ref operands) => {
                            let agg_ty = lhs_place.ty(self.body, self.tcx).ty;
                            let lhs = match kind {
                                // Do not support unions.
                                AggregateKind::Adt(.., Some(_)) => return None,
                                AggregateKind::Adt(_, variant_index, ..) if agg_ty.is_enum() => {
                                    if let Some(discr_target) = self.map.apply(lhs, TrackElem::Discriminant)
                                        && let Some(discr_value) = discriminant_for_variant(agg_ty, *variant_index)
                                    {
                                        self.process_operand(bb, discr_target, &discr_value, state);
                                    }
                                    self.map.apply(lhs, TrackElem::Variant(*variant_index))?
                                }
                                _ => lhs,
                            };
                            for (field_index, operand) in operands.iter_enumerated() {
                                if let Some(field) =
                                    self.map.apply(lhs, TrackElem::Field(field_index))
                                {
                                    self.process_operand(bb, field, operand, state);
                                }
                            }
                        }
                        // Transfer the conditions on the copy rhs, after inversing polarity.
                        Rvalue::UnaryOp(UnOp::Not, Operand::Move(place) | Operand::Copy(place)) => {
                            let conditions = state.try_get_idx(lhs, self.map)?;
                            let place = self.map.find(place.as_ref())?;
                            let conds = conditions.map(self.arena, Condition::inv);
                            state.insert_value_idx(place, conds, self.map);
                        }
                        // We expect `lhs ?= A`. We found `lhs = Eq(rhs, B)`.
                        // Create a condition on `rhs ?= B`.
                        Rvalue::BinaryOp(
                            op,
                            box (
                                Operand::Move(place) | Operand::Copy(place),
                                Operand::Constant(value),
                            )
                            | box (
                                Operand::Constant(value),
                                Operand::Move(place) | Operand::Copy(place),
                            ),
                        ) => {
                            let conditions = state.try_get_idx(lhs, self.map)?;
                            let place = self.map.find(place.as_ref())?;
                            let equals = match op {
                                BinOp::Eq => ScalarInt::TRUE,
                                BinOp::Ne => ScalarInt::FALSE,
                                _ => return None,
                            };
                            let value = value
                                .const_
                                .normalize(self.tcx, self.param_env)
                                .try_to_scalar_int()?;
                            let conds = conditions.map(self.arena, |c| Condition {
                                value,
                                polarity: if c.matches(equals) {
                                    Polarity::Eq
                                } else {
                                    Polarity::Ne
                                },
                                ..c
                            });
                            state.insert_value_idx(place, conds, self.map);
                        }

                        _ => {}
                    }
                }
            }
            _ => {}
        }

        None
    }

    #[instrument(level = "trace", skip(self, cost))]
    fn recurse_through_terminator(
        &mut self,
        bb: BasicBlock,
        state: &State<ConditionSet<'a>>,
        cost: &CostChecker<'_, 'tcx>,
        depth: usize,
    ) {
        let register_opportunity = |c: Condition| {
            debug!(?bb, ?c.target, "register");
            self.opportunities.push(ThreadingOpportunity { chain: vec![bb], target: c.target })
        };

        let term = self.body.basic_blocks[bb].terminator();
        let place_to_flood = match term.kind {
            // We come from a target, so those are not possible.
            TerminatorKind::UnwindResume
            | TerminatorKind::UnwindTerminate(_)
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop => bug!("{term:?} has no terminators"),
            // Disallowed during optimizations.
            TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. }
            | TerminatorKind::Yield { .. } => bug!("{term:?} invalid"),
            // Cannot reason about inline asm.
            TerminatorKind::InlineAsm { .. } => return,
            // `SwitchInt` is handled specially.
            TerminatorKind::SwitchInt { .. } => return,
            // We can recurse, no thing particular to do.
            TerminatorKind::Goto { .. } => None,
            // Flood the overwritten place, and progress through.
            TerminatorKind::Drop { place: destination, .. }
            | TerminatorKind::Call { destination, .. } => Some(destination),
            // Treat as an `assume(cond == expected)`.
            TerminatorKind::Assert { ref cond, expected, .. } => {
                if let Some(place) = cond.place()
                    && let Some(conditions) = state.try_get(place.as_ref(), self.map)
                {
                    let expected = if expected { ScalarInt::TRUE } else { ScalarInt::FALSE };
                    conditions.iter_matches(expected).for_each(register_opportunity);
                }
                None
            }
        };

        // We can recurse through this terminator.
        let mut state = state.clone();
        if let Some(place_to_flood) = place_to_flood {
            state.flood_with(place_to_flood.as_ref(), self.map, ConditionSet::default());
        }
        self.find_opportunity(bb, state, cost.clone(), depth + 1);
    }

    #[instrument(level = "trace", skip(self))]
    fn process_switch_int(
        &mut self,
        discr: &Operand<'tcx>,
        targets: &SwitchTargets,
        target_bb: BasicBlock,
        state: &mut State<ConditionSet<'a>>,
    ) -> Option<!> {
        debug_assert_ne!(target_bb, START_BLOCK);
        debug_assert_eq!(self.body.basic_blocks.predecessors()[target_bb].len(), 1);

        let discr = discr.place()?;
        let discr_ty = discr.ty(self.body, self.tcx).ty;
        let discr_layout = self.tcx.layout_of(self.param_env.and(discr_ty)).ok()?;
        let conditions = state.try_get(discr.as_ref(), self.map)?;

        if let Some((value, _)) = targets.iter().find(|&(_, target)| target == target_bb) {
            let value = ScalarInt::try_from_uint(value, discr_layout.size)?;
            debug_assert_eq!(targets.iter().filter(|&(_, target)| target == target_bb).count(), 1);

            // We are inside `target_bb`. Since we have a single predecessor, we know we passed
            // through the `SwitchInt` before arriving here. Therefore, we know that
            // `discr == value`. If one condition can be fulfilled by `discr == value`,
            // that's an opportunity.
            for c in conditions.iter_matches(value) {
                debug!(?target_bb, ?c.target, "register");
                self.opportunities.push(ThreadingOpportunity { chain: vec![], target: c.target });
            }
        } else if let Some((value, _, else_bb)) = targets.as_static_if()
            && target_bb == else_bb
        {
            let value = ScalarInt::try_from_uint(value, discr_layout.size)?;

            // We only know that `discr != value`. That's much weaker information than
            // the equality we had in the previous arm. All we can conclude is that
            // the replacement condition `discr != value` can be threaded, and nothing else.
            for c in conditions.iter() {
                if c.value == value && c.polarity == Polarity::Ne {
                    debug!(?target_bb, ?c.target, "register");
                    self.opportunities
                        .push(ThreadingOpportunity { chain: vec![], target: c.target });
                }
            }
        }

        None
    }
}

struct OpportunitySet {
    opportunities: Vec<ThreadingOpportunity>,
    /// For each bb, give the TOs in which it appears. The pair corresponds to the index
    /// in `opportunities` and the index in `ThreadingOpportunity::chain`.
    involving_tos: IndexVec<BasicBlock, Vec<(usize, usize)>>,
    /// Cache the number of predecessors for each block, as we clear the basic block cache..
    predecessors: IndexVec<BasicBlock, usize>,
}

impl OpportunitySet {
    fn new(body: &Body<'_>, opportunities: Vec<ThreadingOpportunity>) -> OpportunitySet {
        let mut involving_tos = IndexVec::from_elem(Vec::new(), &body.basic_blocks);
        for (index, to) in opportunities.iter().enumerate() {
            for (ibb, &bb) in to.chain.iter().enumerate() {
                involving_tos[bb].push((index, ibb));
            }
            involving_tos[to.target].push((index, to.chain.len()));
        }
        let predecessors = predecessor_count(body);
        OpportunitySet { opportunities, involving_tos, predecessors }
    }

    /// Apply the opportunities on the graph.
    fn apply(&mut self, body: &mut Body<'_>) {
        for i in 0..self.opportunities.len() {
            self.apply_once(i, body);
        }
    }

    #[instrument(level = "trace", skip(self, body))]
    fn apply_once(&mut self, index: usize, body: &mut Body<'_>) {
        debug!(?self.predecessors);
        debug!(?self.involving_tos);

        // Check that `predecessors` satisfies its invariant.
        debug_assert_eq!(self.predecessors, predecessor_count(body));

        // Remove the TO from the vector to allow modifying the other ones later.
        let op = &mut self.opportunities[index];
        debug!(?op);
        let op_chain = std::mem::take(&mut op.chain);
        let op_target = op.target;
        debug_assert_eq!(op_chain.len(), op_chain.iter().collect::<FxHashSet<_>>().len());

        let Some((current, chain)) = op_chain.split_first() else { return };
        let basic_blocks = body.basic_blocks.as_mut();

        // Invariant: the control-flow is well-formed at the end of each iteration.
        let mut current = *current;
        for &succ in chain {
            debug!(?current, ?succ);

            // `succ` must be a successor of `current`. If it is not, this means this TO is not
            // satisfiable, so we bail out.
            if basic_blocks[current].terminator().successors().find(|s| *s == succ).is_none() {
                debug!("impossible");
                return;
            }

            // Fast path: `succ` is only used once, so we can reuse it directly.
            if self.predecessors[succ] == 1 {
                debug!("single");
                current = succ;
                continue;
            }

            let new_succ = basic_blocks.push(basic_blocks[succ].clone());
            debug!(?new_succ);

            // Replace `succ` by `new_succ` where it appears.
            let mut num_edges = 0;
            for s in basic_blocks[current].terminator_mut().successors_mut() {
                if *s == succ {
                    *s = new_succ;
                    num_edges += 1;
                }
            }

            // Update predecessors with the new block.
            let _new_succ = self.predecessors.push(num_edges);
            debug_assert_eq!(new_succ, _new_succ);
            self.predecessors[succ] -= num_edges;
            self.update_predecessor_count(basic_blocks[new_succ].terminator(), Update::Incr);

            // Replace the `current -> succ` edge by `current -> new_succ` in all the following
            // TOs. This is necessary to avoid trying to thread through a non-existing edge. We
            // use `involving_tos` here to avoid traversing the full set of TOs on each iteration.
            let mut new_involved = Vec::new();
            for &(to_index, in_to_index) in &self.involving_tos[current] {
                // That TO has already been applied, do nothing.
                if to_index <= index {
                    continue;
                }

                let other_to = &mut self.opportunities[to_index];
                if other_to.chain.get(in_to_index) != Some(&current) {
                    continue;
                }
                let s = other_to.chain.get_mut(in_to_index + 1).unwrap_or(&mut other_to.target);
                if *s == succ {
                    // `other_to` references the `current -> succ` edge, so replace `succ`.
                    *s = new_succ;
                    new_involved.push((to_index, in_to_index + 1));
                }
            }

            // The TOs that we just updated now reference `new_succ`. Update `involving_tos`
            // in case we need to duplicate an edge starting at `new_succ` later.
            let _new_succ = self.involving_tos.push(new_involved);
            debug_assert_eq!(new_succ, _new_succ);

            current = new_succ;
        }

        let current = &mut basic_blocks[current];
        self.update_predecessor_count(current.terminator(), Update::Decr);
        current.terminator_mut().kind = TerminatorKind::Goto { target: op_target };
        self.predecessors[op_target] += 1;
    }

    fn update_predecessor_count(&mut self, terminator: &Terminator<'_>, incr: Update) {
        match incr {
            Update::Incr => {
                for s in terminator.successors() {
                    self.predecessors[s] += 1;
                }
            }
            Update::Decr => {
                for s in terminator.successors() {
                    self.predecessors[s] -= 1;
                }
            }
        }
    }
}

fn predecessor_count(body: &Body<'_>) -> IndexVec<BasicBlock, usize> {
    let mut predecessors: IndexVec<_, _> =
        body.basic_blocks.predecessors().iter().map(|ps| ps.len()).collect();
    predecessors[START_BLOCK] += 1; // Account for the implicit entry edge.
    predecessors
}

enum Update {
    Incr,
    Decr,
}

/// Compute the set of loop headers in the given body. We define a loop header as a block which has
/// at least a predecessor which it dominates. This definition is only correct for reducible CFGs.
/// But if the CFG is already irreducible, there is no point in trying much harder.
/// is already irreducible.
fn loop_headers(body: &Body<'_>) -> BitSet<BasicBlock> {
    let mut loop_headers = BitSet::new_empty(body.basic_blocks.len());
    let dominators = body.basic_blocks.dominators();
    // Only visit reachable blocks.
    for (bb, bbdata) in traversal::preorder(body) {
        for succ in bbdata.terminator().successors() {
            if dominators.dominates(succ, bb) {
                loop_headers.insert(succ);
            }
        }
    }
    loop_headers
}
