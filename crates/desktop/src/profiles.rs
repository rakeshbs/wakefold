//! Non-secret connection metadata. Tokens live in the platform credential store.
use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{fs, io::ErrorKind, path::Path};

pub const LOCAL: &str = "http://127.0.0.1:8700/";
#[derive(Clone, Deserialize, Serialize)]
pub struct Profile {
    pub name: String,
    pub url: String,
}
impl Profile {
    pub fn local() -> Self {
        Self {
            name: if cfg!(target_os = "macos") {
                "This Mac"
            } else {
                "This computer"
            }
            .into(),
            url: LOCAL.into(),
        }
    }
    pub fn remote(name: &str, address: &str) -> Result<Self> {
        let name = name.trim();
        ensure!(!name.is_empty(), "Enter a server name.");
        let url = Url::parse(address.trim()).context("Enter an HTTPS server address.")?;
        ensure!(
            url.scheme() == "https"
                && url.host().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "Use an HTTPS address with a host and optional port."
        );
        Ok(Self {
            name: name.into(),
            url: url.to_string(),
        })
    }
    pub fn credential_key(&self) -> String {
        format!("wakefold-desktop:{}", self.url)
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Profiles {
    pub servers: Vec<Profile>,
    pub selected: Option<String>,
}
impl Profiles {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let mut saved: Self =
            serde_json::from_slice(&bytes).context("Could not read saved server settings.")?;
        for p in &mut saved.servers {
            *p = Profile::remote(&p.name, &p.url)?;
        }
        Ok(saved)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let directory = path.parent().context("Invalid settings path")?;
        fs::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer_pretty(file.as_file_mut(), self)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }
    pub fn remember(&mut self, profile: Profile) {
        self.selected = Some(profile.url.clone());
        if profile.url == LOCAL {
            return;
        }
        if let Some(entry) = self.servers.iter_mut().find(|s| s.url == profile.url) {
            *entry = profile;
        } else {
            self.servers.push(profile);
        }
    }
}
