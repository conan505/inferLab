use std::{ffi::OsString, path::PathBuf, process::ExitCode};

use model_artifacts::load_pinned_pythia;

const USAGE: &str = "usage: inferlab-model-inspect inspect --lock <lock.json> --assets <directory>";

struct Arguments {
    lock: PathBuf,
    assets: PathBuf,
}

fn parse_arguments(arguments: impl IntoIterator<Item = OsString>) -> Result<Arguments, ()> {
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("inspect"))
        || arguments.next().as_deref() != Some(std::ffi::OsStr::new("--lock"))
    {
        return Err(());
    }
    let lock = arguments.next().map(PathBuf::from).ok_or(())?;
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--assets")) {
        return Err(());
    }
    let assets = arguments.next().map(PathBuf::from).ok_or(())?;
    if arguments.next().is_some() {
        return Err(());
    }
    Ok(Arguments { lock, assets })
}

fn main() -> ExitCode {
    let arguments = match parse_arguments(std::env::args_os()) {
        Ok(arguments) => arguments,
        Err(()) => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let bundle = match load_pinned_pythia(&arguments.lock, &arguments.assets) {
        Ok(bundle) => bundle,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match serde_json::to_string_pretty(bundle.report()) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(_) => {
            eprintln!("model artifact verification failed: report_encoding_failed");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{USAGE, parse_arguments};

    #[test]
    fn accepts_only_the_documented_offline_inspect_shape() {
        let arguments = parse_arguments(
            [
                "inferlab-model-inspect",
                "inspect",
                "--lock",
                "lock.json",
                "--assets",
                "assets",
            ]
            .map(Into::into),
        )
        .expect("documented arguments");
        assert_eq!(arguments.lock.to_string_lossy(), "lock.json");
        assert_eq!(arguments.assets.to_string_lossy(), "assets");
    }

    #[test]
    fn rejects_network_or_ambiguous_cli_shapes() {
        for arguments in [
            vec!["inferlab-model-inspect", "fetch"],
            vec!["inferlab-model-inspect", "inspect", "--lock", "lock.json"],
            vec![
                "inferlab-model-inspect",
                "inspect",
                "--lock",
                "lock.json",
                "--assets",
                "assets",
                "extra",
            ],
        ] {
            assert!(parse_arguments(arguments.into_iter().map(Into::into)).is_err());
        }
        assert!(USAGE.starts_with("usage: inferlab-model-inspect inspect"));
    }
}
