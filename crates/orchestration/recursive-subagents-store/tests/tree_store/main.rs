//! `SQLite` tree-store integration: indexed commit cuts, view deletion,
//! row-level saves, and the schema-10 rows and custody.

mod adoption_pin_cuts;
mod commit_cut_matrix;
mod conflict_candidates;
mod revert;
mod row_writes;
mod schema_ten;
mod view_deletion;
