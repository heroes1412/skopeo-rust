//! skopeo binary (Rust implementation): parse the CLI, dispatch the subcommand, report errors and
//! exit codes the way skopeo does (0 ok, 1 error, 2 image-not-found).

fn main() {
    std::process::exit(real_main());
}

fn real_main() -> i32 {
    let command = skopeo::cli::build_cli();
    let matches = match command.try_get_matches() {
        Ok(matches) => matches,
        Err(err) => {
            // clap would exit 2; skopeo exits 1 for CLI errors.
            let use_stderr = err.use_stderr();
            let _ = err.print();
            return if use_stderr { 1 } else { 0 };
        }
    };

    if matches.get_flag("version") {
        println!(
            "skopeo version {} commit: {} (Rust port, forked from skopeo {})",
            skopeo::VERSION,
            option_env!("SKOPEO_COMMIT").unwrap_or("unknown"),
            skopeo::UPSTREAM_VERSION
        );
        return 0;
    }

    let (name, sub_matches) = match matches.subcommand() {
        Some(pair) => pair,
        None => {
            eprintln!("missing command 'skopeo COMMAND'");
            eprintln!("Try 'skopeo --help' for more information");
            return 1;
        }
    };

    let global = match skopeo::cli::GlobalOptions::from_matches(&matches) {
        Ok(global) => global,
        Err(err) => {
            eprintln!("Error: {err:#}");
            return 1;
        }
    };

    match skopeo::commands::dispatch(name, sub_matches, &global) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("Error: {err:#}");
            if is_not_found_error(&err) {
                2
            } else {
                1
            }
        }
    }
}

/// skopeo exits 2 for "image not known" errors (registry 404s).
fn is_not_found_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(status) = cause.downcast_ref::<skopeo::registry::HttpStatusError>() {
            if status.status == 404 {
                return true;
            }
        }
    }
    let text = err.to_string();
    text.contains("manifest unknown")
        || text.contains("MANIFEST_UNKNOWN")
        || text.contains("manifest does not exist")
        || text.contains("no such image")
}
