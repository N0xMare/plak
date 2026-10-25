//! Live checkpoint authority, publication candidates and bounded storage IO.

pub(in crate::runtime) mod identity;

pub(in crate::runtime) mod candidate;

pub(in crate::runtime) mod owner;

#[cfg(test)]
mod capacity_tests;
pub(in crate::runtime) mod io;
pub(crate) mod storage_pause;
