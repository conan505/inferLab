use std::process::ExitCode;

use control_plane::figure_eight_safety_report;

fn main() -> ExitCode {
    let report = figure_eight_safety_report();
    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("serialize Figure-8 safety report: {error}");
            return ExitCode::FAILURE;
        }
    }
    if report.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
