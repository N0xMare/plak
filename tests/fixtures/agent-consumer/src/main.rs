//! Exact supported inventory and workflows for the curated agent component.
//!
//! Public implementation-tier nameability outside these modules does not add
//! an out-of-tree support promise.

mod control;
mod observation;
mod profiles;

fn main() {
    control::surface_is_nameable();
    observation::surface_is_nameable();
    profiles::surface_is_nameable();
}
