use serde_json::Value;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub(super) struct Table {
    pub directory: PathBuf,
}

impl Table {
    pub fn new(run: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("target/crash-sweep");
        let directory = root.join(format!(
            "{run}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        Self { directory }
    }

    pub fn write(&self, rows: &[Value]) {
        let mut jsonl = std::fs::File::create(self.directory.join("sweep.jsonl.tmp")).unwrap();
        let mut table = String::from(
            "# Crash-Point Sweep\n\nTargets are exact per-store indexed ordinals; cross-store global order is informational.\n\nClasses: COMPLETED_ONCE (one published expected terminal: answer or capacity denial); ABANDONED{claim} (one honest interruption notice); REQUEUED (prompt restored to paused queue); REFUSED{name} (typed exit 5, named signature); NOT_ADMITTED (no durable prompt, clean reopen and exactly one follow-up dispatch).\n",
        );
        class_counts(&mut table, rows);
        refusal_tags(&mut table, rows);
        table.push_str("\n## Cuts\n\n| Shape | Repeat | Cut | Global Cut (Informational) | Commit | Outcome | Invariants |\n| --- | --- | --- | --- | --- | --- | --- |\n");
        for row in rows {
            writeln!(jsonl, "{row}").unwrap();
            writeln!(
                table,
                "| {} | {} | {} | {} | {} | {} | {} |",
                row["shape"].as_str().unwrap(),
                row["repeat"],
                row["cut"].as_str().unwrap(),
                row["global_ordinal"],
                row["commit"].as_str().unwrap_or("pilot"),
                row["outcome"].as_str().unwrap(),
                if row["invariants"] == true {
                    "PASS"
                } else {
                    "FAIL"
                }
            )
            .unwrap();
        }
        jsonl.sync_all().unwrap();
        std::fs::write(self.directory.join("table.md.tmp"), table).unwrap();
        std::fs::rename(
            self.directory.join("sweep.jsonl.tmp"),
            self.directory.join("sweep.jsonl"),
        )
        .unwrap();
        std::fs::rename(
            self.directory.join("table.md.tmp"),
            self.directory.join("table.md"),
        )
        .unwrap();
    }
}

fn class_counts(table: &mut String, rows: &[Value]) {
    let classes = [
        "COMPLETED_ONCE",
        "ABANDONED",
        "REQUEUED",
        "REFUSED",
        "NOT_ADMITTED",
        "UNCLASSIFIED",
        "DIVERGENT",
    ];
    let mut counts = std::collections::BTreeMap::<(String, u64), [usize; 7]>::new();
    for row in rows {
        let outcome = row["outcome"].as_str().unwrap().split('{').next().unwrap();
        let index = classes.iter().position(|class| *class == outcome).unwrap();
        counts
            .entry((
                row["shape"].as_str().unwrap().to_owned(),
                row["repeat"].as_u64().unwrap(),
            ))
            .or_default()[index] += 1;
    }
    table.push_str("\n## Class Counts\n\n| Shape | Repeat | COMPLETED_ONCE | ABANDONED | REQUEUED | REFUSED | NOT_ADMITTED | UNCLASSIFIED | DIVERGENT |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
    for ((shape, repeat), counts) in counts {
        write!(table, "| {shape} | {repeat} |").unwrap();
        for count in counts {
            write!(table, " {count} |").unwrap();
        }
        table.push('\n');
    }
}

fn refusal_tags(table: &mut String, rows: &[Value]) {
    let allowlist: Value = serde_json::from_str(include_str!("refusal_allowlist.json")).unwrap();
    let mut signatures = std::collections::BTreeMap::<&str, Vec<String>>::new();
    for row in rows {
        if let Some(signature) = row["refusal_signature"].as_str() {
            signatures.entry(signature).or_default().push(format!(
                "{}:{}",
                row["repeat"],
                row["cut"].as_str().unwrap()
            ));
        }
    }
    table.push_str("\n## Refused Signatures\n\n| Shape | Signature Name | Note | Repeat:Cut | Post-State Signature |\n| --- | --- | --- | --- | --- |\n");
    for (signature, cuts) in signatures {
        let entry = &allowlist[signature];
        let shape: Value = serde_json::from_str(signature).unwrap();
        writeln!(
            table,
            "| {} | {} | {} | {} | `{}` |",
            shape["shape"].as_str().unwrap(),
            entry["name"].as_str().unwrap_or("not_allowlisted"),
            entry["note"].as_str().unwrap_or("MISSING"),
            cuts.join(", "),
            signature
        )
        .unwrap();
    }
}
