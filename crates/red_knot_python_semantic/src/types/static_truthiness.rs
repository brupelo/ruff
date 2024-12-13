use crate::semantic_index::{
    ast_ids::HasScopedExpressionId,
    constraint::{Constraint, ConstraintNode, PatternConstraintKind},
    visibility_constraint::VisibilityConstraint,
};
use crate::types::{infer_expression_types, Truthiness};
use crate::Db;

/// The result of a static-truthiness analysis.
///
/// Consider the following example:
/// ```py
/// a = 1
/// if True:
///     b = 1
///     if <bool>:
///         c = 1
///         if False:
///             d = 1
/// ```
///
/// Given an iterator over the visibility constraints for each of these bindings, we would get:
/// ```txt
/// - a: {any_always_false: false, all_always_true: true,  at_least_one_condition: false}
/// - b: {any_always_false: false, all_always_true: true,  at_least_one_condition: true}
/// - c: {any_always_false: false, all_always_true: false, at_least_one_condition: true}
/// - d: {any_always_false: true,  all_always_true: false, at_least_one_condition: true}
/// ```

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StaticVisibility {
    Visible,
    Invisible,
    Ambiguous,
}

/// Analyze the (statically known) truthiness for a list of visibility constraints.
pub(crate) fn analyze_visibility<'db>(
    db: &'db dyn Db,
    visibility_constraint: VisibilityConstraint,
) -> StaticVisibility {
    match visibility_constraint {
        VisibilityConstraint::Single(Constraint {
            node: ConstraintNode::Expression(test_expr),
            is_positive,
        }) => {
            let inference = infer_expression_types(db, test_expr);
            let scope = test_expr.scope(db);
            let ty =
                inference.expression_ty(test_expr.node_ref(db).scoped_expression_id(db, scope));

            if ty.bool(db).negate_if(!is_positive).is_always_false() {
                StaticVisibility::Invisible
            } else {
                StaticVisibility::Visible
            }
        }
        VisibilityConstraint::Single(Constraint {
            node: ConstraintNode::Pattern(inner),
            ..
        }) => match inner.kind(db) {
            PatternConstraintKind::Value(value) => {
                let subject_expression = inner.subject(db);
                let inference = infer_expression_types(db, *subject_expression);
                let scope = subject_expression.scope(db);
                let subject_ty = inference.expression_ty(
                    subject_expression
                        .node_ref(db)
                        .scoped_expression_id(db, scope),
                );

                let inference = infer_expression_types(db, *value);
                let scope = value.scope(db);
                let value_ty =
                    inference.expression_ty(value.node_ref(db).scoped_expression_id(db, scope));

                if subject_ty.is_single_valued(db) {
                    if Truthiness::from(subject_ty.is_equivalent_to(db, value_ty)).is_always_false()
                    {
                        StaticVisibility::Invisible
                    } else {
                        StaticVisibility::Visible
                    }
                } else {
                    StaticVisibility::Visible
                }
            }
            PatternConstraintKind::Singleton(_) | PatternConstraintKind::Unsupported => {
                StaticVisibility::Visible
            }
        },
        VisibilityConstraint::None => StaticVisibility::Visible,
        VisibilityConstraint::And(visibility_constraint, visibility_constraint1) => {
            let lhs = analyze_visibility(db, *visibility_constraint);
            let rhs = analyze_visibility(db, *visibility_constraint1);

            if lhs == StaticVisibility::Invisible || rhs == StaticVisibility::Invisible {
                StaticVisibility::Invisible
            } else {
                StaticVisibility::Visible
            }
        }
        VisibilityConstraint::Or(visibility_constraint, visibility_constraint1) => {
            let lhs = analyze_visibility(db, *visibility_constraint);
            let rhs = analyze_visibility(db, *visibility_constraint1);

            if lhs == StaticVisibility::Invisible && rhs == StaticVisibility::Invisible {
                StaticVisibility::Invisible
            } else {
                StaticVisibility::Visible
            }
        }
    }
}

// /// Merge two static truthiness results, as if they came from two different control-flow paths.
// ///
// /// Note that the logical operations are exactly opposite to what one would expect from the names
// /// of the fields. The reason for this is that we want to draw conclusions like "this symbol can
// /// not be bound because one of the visibility constraints is always false". We can only draw this
// /// conclusion if this is true in both control-flow paths. Similarly, we want to infer that the
// /// binding of a symbol is unconditionally visible if all constraints are known to be statically
// /// true. It is enough if this is the case for either of the two control-flow paths. The other
// /// paths can not be taken if this is the case.
// pub(crate) fn flow_merge(self, other: &Self) -> Self {
//     Self {
//         any_always_false: self.any_always_false && other.any_always_false,
//         all_always_true: self.all_always_true || other.all_always_true,
//         at_least_one_constraint: self.at_least_one_constraint && other.at_least_one_constraint,
//     }
// }

// /// A static truthiness result that states our knowledge before we have seen any bindings.
// ///
// /// This is used as a starting point for merging multiple results.
// pub(crate) fn no_bindings() -> Self {
//     Self {
//         // Corresponds to "definitely unbound". Before we haven't seen any bindings, we
//         // can conclude that the symbol is not bound.
//         any_always_false: true,
//         // Corresponds to "definitely bound". Before we haven't seen any bindings, we
//         // can not conclude that the symbol is bound.
//         all_always_true: false,
//         // Irrelevant for this analysis.
//         at_least_one_constraint: false,
//     }
// }
