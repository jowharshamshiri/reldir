//! Prompts: the ways of working with a database an agent is offered, each a
//! message that sets the task and names the tools that do it.

use reldir_core::diagnostic::{DbError, Result};
use rmcp::model::{GetPromptResult, Prompt, PromptArgument, PromptMessage, Role};
use serde_json::{Map, Value};

pub fn list() -> Vec<Prompt> {
    vec![
        Prompt::new(
            "explore-database",
            Some(
                "Learn what the database holds: its tables, their keys and references, and their data.",
            ),
            None,
        ),
        Prompt::new(
            "design-schema",
            Some(
                "Design a new table, or a change to one, and make it through validated migrations.",
            ),
            Some(vec![
                PromptArgument::new("table")
                    .with_description("The table to design or change")
                    .with_required(true),
                PromptArgument::new("purpose")
                    .with_description("What the table is to hold")
                    .with_required(false),
            ]),
        ),
        Prompt::new(
            "repair-database",
            Some(
                "Find every fault in the database and repair it, asking before anything is removed.",
            ),
            None,
        ),
    ]
}

pub fn get(name: &str, arguments: Map<String, Value>) -> Result<GetPromptResult> {
    let argument = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let text = match name {
        "explore-database" => "Explore this reldir database before doing anything else. Call \
             `status` to learn whether it is valid, then `tables` for its tables and row counts. \
             For each table that matters, `describe` it -- columns, key, references in and out -- \
             and read a few rows with `list_rows` or `query`. Read reldir://docs/concepts for how \
             a folder of JSON files is a database. Summarise the tables, how they relate, and \
             anything `status` or `check` reports as wrong. Change nothing."
            .to_string(),
        "design-schema" => {
            let table = argument("table")
                .ok_or_else(|| DbError::usage("design-schema needs the table it designs"))?;
            let purpose = argument("purpose")
                .map(|purpose| format!(" It is to hold: {purpose}."))
                .unwrap_or_default();
            format!(
                "Design the table `{table}` in this reldir database.{purpose} First read \
                 reldir://docs/schemas, call `tables`, and `describe` the tables `{table}` will \
                 reference or be referenced by. If `{table}` does not exist, declare it with \
                 `schema_new`; change it with `migrate` (add_column, add_constraint, ...). Every \
                 change is planned first: show the user the plan -- the files it touches, and \
                 whatever it removes -- before committing it with its plan_id. Prefer a \
                 constraint the data already satisfies; `lint` suggests them."
            )
        }
        "repair-database" => "Repair this reldir database. Call `check` for every fault, with \
             its file, line and JSON Pointer, then `doctor_plan` for the repairs that apply, least \
             destructive first; `doctor_explain` says what a repair does. Apply repairs with \
             `apply_fixes`: it plans first, and its plan lists every file it would change. Show \
             the user any plan that removes data before committing it, and commit nothing they \
             have not agreed to. Call `check` again to confirm the database is valid."
            .to_string(),
        other => return Err(DbError::usage(format!("there is no prompt {other:?}"))),
    };
    Ok(GetPromptResult::new(vec![PromptMessage::new_text(
        Role::User,
        text,
    )]))
}
