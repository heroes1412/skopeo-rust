//! `skopeo login`.

use std::io::{BufRead, Read, Write};

use anyhow::{bail, Context, Result};
use clap::ArgMatches;

use crate::auth::{self, AuthFile};
use crate::options::AuthOptions;
use crate::cli::GlobalOptions;
use crate::options::ImageOptions;
use crate::registry;

pub fn run(m: &ArgMatches, global: &GlobalOptions) -> Result<()> {
    let authfile = m
        .get_one::<String>("authfile")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    let compat_auth_file = m
        .get_one::<String>("compat-auth-file")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    if authfile.is_some() && compat_auth_file.is_some() {
        bail!("options for paths to the credential file and to the Docker-compatible credential file can not be set simultaneously");
    }
    let cert_dir = m
        .get_one::<String>("cert-dir")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    let tls_verify = m.get_one::<bool>("tls-verify").copied();
    let get_login = m.get_flag("get-login");
    let verbose = m.get_flag("verbose");

    let positional: Vec<String> = m
        .get_many::<String>("REGISTRY")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    if positional.is_empty() {
        bail!("please provide a registry to log in to");
    }
    if positional.len() > 1 {
        bail!("login accepts only one registry to log in to");
    }
    // `login` accepts registries, namespaces and repositories; the full
    // argument (scheme stripped) is the credentials key.
    let registry_key = auth::parse_credentials_key(&positional[0])?;

    let auth_opts = AuthOptions {
        authfile: authfile.clone(),
        ..Default::default()
    };

    if get_login {
        let user = auth::get_login(&registry_key, &auth_opts)?;
        println!("{user}");
        return Ok(());
    }

    let mut image_opts = ImageOptions::new();
    image_opts.debug = global.debug;
    image_opts.timeout = global.timeout;
    image_opts.cert_dir = cert_dir;
    image_opts.tls_verify = tls_verify;

    let stored = AuthFile::load(authfile.as_deref())
        .ok()
        .and_then(|file| file.lookup(&registry_key));

    // Go refuses to proceed when the stored login is an identity token.
    if let Some(stored) = &stored {
        if stored.identity_token.is_some() {
            bail!("currently logged in, auth file contains an Identity token");
        }
    }

    let username_flag = m.get_one::<String>("username").filter(|v| !v.is_empty());
    let password_flag = m.get_one::<String>("password").filter(|v| !v.is_empty());
    let password_stdin = m.get_flag("password-stdin");

    if password_flag.is_some() && password_stdin {
        bail!("can't specify both --password-stdin and --password");
    }
    if password_stdin && username_flag.is_none() {
        bail!("must provide --username with --password-stdin");
    }

    // With neither -u nor a password source, try the stored credentials
    // first (matching auth.Login's flow).
    if username_flag.is_none() && password_flag.is_none() && !password_stdin {
        if let Some(stored) = &stored {
            if !stored.password.is_empty() {
                println!("Authenticating with existing credentials for {registry_key}");
                if registry::try_login(
                    &registry_key,
                    &stored.username,
                    &stored.password,
                    &image_opts,
                )
                .is_ok()
                {
                    println!(
                        "Existing credentials are valid. Already logged in to {registry_key}"
                    );
                    return Ok(());
                }
                println!("Existing credentials are invalid, please enter valid username and password");
            }
        }
    }

    let stored_username = stored
        .as_ref()
        .map(|s| s.username.clone())
        .unwrap_or_default();
    let username = match username_flag {
        Some(user) => user.clone(),
        None => {
            if stored_username.is_empty() {
                print!("Username: ");
            } else {
                print!("Username ({stored_username}): ");
            }
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                if stored_username.is_empty() {
                    bail!("username required");
                }
                stored_username.clone()
            } else {
                trimmed
            }
        }
    };
    if username.is_empty() {
        bail!("username required");
    }
    let password = if let Some(pass) = password_flag {
        pass.clone()
    } else if password_stdin {
        let mut buffer = String::new();
        std::io::stdin().lock().read_to_string(&mut buffer)?;
        buffer.trim_end_matches(['\r', '\n']).to_string()
    } else {
        print!("Password: ");
        std::io::stdout().flush()?;
        rpassword::read_password().context("reading password")?
    };
    if password.is_empty() {
        bail!("password required");
    }

    if verbose {
        println!("Used:  {registry_key}");
    }
    registry::try_login(&registry_key, &username, &password, &image_opts)?;

    if let Some(compat_path) = &compat_auth_file {
        // Docker-compatible config: docker.io is keyed on the legacy
        // `https://index.docker.io/v1/` spelling.
        let key = match registry_key.as_str() {
            "docker.io" | "index.docker.io" | "registry-1.docker.io" => {
                "https://index.docker.io/v1/".to_string()
            }
            other => other.to_string(),
        };
        let mut file = AuthFile::load(Some(compat_path))?;
        file.set(&key, &username, &password, None);
        file.save().context("writing credentials to auth file")?;
    } else {
        let mut file = AuthFile::load(authfile.as_deref())?;
        file.set(&registry_key, &username, &password, None);
        file.save().context("writing credentials to auth file")?;
    }
    println!("Login Succeeded!");
    Ok(())
}
