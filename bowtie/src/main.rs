//! reldir's row validator, speaking Bowtie's connector protocol on stdin and
//! stdout, so it is measured against the JSON Schema test suite alongside
//! every other implementation (https://bowtie.report).
//!
//! Each case's schema is compiled exactly as reldir compiles a table's
//! schema; a schema reldir's dialect refuses -- an external `$ref`, a
//! keyword the dialect closes -- is reported as an error of the case, with
//! the code reldir refuses it with.

use indexmap::IndexMap;
use reldir_core::schema::row::RowValidator;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

fn start() -> Value {
    json!({
        "version": 1,
        "implementation": {
            "language": "rust",
            "name": "reldir",
            "version": reldir_core::VERSION,
            "homepage": "https://jowharshamshiri.github.io/reldir/",
            "documentation": "https://jowharshamshiri.github.io/reldir/schemas",
            "issues": "https://github.com/jowharshamshiri/reldir/issues",
            "source": "https://github.com/jowharshamshiri/reldir",
            "dialects": [DIALECT],
        }
    })
}

fn run(seq: &Value, case: &Value) -> Value {
    let schema = &case["schema"];
    let validator = match RowValidator::compile(schema, &IndexMap::new()) {
        Ok(validator) => validator,
        Err(diagnostic) => {
            return json!({
                "seq": seq,
                "errored": true,
                "context": {"message": format!("{}: {}", diagnostic.code, diagnostic.message)},
            });
        }
    };
    let results: Vec<Value> = case["tests"]
        .as_array()
        .map(|tests| {
            tests
                .iter()
                .map(|test| json!({"valid": validator.is_valid(&test["instance"])}))
                .collect()
        })
        .unwrap_or_default();
    json!({"seq": seq, "results": results})
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("not a request: {error}");
                std::process::exit(1);
            }
        };
        let reply = match request["cmd"].as_str() {
            Some("start") => start(),
            Some("dialect") => json!({"ok": request["dialect"] == DIALECT}),
            Some("run") => run(&request["seq"], &request["case"]),
            Some("stop") => break,
            other => {
                eprintln!("unknown command {other:?}");
                std::process::exit(1);
            }
        };
        if writeln!(stdout, "{reply}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            break;
        }
    }
}
