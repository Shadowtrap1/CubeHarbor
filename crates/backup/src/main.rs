//! cubeharbor-backup: runs inside backup and restore Jobs. Configured by the operator through
//! environment variables, see `cubeharbor_backup::run::Settings`.

use std::path::Path;

use cubeharbor_backup::run::{self, Report, Settings};

const TERMINATION_LOG: &str = "/dev/termination-log";

fn main() {
    let action = std::env::args().nth(1).unwrap_or_default();
    let result = Settings::from_env().and_then(|s| match action.as_str() {
        "backup" => run::backup(&s),
        "restore" => run::restore(&s),
        "save-on" => run::save_on(&s),
        other => anyhow::bail!("unknown action {other:?}, expected backup, restore or save-on"),
    });
    let report = match &result {
        Ok(r) => r.clone(),
        Err(e) => Report::failure(format!("{e:#}")),
    };
    println!("{}", report.to_json());
    run::write_report(Path::new(TERMINATION_LOG), &report);
    if report.error.is_some() {
        std::process::exit(1);
    }
}
