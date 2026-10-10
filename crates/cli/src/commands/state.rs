// CODEGEN-BEGIN
//! `vat state <id>` — print the full agent-legible [`VatState`] as JSON.
//!
//! This is the command an agent calls to understand a vat. Output is pretty
//! JSON by default (readable in a transcript) or single-line with `--compact`.

use std::process::ExitCode;

use anyhow::Result;

use crate::store;

pub fn exec(id: String, compact: bool) -> Result<ExitCode> {
    if crate::native::container::looks_like_id(&id) {
        // Native containers (`ctr-…`) report through `vat container inspect`.
        let container = crate::native::container::find(&id)?;
        crate::commands::print_json(&container.inspect()?, compact)?;
        return Ok(ExitCode::SUCCESS);
    }
    let vat = store::load(&id)?;
    let state = vat.project()?;
    crate::commands::print_json(&state, compact)?;
    Ok(ExitCode::SUCCESS)
}
// CODEGEN-END
