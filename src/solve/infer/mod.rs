use ena::unify as ena;
use ir::*;

pub mod canonicalize;
pub mod ucanonicalize;
mod normalize_deep;
pub mod instantiate;
mod invert;
pub mod unify;
pub mod var;
#[cfg(test)]
mod test;

use self::var::*;

#[derive(Clone)]
pub struct InferenceTable {
    unify: ena::UnificationTable<InferenceVariable>,
    vars: Vec<InferenceVariable>,
    max_universe: UniverseIndex,
}

pub struct InferenceSnapshot {
    unify_snapshot: ena::Snapshot<InferenceVariable>,
    max_universe: UniverseIndex,
    vars: Vec<InferenceVariable>,
}

pub type ParameterInferenceVariable = ParameterKind<InferenceVariable>;

impl InferenceTable {
    /// Create an empty inference table with no variables.
    pub fn new() -> Self {
        InferenceTable {
            unify: ena::UnificationTable::new(),
            vars: vec![],
            max_universe: UniverseIndex::root(),
        }
    }

    /// Creates and returns a fresh universe that is distinct from all
    /// others created within this inference table. This universe is
    /// able to see all previously created universes (though hopefully
    /// it is only brought into contact with its logical *parents*).
    pub fn new_universe(&mut self) -> UniverseIndex {
        let u = self.max_universe.next();
        self.max_universe = u;
        u
    }

    /// Creates and returns a fresh universe that is distinct from all
    /// others created within this inference table. This universe is
    /// able to see all previously created universes (though hopefully
    /// it is only brought into contact with its logical *parents*).
    pub fn instantiate_universes<'v, T>(&mut self, value: &'v UCanonical<T>) -> &'v Canonical<T> {
        let UCanonical { universes, canonical } = value;
        assert!(*universes >= 1); // always have U0
        for _ in 1 .. *universes {
            self.new_universe();
        }
        canonical
    }

    /// Current maximum universe -- one that can see all existing names.
    pub fn max_universe(&self) -> UniverseIndex {
        self.max_universe
    }

    /// Creates a new inference variable and returns its index. The
    /// kind of the variable should be known by the caller, but is not
    /// tracked directly by the inference table.
    pub fn new_variable(&mut self, ui: UniverseIndex) -> InferenceVariable {
        let var = self.unify.new_key(InferenceValue::Unbound(ui));
        self.vars.push(var);
        debug!("new_variable: var={:?} ui={:?}", var, ui);
        var
    }

    /// Takes a "snapshot" of the current state of the inference
    /// table.  Later, you must invoke either `rollback_to` or
    /// `commit` with that snapshot.  Snapshots can be nested, but you
    /// must respect a stack discipline (i.e., rollback or commit
    /// snapshots in reverse order of that with which they were
    /// created).
    pub fn snapshot(&mut self) -> InferenceSnapshot {
        let unify_snapshot = self.unify.snapshot();
        let vars = self.vars.clone();
        let max_universe = self.max_universe;
        InferenceSnapshot {
            unify_snapshot,
            max_universe,
            vars,
        }
    }

    /// Restore the table to the state it had when the snapshot was taken.
    pub fn rollback_to(&mut self, snapshot: InferenceSnapshot) {
        self.unify.rollback_to(snapshot.unify_snapshot);
        self.vars = snapshot.vars;
        self.max_universe = snapshot.max_universe;
    }

    /// Make permanent the changes made since the snapshot was taken.
    pub fn commit(&mut self, snapshot: InferenceSnapshot) {
        self.unify.commit(snapshot.unify_snapshot);
    }

    /// This helper function creates a snapshot and then execues `op`;
    /// if `op` returns `Ok(v)`, then the snapshot is committed and
    /// `Ok(v)` is returned.  If `op` returns `Err(e)`, then the
    /// changes are rolled back and `Err(e)` is propagated.
    ///
    /// This is commonly used to perform a series of smaller changes,
    /// some of which may be fallible; the result is that either all
    /// the changes take effect, or none.
    pub fn commit_if_ok<F, R, E>(&mut self, op: F) -> Result<R, E>
    where
        F: FnOnce(&mut Self) -> Result<R, E>,
    {
        let snapshot = self.snapshot();
        match op(self) {
            Ok(v) => {
                self.commit(snapshot);
                Ok(v)
            }

            Err(err) => {
                self.rollback_to(snapshot);
                Err(err)
            }
        }
    }

    /// If type `leaf` is a free inference variable, and that variable has been
    /// bound, returns `Some(T)` where `T` is the type to which it has been bound.
    ///
    /// `binders` is the number of binders under which `leaf` appears;
    /// the return value will also be shifted accordingly so that it
    /// can appear under that same number of binders.
    pub fn normalize_shallow(&mut self, leaf: &Ty, binders: usize) -> Option<Ty> {
        leaf.var().and_then(|depth| {
            if depth < binders {
                None // bound variable, not an inference var
            } else {
                let var = InferenceVariable::from_depth(depth - binders);
                match self.unify.probe_value(var) {
                    InferenceValue::Unbound(_) => None,
                    InferenceValue::Bound(ref val) => {
                        let ty = val.as_ref().ty().unwrap();
                        Some(ty.up_shift(binders))
                    }
                }
            }
        })
    }

    /// If `leaf` represents an inference variable `X`, and `X` is bound,
    /// returns `Some(v)` where `v` is the value to which `X` is bound.
    fn normalize_lifetime(&mut self, leaf: &Lifetime) -> Option<Lifetime> {
        match *leaf {
            Lifetime::Var(v) => self.probe_lifetime_var(InferenceVariable::from_depth(v)),
            Lifetime::ForAll(_) => None,
        }
    }

    /// Finds the type to which `var` is bound, returning `None` if it is not yet
    /// bound.
    ///
    /// # Panics
    ///
    /// This method is only valid for inference variables of kind
    /// type. If this variable is of a different kind, then the
    /// function may panic.
    fn probe_ty_var(&mut self, var: InferenceVariable) -> Option<Ty> {
        match self.unify.probe_value(var) {
            InferenceValue::Unbound(_) => None,
            InferenceValue::Bound(ref val) => Some(val.as_ref().ty().unwrap().clone()),
        }
    }

    /// Finds the lifetime to which `var` is bound, returning `None` if it is not yet
    /// bound.
    ///
    /// # Panics
    ///
    /// This method is only valid for inference variables of kind
    /// lifetime. If this variable is of a different kind, then the function may panic.
    fn probe_lifetime_var(&mut self, var: InferenceVariable) -> Option<Lifetime> {
        match self.unify.probe_value(var) {
            InferenceValue::Unbound(_) => None,
            InferenceValue::Bound(ref val) => Some(val.as_ref().lifetime().unwrap().clone()),
        }
    }

    /// True if the given inference variable is bound to a value.
    pub fn var_is_bound(&mut self, var: InferenceVariable) -> bool {
        match self.unify.probe_value(var) {
            InferenceValue::Unbound(_) => false,
            InferenceValue::Bound(_) => true,
        }
    }

    /// Given an unbound variable, returns its universe.
    ///
    /// # Panics
    ///
    /// Panics if the variable is bound.
    fn universe_of_unbound_var(&mut self, var: InferenceVariable) -> UniverseIndex {
        match self.unify.probe_value(var) {
            InferenceValue::Unbound(ui) => ui,
            InferenceValue::Bound(_) => panic!("var_universe invoked on bound variable"),
        }
    }
}

impl Ty {
    /// If this is a `Ty::Var(d)`, returns `Some(d)` else `None`.
    pub fn var(&self) -> Option<usize> {
        if let Ty::Var(depth) = *self {
            Some(depth)
        } else {
            None
        }
    }

    /// If this is a `Ty::Var`, returns the
    /// `TyInferenceVariable` it represents. Only makes sense if
    /// `self` is known not to appear inside of any binders, since
    /// otherwise the depth would have be adjusted to account for
    /// those binders.
    pub fn inference_var(&self) -> Option<InferenceVariable> {
        self.var().map(InferenceVariable::from_depth)
    }
}

impl Lifetime {
    /// If this is a `Lifetime::Var(d)`, returns `Some(d)` else `None`.
    pub fn var(&self) -> Option<usize> {
        if let Lifetime::Var(depth) = *self {
            Some(depth)
        } else {
            None
        }
    }

    /// If this is a `Lifetime::Var`, returns the
    /// `LifetimeInferenceVariable` it represents. Only makes sense if
    /// `self` is known not to appear inside of any binders, since
    /// otherwise the depth would have be adjusted to account for
    /// those binders.
    pub fn inference_var(&self) -> Option<InferenceVariable> {
        self.var().map(InferenceVariable::from_depth)
    }
}

impl Substitution {
    /// Check whether this substitution is the identity substitution in the
    /// given inference context.
    pub fn is_trivial_within(&self, in_infer: &mut InferenceTable) -> bool {
        for value in self.parameters.values() {
            match value {
                ParameterKind::Ty(ty) => if let Some(var) = ty.inference_var() {
                    if in_infer.var_is_bound(var) {
                        return false;
                    }
                },

                ParameterKind::Lifetime(lifetime) => if let Some(var) = lifetime.inference_var() {
                    if in_infer.var_is_bound(var) {
                        return false;
                    }
                },
            }
        }

        true
    }
}

impl ParameterInferenceVariable {
    pub fn to_parameter(self) -> Parameter {
        match self {
            ParameterKind::Ty(v) => ParameterKind::Ty(v.to_ty()),
            ParameterKind::Lifetime(v) => ParameterKind::Lifetime(v.to_lifetime()),
        }
    }
}
