//! Validation of patterns/matches.

mod check_match;
mod const_to_pat;
mod deconstruct_pat;
mod usefulness;

pub(crate) use self::check_match::check_match;
use self::deconstruct_pat::{Constructor, DeconstructedPat, Fields, IntRange, Slice, SliceKind};
use self::usefulness::UsefulnessCtxt;
use crate::thir::util::UserAnnotatedTyHelpers;

use rustc_apfloat::ieee::{DoubleS, IeeeFloat, SingleS};
use rustc_arena::TypedArena;
use rustc_data_structures::captures::Captures;
use rustc_errors::struct_span_err;
use rustc_hir as hir;
use rustc_hir::def::{CtorOf, DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::pat_util::EnumerateAndAdjustIterator;
use rustc_hir::{HirId, RangeEnd};
use rustc_index::vec::Idx;
use rustc_middle::middle::stability::EvalResult;
use rustc_middle::mir::interpret::{
    get_slice_bytes, ConstValue, ErrorHandled, LitToConstError, LitToConstInput, Scalar,
};
use rustc_middle::mir::{BorrowKind, Field, Mutability};
use rustc_middle::thir::{Ascription, BindingMode, FieldPat, Pat, PatKind, PatRange, PatTyProj};
use rustc_middle::ty::subst::GenericArg;
use rustc_middle::ty::{self, ConstKind, DefIdTree, Ty, TyCtxt};
use rustc_span::symbol::Ident;
use rustc_span::{sym, Span, DUMMY_SP};
use rustc_target::abi::{Size, VariantIdx};

use smallvec::{smallvec, SmallVec};
use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Debug)]
crate enum PatternError {
    AssocConstInPattern(Span),
    ConstParamInPattern(Span),
    StaticInPattern(Span),
    NonConstPath(Span),
}

crate struct PatCtxt<'a, 'tcx> {
    crate tcx: TyCtxt<'tcx>,
    crate param_env: ty::ParamEnv<'tcx>,
    crate typeck_results: &'a ty::TypeckResults<'tcx>,
    crate errors: Vec<PatternError>,
    include_lint_checks: bool,
}

crate fn pat_from_hir<'a, 'tcx>(
    tcx: TyCtxt<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    typeck_results: &'a ty::TypeckResults<'tcx>,
    pat: &'tcx hir::Pat<'tcx>,
) -> Pat<'tcx> {
    let mut pcx = PatCtxt::new(tcx, param_env, typeck_results);
    let result = pcx.lower_pattern(pat);
    if !pcx.errors.is_empty() {
        let msg = format!("encountered errors lowering pattern: {:?}", pcx.errors);
        tcx.sess.delay_span_bug(pat.span, &msg);
    }
    debug!("pat_from_hir({:?}) = {:?}", pat, result);
    result
}

impl<'a, 'tcx> PatCtxt<'a, 'tcx> {
    crate fn new(
        tcx: TyCtxt<'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        typeck_results: &'a ty::TypeckResults<'tcx>,
    ) -> Self {
        PatCtxt { tcx, param_env, typeck_results, errors: vec![], include_lint_checks: false }
    }

    crate fn include_lint_checks(&mut self) -> &mut Self {
        self.include_lint_checks = true;
        self
    }

    crate fn lower_pattern(&mut self, pat: &'tcx hir::Pat<'tcx>) -> Pat<'tcx> {
        // When implicit dereferences have been inserted in this pattern, the unadjusted lowered
        // pattern has the type that results *after* dereferencing. For example, in this code:
        //
        // ```
        // match &&Some(0i32) {
        //     Some(n) => { ... },
        //     _ => { ... },
        // }
        // ```
        //
        // the type assigned to `Some(n)` in `unadjusted_pat` would be `Option<i32>` (this is
        // determined in rustc_typeck::check::match). The adjustments would be
        //
        // `vec![&&Option<i32>, &Option<i32>]`.
        //
        // Applying the adjustments, we want to instead output `&&Some(n)` (as a THIR pattern). So
        // we wrap the unadjusted pattern in `PatKind::Deref` repeatedly, consuming the
        // adjustments in *reverse order* (last-in-first-out, so that the last `Deref` inserted
        // gets the least-dereferenced type).
        let unadjusted_pat = self.lower_pattern_unadjusted(pat);
        self.typeck_results.pat_adjustments().get(pat.hir_id).unwrap_or(&vec![]).iter().rev().fold(
            unadjusted_pat,
            |pat, ref_ty| {
                debug!("{:?}: wrapping pattern with type {:?}", pat, ref_ty);
                Pat {
                    span: pat.span,
                    ty: ref_ty,
                    kind: Box::new(PatKind::Deref { subpattern: pat }),
                }
            },
        )
    }

    fn lower_range_expr(
        &mut self,
        expr: &'tcx hir::Expr<'tcx>,
    ) -> (PatKind<'tcx>, Option<Ascription<'tcx>>) {
        match self.lower_lit(expr) {
            PatKind::AscribeUserType { ascription, subpattern: Pat { kind: box kind, .. } } => {
                (kind, Some(ascription))
            }
            kind => (kind, None),
        }
    }

    fn lower_pattern_range(
        &mut self,
        ty: Ty<'tcx>,
        lo: &'tcx ty::Const<'tcx>,
        hi: &'tcx ty::Const<'tcx>,
        end: RangeEnd,
        span: Span,
    ) -> PatKind<'tcx> {
        assert_eq!(lo.ty, ty);
        assert_eq!(hi.ty, ty);
        let cmp = compare_const_vals(self.tcx, lo, hi, self.param_env, ty);
        match (end, cmp) {
            // `x..y` where `x < y`.
            // Non-empty because the range includes at least `x`.
            (RangeEnd::Excluded, Some(Ordering::Less)) => PatKind::Range(PatRange { lo, hi, end }),
            // `x..y` where `x >= y`. The range is empty => error.
            (RangeEnd::Excluded, _) => {
                struct_span_err!(
                    self.tcx.sess,
                    span,
                    E0579,
                    "lower range bound must be less than upper"
                )
                .emit();
                PatKind::Wild
            }
            // `x..=y` where `x == y`.
            (RangeEnd::Included, Some(Ordering::Equal)) => PatKind::Constant { value: lo },
            // `x..=y` where `x < y`.
            (RangeEnd::Included, Some(Ordering::Less)) => PatKind::Range(PatRange { lo, hi, end }),
            // `x..=y` where `x > y` hence the range is empty => error.
            (RangeEnd::Included, _) => {
                let mut err = struct_span_err!(
                    self.tcx.sess,
                    span,
                    E0030,
                    "lower range bound must be less than or equal to upper"
                );
                err.span_label(span, "lower bound larger than upper bound");
                if self.tcx.sess.teach(&err.get_code().unwrap()) {
                    err.note(
                        "When matching against a range, the compiler \
                              verifies that the range is non-empty. Range \
                              patterns include both end-points, so this is \
                              equivalent to requiring the start of the range \
                              to be less than or equal to the end of the range.",
                    );
                }
                err.emit();
                PatKind::Wild
            }
        }
    }

    fn normalize_range_pattern_ends(
        &self,
        ty: Ty<'tcx>,
        lo: Option<&PatKind<'tcx>>,
        hi: Option<&PatKind<'tcx>>,
    ) -> Option<(&'tcx ty::Const<'tcx>, &'tcx ty::Const<'tcx>)> {
        match (lo, hi) {
            (Some(PatKind::Constant { value: lo }), Some(PatKind::Constant { value: hi })) => {
                Some((lo, hi))
            }
            (Some(PatKind::Constant { value: lo }), None) => {
                Some((lo, ty.numeric_max_val(self.tcx)?))
            }
            (None, Some(PatKind::Constant { value: hi })) => {
                Some((ty.numeric_min_val(self.tcx)?, hi))
            }
            _ => None,
        }
    }

    fn lower_pattern_unadjusted(&mut self, pat: &'tcx hir::Pat<'tcx>) -> Pat<'tcx> {
        let mut ty = self.typeck_results.node_type(pat.hir_id);

        let kind = match pat.kind {
            hir::PatKind::Wild => PatKind::Wild,

            hir::PatKind::Lit(ref value) => self.lower_lit(value),

            hir::PatKind::Range(ref lo_expr, ref hi_expr, end) => {
                let (lo_expr, hi_expr) = (lo_expr.as_deref(), hi_expr.as_deref());
                let lo_span = lo_expr.map_or(pat.span, |e| e.span);
                let lo = lo_expr.map(|e| self.lower_range_expr(e));
                let hi = hi_expr.map(|e| self.lower_range_expr(e));

                let (lp, hp) = (lo.as_ref().map(|x| &x.0), hi.as_ref().map(|x| &x.0));
                let mut kind = match self.normalize_range_pattern_ends(ty, lp, hp) {
                    Some((lc, hc)) => self.lower_pattern_range(ty, lc, hc, end, lo_span),
                    None => {
                        let msg = &format!(
                            "found bad range pattern `{:?}` outside of error recovery",
                            (&lo, &hi),
                        );
                        self.tcx.sess.delay_span_bug(pat.span, msg);
                        PatKind::Wild
                    }
                };

                // If we are handling a range with associated constants (e.g.
                // `Foo::<'a>::A..=Foo::B`), we need to put the ascriptions for the associated
                // constants somewhere. Have them on the range pattern.
                for end in &[lo, hi] {
                    if let Some((_, Some(ascription))) = end {
                        let subpattern = Pat { span: pat.span, ty, kind: Box::new(kind) };
                        kind = PatKind::AscribeUserType { ascription: *ascription, subpattern };
                    }
                }

                kind
            }

            hir::PatKind::Path(ref qpath) => {
                return self.lower_path(qpath, pat.hir_id, pat.span);
            }

            hir::PatKind::Ref(ref subpattern, _) | hir::PatKind::Box(ref subpattern) => {
                PatKind::Deref { subpattern: self.lower_pattern(subpattern) }
            }

            hir::PatKind::Slice(ref prefix, ref slice, ref suffix) => {
                self.slice_or_array_pattern(pat.span, ty, prefix, slice, suffix)
            }

            hir::PatKind::Tuple(ref pats, ddpos) => {
                let tys = match ty.kind() {
                    ty::Tuple(tys) => tys,
                    _ => span_bug!(pat.span, "unexpected type for tuple pattern: {:?}", ty),
                };
                let subpatterns = self.lower_tuple_subpats(pats, tys.len(), ddpos);
                PatKind::Leaf { subpatterns }
            }

            hir::PatKind::Binding(_, id, ident, ref sub) => {
                let bm = *self
                    .typeck_results
                    .pat_binding_modes()
                    .get(pat.hir_id)
                    .expect("missing binding mode");
                let (mutability, mode) = match bm {
                    ty::BindByValue(mutbl) => (mutbl, BindingMode::ByValue),
                    ty::BindByReference(hir::Mutability::Mut) => (
                        Mutability::Not,
                        BindingMode::ByRef(BorrowKind::Mut { allow_two_phase_borrow: false }),
                    ),
                    ty::BindByReference(hir::Mutability::Not) => {
                        (Mutability::Not, BindingMode::ByRef(BorrowKind::Shared))
                    }
                };

                // A ref x pattern is the same node used for x, and as such it has
                // x's type, which is &T, where we want T (the type being matched).
                let var_ty = ty;
                if let ty::BindByReference(_) = bm {
                    if let ty::Ref(_, rty, _) = ty.kind() {
                        ty = rty;
                    } else {
                        bug!("`ref {}` has wrong type {}", ident, ty);
                    }
                };

                PatKind::Binding {
                    mutability,
                    mode,
                    name: ident.name,
                    var: id,
                    ty: var_ty,
                    subpattern: self.lower_opt_pattern(sub),
                    is_primary: id == pat.hir_id,
                }
            }

            hir::PatKind::TupleStruct(ref qpath, ref pats, ddpos) => {
                let res = self.typeck_results.qpath_res(qpath, pat.hir_id);
                let adt_def = match ty.kind() {
                    ty::Adt(adt_def, _) => adt_def,
                    _ => span_bug!(pat.span, "tuple struct pattern not applied to an ADT {:?}", ty),
                };
                let variant_def = adt_def.variant_of_res(res);
                let subpatterns = self.lower_tuple_subpats(pats, variant_def.fields.len(), ddpos);
                self.lower_variant_or_leaf(res, pat.hir_id, pat.span, ty, subpatterns)
            }

            hir::PatKind::Struct(ref qpath, ref fields, _) => {
                let res = self.typeck_results.qpath_res(qpath, pat.hir_id);
                let subpatterns = fields
                    .iter()
                    .map(|field| FieldPat {
                        field: Field::new(self.tcx.field_index(field.hir_id, self.typeck_results)),
                        pattern: self.lower_pattern(&field.pat),
                    })
                    .collect();

                self.lower_variant_or_leaf(res, pat.hir_id, pat.span, ty, subpatterns)
            }

            hir::PatKind::Or(ref pats) => PatKind::Or { pats: self.lower_patterns(pats) },
        };

        Pat { span: pat.span, ty, kind: Box::new(kind) }
    }

    fn lower_tuple_subpats(
        &mut self,
        pats: &'tcx [hir::Pat<'tcx>],
        expected_len: usize,
        gap_pos: Option<usize>,
    ) -> Vec<FieldPat<'tcx>> {
        pats.iter()
            .enumerate_and_adjust(expected_len, gap_pos)
            .map(|(i, subpattern)| FieldPat {
                field: Field::new(i),
                pattern: self.lower_pattern(subpattern),
            })
            .collect()
    }

    fn lower_patterns(&mut self, pats: &'tcx [hir::Pat<'tcx>]) -> Vec<Pat<'tcx>> {
        pats.iter().map(|p| self.lower_pattern(p)).collect()
    }

    fn lower_opt_pattern(&mut self, pat: &'tcx Option<&'tcx hir::Pat<'tcx>>) -> Option<Pat<'tcx>> {
        pat.as_ref().map(|p| self.lower_pattern(p))
    }

    fn slice_or_array_pattern(
        &mut self,
        span: Span,
        ty: Ty<'tcx>,
        prefix: &'tcx [hir::Pat<'tcx>],
        slice: &'tcx Option<&'tcx hir::Pat<'tcx>>,
        suffix: &'tcx [hir::Pat<'tcx>],
    ) -> PatKind<'tcx> {
        let prefix = self.lower_patterns(prefix);
        let slice = self.lower_opt_pattern(slice);
        let suffix = self.lower_patterns(suffix);
        match ty.kind() {
            // Matching a slice, `[T]`.
            ty::Slice(..) => PatKind::Slice { prefix, slice, suffix },
            // Fixed-length array, `[T; len]`.
            ty::Array(_, len) => {
                let len = len.eval_usize(self.tcx, self.param_env);
                assert!(len >= prefix.len() as u64 + suffix.len() as u64);
                PatKind::Array { prefix, slice, suffix }
            }
            _ => span_bug!(span, "bad slice pattern type {:?}", ty),
        }
    }

    fn lower_variant_or_leaf(
        &mut self,
        res: Res,
        hir_id: hir::HirId,
        span: Span,
        ty: Ty<'tcx>,
        subpatterns: Vec<FieldPat<'tcx>>,
    ) -> PatKind<'tcx> {
        let res = match res {
            Res::Def(DefKind::Ctor(CtorOf::Variant, ..), variant_ctor_id) => {
                let variant_id = self.tcx.parent(variant_ctor_id).unwrap();
                Res::Def(DefKind::Variant, variant_id)
            }
            res => res,
        };

        let mut kind = match res {
            Res::Def(DefKind::Variant, variant_id) => {
                let enum_id = self.tcx.parent(variant_id).unwrap();
                let adt_def = self.tcx.adt_def(enum_id);
                if adt_def.is_enum() {
                    let substs = match ty.kind() {
                        ty::Adt(_, substs) | ty::FnDef(_, substs) => substs,
                        ty::Error(_) => {
                            // Avoid ICE (#50585)
                            return PatKind::Wild;
                        }
                        _ => bug!("inappropriate type for def: {:?}", ty),
                    };
                    PatKind::Variant {
                        adt_def,
                        substs,
                        variant_index: adt_def.variant_index_with_id(variant_id),
                        subpatterns,
                    }
                } else {
                    PatKind::Leaf { subpatterns }
                }
            }

            Res::Def(
                DefKind::Struct
                | DefKind::Ctor(CtorOf::Struct, ..)
                | DefKind::Union
                | DefKind::TyAlias
                | DefKind::AssocTy,
                _,
            )
            | Res::SelfTy(..)
            | Res::SelfCtor(..) => PatKind::Leaf { subpatterns },
            _ => {
                let pattern_error = match res {
                    Res::Def(DefKind::ConstParam, _) => PatternError::ConstParamInPattern(span),
                    Res::Def(DefKind::Static, _) => PatternError::StaticInPattern(span),
                    _ => PatternError::NonConstPath(span),
                };
                self.errors.push(pattern_error);
                PatKind::Wild
            }
        };

        if let Some(user_ty) = self.user_substs_applied_to_ty_of_hir_id(hir_id) {
            debug!("lower_variant_or_leaf: kind={:?} user_ty={:?} span={:?}", kind, user_ty, span);
            kind = PatKind::AscribeUserType {
                subpattern: Pat { span, ty, kind: Box::new(kind) },
                ascription: Ascription {
                    user_ty: PatTyProj::from_user_type(user_ty),
                    user_ty_span: span,
                    variance: ty::Variance::Covariant,
                },
            };
        }

        kind
    }

    /// Takes a HIR Path. If the path is a constant, evaluates it and feeds
    /// it to `const_to_pat`. Any other path (like enum variants without fields)
    /// is converted to the corresponding pattern via `lower_variant_or_leaf`.
    fn lower_path(&mut self, qpath: &hir::QPath<'_>, id: hir::HirId, span: Span) -> Pat<'tcx> {
        let ty = self.typeck_results.node_type(id);
        let res = self.typeck_results.qpath_res(qpath, id);

        let pat_from_kind = |kind| Pat { span, ty, kind: Box::new(kind) };

        let (def_id, is_associated_const) = match res {
            Res::Def(DefKind::Const, def_id) => (def_id, false),
            Res::Def(DefKind::AssocConst, def_id) => (def_id, true),

            _ => return pat_from_kind(self.lower_variant_or_leaf(res, id, span, ty, vec![])),
        };

        // Use `Reveal::All` here because patterns are always monomorphic even if their function
        // isn't.
        let param_env_reveal_all = self.param_env.with_reveal_all_normalized(self.tcx);
        let substs = self.typeck_results.node_substs(id);
        let instance = match ty::Instance::resolve(self.tcx, param_env_reveal_all, def_id, substs) {
            Ok(Some(i)) => i,
            Ok(None) => {
                // It should be assoc consts if there's no error but we cannot resolve it.
                debug_assert!(is_associated_const);

                self.errors.push(PatternError::AssocConstInPattern(span));

                return pat_from_kind(PatKind::Wild);
            }

            Err(_) => {
                self.tcx.sess.span_err(span, "could not evaluate constant pattern");
                return pat_from_kind(PatKind::Wild);
            }
        };

        // `mir_const_qualif` must be called with the `DefId` of the item where the const is
        // defined, not where it is declared. The difference is significant for associated
        // constants.
        let mir_structural_match_violation = self.tcx.mir_const_qualif(instance.def_id()).custom_eq;
        debug!("mir_structural_match_violation({:?}) -> {}", qpath, mir_structural_match_violation);

        match self.tcx.const_eval_instance(param_env_reveal_all, instance, Some(span)) {
            Ok(value) => {
                let const_ =
                    ty::Const::from_value(self.tcx, value, self.typeck_results.node_type(id));

                let pattern = self.const_to_pat(&const_, id, span, mir_structural_match_violation);

                if !is_associated_const {
                    return pattern;
                }

                let user_provided_types = self.typeck_results().user_provided_types();
                if let Some(u_ty) = user_provided_types.get(id) {
                    let user_ty = PatTyProj::from_user_type(*u_ty);
                    Pat {
                        span,
                        kind: Box::new(PatKind::AscribeUserType {
                            subpattern: pattern,
                            ascription: Ascription {
                                /// Note that use `Contravariant` here. See the
                                /// `variance` field documentation for details.
                                variance: ty::Variance::Contravariant,
                                user_ty,
                                user_ty_span: span,
                            },
                        }),
                        ty: const_.ty,
                    }
                } else {
                    pattern
                }
            }
            Err(ErrorHandled::TooGeneric) => {
                // While `Reported | Linted` cases will have diagnostics emitted already
                // it is not true for TooGeneric case, so we need to give user more information.
                self.tcx.sess.span_err(span, "constant pattern depends on a generic parameter");
                pat_from_kind(PatKind::Wild)
            }
            Err(_) => {
                self.tcx.sess.span_err(span, "could not evaluate constant pattern");
                pat_from_kind(PatKind::Wild)
            }
        }
    }

    /// Converts literals, paths and negation of literals to patterns.
    /// The special case for negation exists to allow things like `-128_i8`
    /// which would overflow if we tried to evaluate `128_i8` and then negate
    /// afterwards.
    fn lower_lit(&mut self, expr: &'tcx hir::Expr<'tcx>) -> PatKind<'tcx> {
        if let hir::ExprKind::Path(ref qpath) = expr.kind {
            *self.lower_path(qpath, expr.hir_id, expr.span).kind
        } else {
            let (lit, neg) = match expr.kind {
                hir::ExprKind::ConstBlock(ref anon_const) => {
                    let anon_const_def_id = self.tcx.hir().local_def_id(anon_const.hir_id);
                    let value = ty::Const::from_anon_const(self.tcx, anon_const_def_id);
                    if matches!(value.val, ConstKind::Param(_)) {
                        let span = self.tcx.hir().span(anon_const.hir_id);
                        self.errors.push(PatternError::ConstParamInPattern(span));
                        return PatKind::Wild;
                    }
                    return *self.const_to_pat(value, expr.hir_id, expr.span, false).kind;
                }
                hir::ExprKind::Lit(ref lit) => (lit, false),
                hir::ExprKind::Unary(hir::UnOp::Neg, ref expr) => {
                    let lit = match expr.kind {
                        hir::ExprKind::Lit(ref lit) => lit,
                        _ => span_bug!(expr.span, "not a literal: {:?}", expr),
                    };
                    (lit, true)
                }
                _ => span_bug!(expr.span, "not a literal: {:?}", expr),
            };

            let lit_input =
                LitToConstInput { lit: &lit.node, ty: self.typeck_results.expr_ty(expr), neg };
            match self.tcx.at(expr.span).lit_to_const(lit_input) {
                Ok(val) => *self.const_to_pat(val, expr.hir_id, lit.span, false).kind,
                Err(LitToConstError::Reported) => PatKind::Wild,
                Err(LitToConstError::TypeError) => bug!("lower_lit: had type error"),
            }
        }
    }
}

impl<'tcx> UserAnnotatedTyHelpers<'tcx> for PatCtxt<'_, 'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx
    }

    fn typeck_results(&self) -> &ty::TypeckResults<'tcx> {
        self.typeck_results
    }
}

crate fn compare_const_vals<'tcx>(
    tcx: TyCtxt<'tcx>,
    a: &'tcx ty::Const<'tcx>,
    b: &'tcx ty::Const<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    ty: Ty<'tcx>,
) -> Option<Ordering> {
    trace!("compare_const_vals: {:?}, {:?}", a, b);

    let from_bool = |v: bool| v.then_some(Ordering::Equal);

    let fallback = || from_bool(a == b);

    // Use the fallback if any type differs
    if a.ty != b.ty || a.ty != ty {
        return fallback();
    }

    // Early return for equal constants (so e.g. references to ZSTs can be compared, even if they
    // are just integer addresses).
    if a.val == b.val {
        return from_bool(true);
    }

    let a_bits = a.try_eval_bits(tcx, param_env, ty);
    let b_bits = b.try_eval_bits(tcx, param_env, ty);

    if let (Some(a), Some(b)) = (a_bits, b_bits) {
        use rustc_apfloat::Float;
        return match *ty.kind() {
            ty::Float(ty::FloatTy::F32) => {
                let l = rustc_apfloat::ieee::Single::from_bits(a);
                let r = rustc_apfloat::ieee::Single::from_bits(b);
                l.partial_cmp(&r)
            }
            ty::Float(ty::FloatTy::F64) => {
                let l = rustc_apfloat::ieee::Double::from_bits(a);
                let r = rustc_apfloat::ieee::Double::from_bits(b);
                l.partial_cmp(&r)
            }
            ty::Int(ity) => {
                use rustc_middle::ty::layout::IntegerExt;
                let size = rustc_target::abi::Integer::from_int_ty(&tcx, ity).size();
                let a = size.sign_extend(a);
                let b = size.sign_extend(b);
                Some((a as i128).cmp(&(b as i128)))
            }
            _ => Some(a.cmp(&b)),
        };
    }

    if let ty::Str = ty.kind() {
        if let (
            ty::ConstKind::Value(a_val @ ConstValue::Slice { .. }),
            ty::ConstKind::Value(b_val @ ConstValue::Slice { .. }),
        ) = (a.val, b.val)
        {
            let a_bytes = get_slice_bytes(&tcx, a_val);
            let b_bytes = get_slice_bytes(&tcx, b_val);
            return from_bool(a_bytes == b_bytes);
        }
    }
    fallback()
}

/// Return the size of the corresponding number type.
#[inline]
fn number_type_size<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Size {
    use rustc_middle::ty::layout::IntegerExt;
    use rustc_target::abi::{Integer, Primitive};
    match ty.kind() {
        ty::Bool => Size::from_bytes(1),
        ty::Char => Size::from_bytes(4),
        ty::Int(ity) => Integer::from_int_ty(&tcx, *ity).size(),
        ty::Uint(uty) => Integer::from_uint_ty(&tcx, *uty).size(),
        ty::Float(ty::FloatTy::F32) => Primitive::F32.size(&tcx),
        ty::Float(ty::FloatTy::F64) => Primitive::F64.size(&tcx),
        _ => bug!("unexpected type: {}", ty),
    }
}

/// Evaluate an int constant, with a faster branch for a common case.
#[inline]
fn fast_try_eval_bits<'tcx>(
    tcx: TyCtxt<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    ty_size: Size,
    value: &ty::Const<'tcx>,
) -> Option<u128> {
    let int = if let ty::ConstKind::Value(ConstValue::Scalar(Scalar::Int(int))) = value.val {
        // If the constant is already evaluated, we shortcut here.
        int
    } else {
        // This is a more general but slower form of the previous case.
        value.val.eval(tcx, param_env).try_to_scalar_int()?
    };
    int.to_bits(ty_size).ok()
}

fn pat_to_deconstructed<'p, 'tcx>(
    ucx: &UsefulnessCtxt<'p, MatchCheckCtxt<'tcx>>,
    pat: &Pat<'tcx>,
) -> DeconstructedPat<'p, MatchCheckCtxt<'tcx>> {
    use deconstruct_pat::RangeEnd;
    let cx = &ucx.cx;
    let mkpat = |pat| pat_to_deconstructed(ucx, pat);
    let ctor;
    let fields;
    match pat.kind.as_ref() {
        PatKind::AscribeUserType { subpattern, .. } => return mkpat(subpattern),
        PatKind::Binding { subpattern: Some(subpat), .. } => return mkpat(subpat),
        PatKind::Binding { subpattern: None, .. } | PatKind::Wild => {
            ctor = Constructor::Wildcard;
            fields = Fields::empty();
        }
        PatKind::Deref { subpattern } => {
            ctor = match pat.ty.kind() {
                ty::Adt(adt, _) if adt.is_box() => Constructor::BoxPat(subpattern.ty),
                _ => Constructor::Ref(subpattern.ty),
            };
            fields = Fields::singleton(ucx, mkpat(subpattern));
        }
        PatKind::Leaf { subpatterns } | PatKind::Variant { subpatterns, .. } => {
            match pat.ty.kind() {
                ty::Tuple(fs) => {
                    ctor = Constructor::Tuple(fs.as_ref());
                    let mut wilds: SmallVec<[_; 2]> = fs
                        .iter()
                        .map(|ty| ty.expect_ty())
                        .map(DeconstructedPat::wildcard)
                        .collect();
                    for pat in subpatterns {
                        wilds[pat.field.index()] = mkpat(&pat.pattern);
                    }
                    fields = Fields::from_iter(ucx, wilds);
                }
                ty::Adt(adt, substs) if adt.is_box() => {
                    // If we're here, the pattern is using the private `Box(_, _)` constructor.
                    // Since this is private, we can ignore the subpatterns here and pretend it's a
                    // `box _`. There'll be an error later anyways. This prevents an ICE.
                    // See https://github.com/rust-lang/rust/issues/82772 ,
                    // explanation: https://github.com/rust-lang/rust/pull/82789#issuecomment-796921977
                    let inner_ty = substs.type_at(0);
                    ctor = Constructor::BoxPat(inner_ty);
                    fields = Fields::singleton(ucx, DeconstructedPat::wildcard(inner_ty));
                }
                ty::Adt(adt, _) => {
                    let variant = match *pat.kind {
                        PatKind::Leaf { .. } => {
                            ctor = Constructor::Single;
                            adt.non_enum_variant()
                        }
                        PatKind::Variant { variant_index, .. } => {
                            ctor = Constructor::Variant(variant_index);
                            &adt.variants[variant_index]
                        }
                        _ => bug!(),
                    };
                    // For each field in the variant, we store the relevant index into `self.fields` if any.
                    let mut field_id_to_id: Vec<Option<usize>> =
                        (0..variant.fields.len()).map(|_| None).collect();
                    let tys = cx.list_variant_nonhidden_fields(pat.ty, &ctor).enumerate().map(
                        |(i, (field, ty))| {
                            field_id_to_id[field.index()] = Some(i);
                            ty
                        },
                    );
                    let mut wilds: SmallVec<[_; 2]> = tys.map(DeconstructedPat::wildcard).collect();
                    for pat in subpatterns {
                        if let Some(i) = field_id_to_id[pat.field.index()] {
                            wilds[i] = mkpat(&pat.pattern);
                        }
                    }
                    fields = Fields::from_iter(ucx, wilds);
                }
                _ => bug!("pattern has unexpected type: pat: {:?}, ty: {:?}", pat, pat.ty),
            }
        }
        PatKind::Constant { value } => {
            match pat.ty.kind() {
                ty::Bool | ty::Char | ty::Int(_) | ty::Uint(_) | ty::Float(_) => {
                    use rustc_apfloat::Float;
                    let size = number_type_size(cx.tcx, value.ty);
                    ctor = match fast_try_eval_bits(cx.tcx, cx.param_env, size, value) {
                        Some(bits) => match pat.ty.kind() {
                            ty::Bool => Constructor::Bool(bits != 0),
                            ty::Char | ty::Int(_) | ty::Uint(_) => {
                                Constructor::IntRange(int_range(
                                    size.bits(),
                                    MatchCheckCtxt::is_signed_int(pat.ty),
                                    bits,
                                    bits,
                                    &RangeEnd::Included,
                                ))
                            }
                            ty::Float(ty::FloatTy::F32) => {
                                let value = rustc_apfloat::ieee::Single::from_bits(bits);
                                Constructor::F32Range(value, value, RangeEnd::Included)
                            }
                            ty::Float(ty::FloatTy::F64) => {
                                let value = rustc_apfloat::ieee::Double::from_bits(bits);
                                Constructor::F64Range(value, value, RangeEnd::Included)
                            }
                            _ => unreachable!(),
                        },
                        None => Constructor::Opaque,
                    };
                    fields = Fields::empty();
                }
                &ty::Ref(_, t, _) if t.is_str() => {
                    if let ty::ConstKind::Value(val @ ConstValue::Slice { .. }) = value.val {
                        let bytes = get_slice_bytes(&cx.tcx, val);
                        // We want a `&str` constant to behave like a `Deref` pattern, to be compatible
                        // with other `Deref` patterns. This could have been done in `const_to_pat`,
                        // but that causes issues with the rest of the matching code.
                        // So here, the constructor for a `"foo"` pattern is `&` (represented by
                        // `Single`), and has one field. That field has constructor `Str(value)` and no
                        // fields.
                        // Note: `t` is `str`, not `&str`.
                        let subpattern = DeconstructedPat::new(
                            Constructor::Str(bytes),
                            Fields::empty(),
                            t,
                            pat.span,
                        );
                        ctor = Constructor::Ref(t);
                        fields = Fields::singleton(ucx, subpattern)
                    } else {
                        // FIXME(Nadrieril): Does this ever happen?
                        ctor = Constructor::Opaque;
                        fields = Fields::empty();
                    }
                }
                // All constants that can be structurally matched have already been expanded
                // into the corresponding `Pat`s by `const_to_pat`. Constants that remain are
                // opaque.
                _ => {
                    ctor = Constructor::Opaque;
                    fields = Fields::empty();
                }
            }
        }
        &PatKind::Range(PatRange { lo, hi, end }) => {
            use rustc_apfloat::Float;
            let ty = lo.ty;
            let end = match end {
                rustc_hir::RangeEnd::Included => RangeEnd::Included,
                rustc_hir::RangeEnd::Excluded => RangeEnd::Excluded,
            };
            let size = number_type_size(cx.tcx, ty);
            let lo = fast_try_eval_bits(cx.tcx, cx.param_env, size, lo)
                .unwrap_or_else(|| bug!("expected bits of {:?}, got {:?}", ty, lo));
            let hi = fast_try_eval_bits(cx.tcx, cx.param_env, size, hi)
                .unwrap_or_else(|| bug!("expected bits of {:?}, got {:?}", ty, hi));
            ctor = match ty.kind() {
                ty::Char | ty::Int(_) | ty::Uint(_) => Constructor::IntRange(int_range(
                    size.bits(),
                    MatchCheckCtxt::is_signed_int(ty),
                    lo,
                    hi,
                    &end,
                )),
                ty::Float(ty::FloatTy::F32) => {
                    let lo = rustc_apfloat::ieee::Single::from_bits(lo);
                    let hi = rustc_apfloat::ieee::Single::from_bits(hi);
                    Constructor::F32Range(lo, hi, RangeEnd::Included)
                }
                ty::Float(ty::FloatTy::F64) => {
                    let lo = rustc_apfloat::ieee::Double::from_bits(lo);
                    let hi = rustc_apfloat::ieee::Double::from_bits(hi);
                    Constructor::F64Range(lo, hi, RangeEnd::Included)
                }
                _ => bug!("invalid type for range pattern: {}", ty),
            };
            fields = Fields::empty();
        }
        PatKind::Array { prefix, slice, suffix } | PatKind::Slice { prefix, slice, suffix } => {
            let (ty, array_len) = match pat.ty.kind() {
                ty::Array(ty, length) => {
                    (ty, Some(length.eval_usize(cx.tcx, cx.param_env) as usize))
                }
                ty::Slice(ty) => (ty, None),
                _ => span_bug!(pat.span, "bad ty {:?} for slice pattern", pat.ty),
            };
            let kind = if slice.is_some() {
                SliceKind::VarLen(prefix.len(), suffix.len())
            } else {
                SliceKind::FixedLen(prefix.len() + suffix.len())
            };
            ctor = Constructor::Slice(Slice::new(array_len, kind), *ty);
            fields = Fields::from_iter(ucx, prefix.iter().chain(suffix).map(mkpat));
        }
        PatKind::Or { .. } => {
            /// Recursively expand this pattern into its subpatterns.
            fn expand<'p, 'tcx>(pat: &'p Pat<'tcx>, vec: &mut Vec<&'p Pat<'tcx>>) {
                if let PatKind::Or { pats } = pat.kind.as_ref() {
                    for pat in pats {
                        expand(pat, vec);
                    }
                } else {
                    vec.push(pat)
                }
            }

            let mut pats = Vec::new();
            expand(pat, &mut pats);
            ctor = Constructor::Or;
            fields = Fields::from_iter(ucx, pats.into_iter().map(mkpat));
        }
    }
    DeconstructedPat::new(ctor, fields, pat.ty, pat.span)
}

fn deconstructed_to_pat<'p, 'tcx>(
    cx: &MatchCheckCtxt<'tcx>,
    pat: &DeconstructedPat<'p, MatchCheckCtxt<'tcx>>,
) -> Pat<'tcx> {
    use Constructor::*;
    let is_wildcard = |pat: &Pat<'_>| {
        matches!(*pat.kind, PatKind::Binding { subpattern: None, .. } | PatKind::Wild)
    };
    let mut subpatterns = pat.iter_fields().map(|pat| deconstructed_to_pat(cx, pat));
    let kind = match pat.ctor() {
        Single | Variant(_) => match pat.ty().kind() {
            ty::Adt(adt_def, substs) => {
                let variant_index = match pat.ctor() {
                    Variant(idx) => *idx,
                    Single => VariantIdx::new(0),
                    _ => bug!("bad constructor {:?} for adt", pat.ctor()),
                };
                let subpatterns = cx
                    .list_variant_nonhidden_fields(pat.ty(), pat.ctor())
                    .zip(subpatterns)
                    .map(|((field, _ty), pattern)| FieldPat { field, pattern })
                    .collect();

                if adt_def.is_enum() {
                    PatKind::Variant { adt_def, substs, variant_index, subpatterns }
                } else {
                    PatKind::Leaf { subpatterns }
                }
            }
            _ => bug!("unexpected ctor for type {:?} {:?}", pat.ctor(), pat.ty()),
        },
        Tuple(..) => PatKind::Leaf {
            subpatterns: subpatterns
                .enumerate()
                .map(|(i, p)| FieldPat { field: Field::new(i), pattern: p })
                .collect(),
        },
        // Note: given the expansion of `&str` patterns done in `DeconstructedPat::from_pat`,
        // we should be careful to reconstruct the correct constant pattern here. However a
        // string literal pattern will never be reported as a non-exhaustiveness witness, so we
        // ignore this issue.
        Ref(_) | BoxPat(_) => PatKind::Deref { subpattern: subpatterns.next().unwrap() },
        Slice(slice, _) => {
            match slice.kind {
                SliceKind::FixedLen(_) => {
                    PatKind::Slice { prefix: subpatterns.collect(), slice: None, suffix: vec![] }
                }
                SliceKind::VarLen(prefix, _) => {
                    let mut subpatterns = subpatterns.peekable();
                    let mut prefix: Vec<_> = subpatterns.by_ref().take(prefix).collect();
                    if slice.array_len.is_some() {
                        // Improves diagnostics a bit: if the type is a known-size array, instead
                        // of reporting `[x, _, .., _, y]`, we prefer to report `[x, .., y]`.
                        // This is incorrect if the size is not known, since `[_, ..]` captures
                        // arrays of lengths `>= 1` whereas `[..]` captures any length.
                        while !prefix.is_empty() && is_wildcard(prefix.last().unwrap()) {
                            prefix.pop();
                        }
                        while subpatterns.peek().is_some()
                            && is_wildcard(subpatterns.peek().unwrap())
                        {
                            subpatterns.next();
                        }
                    }
                    let suffix: Vec<_> = subpatterns.collect();
                    let wild = Pat::wildcard_from_ty(pat.ty());
                    PatKind::Slice { prefix, slice: Some(wild), suffix }
                }
            }
        }
        Bool(b) => PatKind::Constant { value: ty::Const::from_bool(cx.tcx, *b) },
        IntRange(range) => {
            let (lo, hi, end) = range.to_bits();
            let env = ty::ParamEnv::empty().and(pat.ty());
            let lo_const = ty::Const::from_bits(cx.tcx, lo, env);
            let hi_const = ty::Const::from_bits(cx.tcx, hi, env);

            if lo == hi {
                PatKind::Constant { value: lo_const }
            } else {
                let end = match end {
                    deconstruct_pat::RangeEnd::Included => rustc_hir::RangeEnd::Included,
                    deconstruct_pat::RangeEnd::Excluded => rustc_hir::RangeEnd::Excluded,
                };
                PatKind::Range(PatRange { lo: lo_const, hi: hi_const, end })
            }
        }
        Wildcard | NonExhaustive => PatKind::Wild,
        Missing { .. } => bug!(
            "trying to convert a `Missing` constructor into a `Pat`; this is probably a bug,
                `Missing` should have been processed in `apply_constructors`"
        ),
        // These will never be converted because we don't emit them as non-exhaustiveness
        // witnesses. And that's good because we're missing the relevant `&Const`.
        F32Range(..) | F64Range(..) | Str(_) | Opaque | Or => {
            bug!("can't convert to pattern: {:?}", pat)
        }
    };

    Pat { ty: pat.ty(), span: DUMMY_SP, kind: Box::new(kind) }
}

#[derive(Copy, Clone)]
crate struct MatchCheckCtxt<'tcx> {
    crate tcx: TyCtxt<'tcx>,
    crate param_env: ty::ParamEnv<'tcx>,
    /// The module in which the match occurs. This is necessary for
    /// checking inhabited-ness of types because whether a type is (visibly)
    /// inhabited can depend on whether it was defined in the current module or
    /// not. E.g., `struct Foo { _private: ! }` cannot be seen to be empty
    /// outside its module and should not be matchable with an empty match statement.
    crate module: DefId,
}

impl<'tcx> fmt::Debug for MatchCheckCtxt<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MatchCheckCtxt").field("module", &self.module).finish()
    }
}

impl<'tcx> usefulness::Context for MatchCheckCtxt<'tcx> {
    type Ty = Ty<'tcx>;
    type TyList = &'tcx [GenericArg<'tcx>];
    type VariantIdx = VariantIdx;
    type F32Float = IeeeFloat<SingleS>;
    type F64Float = IeeeFloat<DoubleS>;
    type StringLit = &'tcx [u8];
    type HirId = HirId;
    type Span = Span;
    type Ident = Ident;

    fn is_numeric(ty: Ty<'tcx>) -> bool {
        matches!(ty.kind(), ty::Bool | ty::Char | ty::Int(_) | ty::Uint(_))
    }

    fn is_foreign_non_exhaustive_enum(&self, ty: Ty<'tcx>) -> bool {
        match ty.kind() {
            ty::Adt(def, ..) => {
                def.is_enum() && def.is_variant_list_non_exhaustive() && !def.did.is_local()
            }
            _ => false,
        }
    }

    fn ty_list_len(ty_list: &Self::TyList) -> usize {
        ty_list.len()
    }

    type TyListIter = std::iter::Map<
        std::iter::Copied<std::slice::Iter<'tcx, GenericArg<'tcx>>>,
        fn(GenericArg<'tcx>) -> Ty<'tcx>,
    >;
    fn ty_list_iter(ty_list: &Self::TyList) -> Self::TyListIter {
        ty_list.iter().copied().map(GenericArg::expect_ty)
    }

    fn list_constructors_for_type(
        &self,
        ty: Ty<'tcx>,
        is_top_level_pattern: bool,
    ) -> SmallVec<[Constructor<Self>; 1]> {
        use Constructor::*;
        use SliceKind::*;
        let cx = self;

        debug!("list_constructors_for_type({:?})", ty);
        let make_range = |size: Size, start, end| {
            IntRange(int_range(
                size.bits(),
                Self::is_signed_int(ty),
                start,
                end,
                &deconstruct_pat::RangeEnd::Included,
            ))
        };
        match ty.kind() {
            _ if cx.is_uninhabited(ty) => smallvec![],
            ty::Bool => smallvec![Bool(false), Bool(true)],
            ty::Char => {
                let size = number_type_size(cx.tcx, ty);
                smallvec![
                    // The valid Unicode Scalar Value ranges.
                    make_range(size, '\u{0000}' as u128, '\u{D7FF}' as u128),
                    make_range(size, '\u{E000}' as u128, '\u{10FFFF}' as u128),
                ]
            }
            ty::Int(_) | ty::Uint(_)
                if ty.is_ptr_sized_integral()
                    && !cx.tcx.features().precise_pointer_size_matching =>
            {
                // `usize`/`isize` are not allowed to be matched exhaustively unless the
                // `precise_pointer_size_matching` feature is enabled. So we treat those types like
                // `#[non_exhaustive]` enums by returning a special unmatcheable constructor.
                smallvec![NonExhaustive]
            }
            &ty::Int(_) => {
                let size = number_type_size(cx.tcx, ty);
                let bits = size.bits() as u128;
                let min = 1u128 << (bits - 1);
                let max = min - 1;
                smallvec![make_range(size, min, max)]
            }
            &ty::Uint(_) => {
                let size = number_type_size(cx.tcx, ty);
                let max = size.truncate(u128::MAX);
                smallvec![make_range(size, 0, max)]
            }
            ty::Adt(def, substs) if def.is_enum() => {
                let exhaustive_patterns_feature_enabled = cx.tcx.features().exhaustive_patterns;
                let mut ctors: SmallVec<[_; 1]> = def
                    .variants
                    .iter_enumerated()
                    .filter(|(_, v)| {
                        // Filter variants that depend on a disabled unstable feature.
                        let is_enabled = !matches!(
                            cx.tcx.eval_stability(v.def_id, None, DUMMY_SP, None),
                            EvalResult::Deny { .. }
                        );
                        // If `exhaustive_patterns` is enabled, we exclude variants known to be
                        // uninhabited.
                        let is_uninhabited = exhaustive_patterns_feature_enabled
                            && v.uninhabited_from(cx.tcx, substs, def.adt_kind(), cx.param_env)
                                .contains(cx.tcx, cx.module);
                        is_enabled && !is_uninhabited
                    })
                    .map(|(idx, _)| Variant(idx))
                    .collect();

                // If the enum is declared as `#[non_exhaustive]`, we treat it as if it had an
                // additional "unknown" constructor.
                // There is no point in enumerating all possible variants, because the user can't
                // actually match against them all themselves. So we always return only the fictitious
                // constructor.
                // E.g., in an example like:
                //
                // ```
                //     let err: io::ErrorKind = ...;
                //     match err {
                //         io::ErrorKind::NotFound => {},
                //     }
                // ```
                //
                // we don't want to show every possible IO error, but instead have only `_` as the
                // witness.
                let is_declared_nonexhaustive = cx.is_foreign_non_exhaustive_enum(ty);

                // If `exhaustive_patterns` is disabled and our scrutinee is an empty enum, we treat it
                // as though it had an "unknown" constructor to avoid exposing its emptiness. The
                // exception is if the pattern is at the top level, because we want empty matches to be
                // considered exhaustive.
                let is_secretly_empty = def.variants.is_empty()
                    && !exhaustive_patterns_feature_enabled
                    && !is_top_level_pattern;

                if is_declared_nonexhaustive || is_secretly_empty {
                    ctors.push(NonExhaustive);
                }
                ctors
            }
            // The only legal non-wildcard patterns of type `Box` are box patterns, so we emit
            // that.
            ty::Adt(def, substs) if def.is_box() => smallvec![BoxPat(substs.type_at(0))],
            ty::Adt(..) => smallvec![Single],
            ty::Ref(_, ty, _) => smallvec![Ref(*ty)],
            ty::Tuple(fs) => smallvec![Tuple(fs.as_ref())],
            ty::Array(sub_ty, len) => {
                let slice = match len.try_eval_usize(cx.tcx, cx.param_env) {
                    Some(len) => {
                        // The uninhabited case is already handled.
                        self::Slice::new(Some(len as usize), VarLen(0, 0))
                    }
                    None => {
                        // Treat arrays of a constant but unknown length like slices.
                        let kind =
                            if cx.is_uninhabited(sub_ty) { FixedLen(0) } else { VarLen(0, 0) };
                        self::Slice::new(None, kind)
                    }
                };
                smallvec![Slice(slice, *sub_ty)]
            }
            ty::Slice(sub_ty) => {
                let kind = if cx.is_uninhabited(sub_ty) { FixedLen(0) } else { VarLen(0, 0) };
                smallvec![Slice(self::Slice::new(None, kind), *sub_ty)]
            }
            // If `exhaustive_patterns` is disabled and our scrutinee is the never type, we don't
            // expose its emptiness and return `NonExhaustive` below. The exception is if the
            // pattern is at the top level, because we want empty matches to be considered
            // exhaustive.
            ty::Never if is_top_level_pattern => smallvec![],
            // This type is one for which we cannot list constructors, like `str` or `f64`, or is
            // uninhabited but `exhaustive_patterns` is disabled.
            _ => smallvec![NonExhaustive],
        }
    }

    fn variant_ident(ty: Ty<'tcx>, ctor: &Constructor<Self>) -> Option<Ident> {
        match ty.kind() {
            ty::Adt(adt, _) => {
                let variant = match ctor {
                    Constructor::Variant(idx) => &adt.variants[*idx],
                    Constructor::Single => adt.non_enum_variant(),
                    _ => return None,
                };
                Some(variant.ident)
            }
            _ => None,
        }
    }

    fn is_doc_hidden_variant(&self, ty: Ty<'tcx>, idx: Self::VariantIdx) -> bool {
        if let ty::Adt(adt, _) = ty.kind() {
            let variant = &adt.variants[idx];
            let attrs = self.tcx.get_attrs(variant.def_id);
            for attr in attrs.iter() {
                if attr.has_name(sym::doc) {
                    let items = attr.meta_item_list().unwrap_or_default();
                    if items.iter().find(|item| item.has_name(sym::hidden)).is_some() {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn list_variant_nonhidden_fields(
        &self,
        ty: Ty<'tcx>,
        ctor: &Constructor<Self>,
    ) -> Vec<Ty<'tcx>> {
        self.list_variant_nonhidden_fields(ty, ctor).map(|(_, ty)| ty).collect()
    }

    fn span_bug(span: Option<Self::Span>, args: fmt::Arguments<'_>) -> ! {
        match span {
            Some(span) => rustc_middle::util::bug::span_bug_fmt(span, args),
            None => rustc_middle::util::bug::bug_fmt(args),
        }
    }
}

impl<'p, 'tcx> usefulness::Alloc<'p, &'p Self> for MatchCheckCtxt<'tcx> {
    type Cx = Self;
    type PatternArena = TypedArena<DeconstructedPat<'p, Self>>;

    fn alloc_pat(
        arena: &'p Self::PatternArena,
        pat: DeconstructedPat<'p, Self>,
    ) -> &'p DeconstructedPat<'p, Self> {
        arena.alloc(pat)
    }

    fn alloc_pats_from_iter(
        arena: &'p Self::PatternArena,
        pats: impl IntoIterator<Item = DeconstructedPat<'p, Self>>,
    ) -> &'p [DeconstructedPat<'p, Self>] {
        arena.alloc_from_iter(pats)
    }
}

impl<'tcx> MatchCheckCtxt<'tcx> {
    fn is_signed_int(ty: Ty<'tcx>) -> bool {
        matches!(ty.kind(), ty::Int(_))
    }

    fn is_uninhabited(&self, ty: Ty<'tcx>) -> bool {
        if self.tcx.features().exhaustive_patterns {
            self.tcx.is_ty_uninhabited_from(self.module, ty, self.param_env)
        } else {
            false
        }
    }

    /// List the fields for this variant.
    /// In the cases of either a `#[non_exhaustive]` field list or a non-public field, we hide
    /// uninhabited fields in order not to reveal the uninhabitedness of the whole variant.
    /// `ty` must be `ty::Adt`, and `ctor` must be a constructor for that type.
    fn list_variant_nonhidden_fields<'a>(
        &'a self,
        ty: Ty<'tcx>,
        ctor: &Constructor<Self>,
    ) -> impl Iterator<Item = (Field, Ty<'tcx>)> + Captures<'a> {
        let cx = self;
        let (adt, substs) = match ty.kind() {
            ty::Adt(adt, substs) => (adt, substs),
            _ => bug!(),
        };
        let variant = match ctor {
            Constructor::Variant(variant_index) => &adt.variants[*variant_index],
            Constructor::Single => adt.non_enum_variant(),
            _ => bug!("bad constructor {:?} for adt {:?}", ctor, ty),
        };
        // Whether we must not match the fields of this variant exhaustively.
        let is_non_exhaustive = variant.is_field_list_non_exhaustive() && !adt.did.is_local();

        variant.fields.iter().enumerate().filter_map(move |(i, field)| {
            let ty = field.ty(cx.tcx, substs);
            // `field.ty()` doesn't normalize after substituting.
            let ty = cx.tcx.normalize_erasing_regions(cx.param_env, ty);
            let is_visible = adt.is_enum() || field.vis.is_accessible_from(cx.module, cx.tcx);
            let is_uninhabited = cx.is_uninhabited(ty);

            if is_uninhabited && (!is_visible || is_non_exhaustive) {
                None
            } else {
                Some((Field::new(i), ty))
            }
        })
    }
}

fn int_range(
    ty_size: u64,
    is_signed_int: bool,
    lo: u128,
    hi: u128,
    end: &deconstruct_pat::RangeEnd,
) -> IntRange {
    match IntRange::from_bits(ty_size, is_signed_int, lo, hi, end) {
        Ok(range) => range,
        // This should have been caught earlier by E0030.
        Err(malformed_range_error) => bug!("{}", malformed_range_error),
    }
}
