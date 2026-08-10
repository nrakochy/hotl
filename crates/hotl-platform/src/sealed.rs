//! Sealing, so a trait doc can make absolute statements about *all*
//! implementors.
//!
//! Every capability trait in this crate takes `Sealed` as a supertrait. A
//! downstream crate must not be able to add an adapter that bypasses an
//! invariant — `DirHandle`'s "no method accepts an absolute path" guarantee is
//! only worth stating because the set of implementors is closed.

pub trait Sealed {}
