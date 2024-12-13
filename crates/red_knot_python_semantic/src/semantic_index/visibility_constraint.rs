use ruff_index::IndexVec;

use crate::semantic_index::use_def::ScopedConstraintId;

use super::constraint::Constraint;

/// TODO
///
/// Used to represent active branching conditions that apply to a particular definition.
/// A definition can either be conditional on a specific constraint from a `if`, `elif`,
/// `while` statement, an `if`-expression, or a Boolean expression. Or it can be marked
/// as 'ambiguous' if it occurred in a control-flow path that is not conditional on any
/// specific expression that can be statically analyzed (`for` loop, `try` ... `except`).
///
///
/// For example:
/// ```py
/// a = 1  # no visibility constraints
///
/// if test1:
///     b = 1  # Constraint(test1)
///
///     if test2:
///         c = 1  # Constraint(test1), Constraint(test2)
///
///     for _ in range(10):
///         d = 1  # Constraint(test1), Ambiguous
/// else:
///    d = 1  # Constraint(~test1)
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VisibilityConstraintRef {
    None,
    Single(ScopedConstraintId),
    And(Box<VisibilityConstraintRef>, Box<VisibilityConstraintRef>),
    Or(Box<VisibilityConstraintRef>, Box<VisibilityConstraintRef>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VisibilityConstraint<'db> {
    None,
    Single(Constraint<'db>),
    And(
        Box<VisibilityConstraint<'db>>,
        Box<VisibilityConstraint<'db>>,
    ),
    Or(
        Box<VisibilityConstraint<'db>>,
        Box<VisibilityConstraint<'db>>,
    ),
}

impl<'db> VisibilityConstraint<'db> {
    pub(crate) fn from_ref(
        all_constraints: &IndexVec<ScopedConstraintId, Constraint<'db>>,
        visibility_constraint_ref: &VisibilityConstraintRef,
    ) -> VisibilityConstraint<'db> {
        match visibility_constraint_ref {
            VisibilityConstraintRef::None => VisibilityConstraint::None,
            VisibilityConstraintRef::Single(id) => {
                VisibilityConstraint::Single(all_constraints[*id])
            }
            VisibilityConstraintRef::And(left, right) => {
                let left = Self::from_ref(all_constraints, left);
                let right = Self::from_ref(all_constraints, right);
                VisibilityConstraint::And(Box::new(left), Box::new(right))
            }
            VisibilityConstraintRef::Or(left, right) => {
                let left = Self::from_ref(all_constraints, left);
                let right = Self::from_ref(all_constraints, right);
                VisibilityConstraint::Or(Box::new(left), Box::new(right))
            }
        }
    }
}
