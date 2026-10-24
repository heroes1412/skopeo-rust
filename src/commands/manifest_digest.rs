//! `skopeo manifest-digest`.

use anyhow::Result;
use clap::ArgMatches;

use crate::commands::write_stdout;
use crate::copy::manifest_digest_from_file;

pub fn run(m: &ArgMatches) -> Result<()> {
    let path = m.get_one::<String>("MANIFEST-FILE").expect("required");
    let digest = manifest_digest_from_file(std::path::Path::new(path))?;
    write_stdout(format!("{digest}\n").as_bytes())
}
