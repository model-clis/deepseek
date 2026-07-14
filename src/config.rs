use anyhow::{Context, Result, bail};
use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::{fs, path::PathBuf};

#[derive(Serialize, Deserialize)]
struct Credentials {
    version: u32,
    key: String,
}

fn path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("Unable to determine home directory")?
        .join("model-clis/deepseek/credentials.json"))
}

fn load_key_at(env_key: Option<String>, target: &std::path::Path) -> Result<String> {
    if let Some(key) = env_key.filter(|k| !k.trim().is_empty()) {
        return Ok(key);
    }
    let c: Credentials =
        serde_json::from_slice(&fs::read(target).context("Not logged in; run deepseek login")?)
            .context("Invalid credentials file")?;
    if c.version != 1 || c.key.trim().is_empty() {
        bail!("Invalid credentials file")
    }
    Ok(c.key)
}

pub fn load_key() -> Result<String> {
    load_key_at(std::env::var("DEEPSEEK_API_KEY").ok(), &path()?)
}

pub fn save_key(key: &str) -> Result<()> {
    let target = path()?;
    let parent = target.parent().unwrap();
    fs::create_dir_all(parent)?;
    let file = AtomicWriteFile::options();
    #[cfg(unix)]
    {
        use atomic_write_file::unix::OpenOptionsExt as _;
        file.preserve_mode(false);
        use std::os::unix::fs::OpenOptionsExt as _;
        file.mode(0o600);
    }
    let mut file = file.open(&target)?;
    file.write_all(&serde_json::to_vec(&Credentials {
        version: 1,
        key: key.into(),
    })?)?;
    file.commit()?;
    Ok(())
}

pub fn logout() -> Result<()> {
    let p = path()?;
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
