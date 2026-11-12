//! `SQLite` tree-store integration: indexed commit cuts, view deletion,
//! row-level saves, the schema-10 rows and custody, and the custody law both
//! tree stores share.

mod adoption_pin_cuts;
mod commit_cut_matrix;
mod conflict_candidates;
mod custody_conformance;
mod handoff_candidates;
mod revert;
mod row_writes;
mod schema_ten;
mod view_deletion;
