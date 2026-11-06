//! Scope supplied by an authorized synchronous creation/lifecycle operation.
//! It is a naming hint, never an authority credential or stored membership.
use super::NameScope;
use std::cell::RefCell;

thread_local! { static SCOPE: RefCell<Option<NameScope>> = const { RefCell::new(None) }; }

pub struct ScopeGuard(Option<NameScope>);
impl Drop for ScopeGuard {
    fn drop(&mut self) {
        SCOPE.with(|scope| *scope.borrow_mut() = self.0.take());
    }
}
pub fn enter(scope: NameScope) -> ScopeGuard {
    ScopeGuard(SCOPE.with(|current| current.replace(Some(scope))))
}
pub fn current() -> NameScope {
    SCOPE
        .with(|scope| scope.borrow().clone())
        .unwrap_or(NameScope::Unassigned)
}
