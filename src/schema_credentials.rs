//! The credentials a private schema URL is read with: GitHub's tokens, in the
//! order the `gh` command reads them, and `.netrc` entries by host.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

const GITHUB_API_HOST: &str = "api.github.com";
const GH_LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
const GH_LOGIN_POLL: Duration = Duration::from_millis(20);

/// A credential, where it came from, and how its owner renews it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    /// Where the credential came from, as a report names it, such as `GH_TOKEN`.
    pub origin: String,
    pub secret: Secret,
    /// What renews a credential the server rejects.
    pub fix: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Secret {
    Bearer(String),
    Basic { login: String, password: String },
}

impl Credential {
    /// The `Authorization` header value the credential is sent as.
    pub fn authorization(&self) -> String {
        match &self.secret {
            Secret::Bearer(token) => format!("Bearer {token}"),
            Secret::Basic { login, password } => {
                format!("Basic {}", base64(format!("{login}:{password}").as_bytes()))
            }
        }
    }
}

/// Where a schema URL's credentials come from.
pub trait CredentialSource: Send + Sync {
    /// GitHub's tokens, in the order they are tried.
    fn github_tokens(&self) -> Vec<Credential>;
    /// The login `.netrc` gives `host`.
    fn host_login(&self, host: &str) -> Option<Credential>;
}

/// The machine's credentials: `GH_TOKEN`, `GITHUB_TOKEN`, the `gh` command's
/// login, and `.netrc`, each read once, when first needed.
#[derive(Default)]
pub struct MachineCredentials {
    github: OnceLock<Vec<Credential>>,
    netrc: OnceLock<Option<String>>,
}

impl CredentialSource for MachineCredentials {
    fn github_tokens(&self) -> Vec<Credential> {
        self.github
            .get_or_init(|| {
                let mut tokens = Vec::new();
                for variable in ["GH_TOKEN", "GITHUB_TOKEN"] {
                    if let Some(token) = std::env::var(variable)
                        .ok()
                        .filter(|token| !token.is_empty())
                    {
                        tokens.push(Credential {
                            origin: format!("`{variable}`"),
                            secret: Secret::Bearer(token),
                            fix: format!("replace `{variable}`"),
                        });
                    }
                }
                if let Some(token) = gh_login_token() {
                    tokens.push(Credential {
                        origin: "`gh auth token`".to_owned(),
                        secret: Secret::Bearer(token),
                        fix: "run `gh auth login`".to_owned(),
                    });
                }
                tokens.extend(self.host_login(GITHUB_API_HOST));
                tokens
            })
            .clone()
    }

    fn host_login(&self, host: &str) -> Option<Credential> {
        let text = self
            .netrc
            .get_or_init(|| netrc_path().and_then(|path| std::fs::read_to_string(path).ok()))
            .as_deref()?;
        let (login, password) = netrc_login(text, host)?;
        Some(Credential {
            origin: format!("the `.netrc` entry for {host}"),
            secret: Secret::Basic { login, password },
            fix: format!("update the `.netrc` entry for {host}"),
        })
    }
}

/// The token of the account `gh auth login` logged in to github.com, where the
/// `gh` command is installed and answers within its time.
fn gh_login_token() -> Option<String> {
    let mut child = Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + GH_LOGIN_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => thread::sleep(GH_LOGIN_POLL),
        }
    }
    let mut token = String::new();
    child.stdout.take()?.read_to_string(&mut token).ok()?;
    let token = token.trim().to_owned();
    (!token.is_empty()).then_some(token)
}

fn netrc_path() -> Option<PathBuf> {
    match std::env::var_os("NETRC") {
        Some(path) => Some(PathBuf::from(path)),
        None => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".netrc")),
    }
}

/// The login and password a `.netrc` gives `host`: its `machine` entry, else its
/// `default` entry.
fn netrc_login(text: &str, host: &str) -> Option<(String, String)> {
    let mut words = text.split_whitespace();
    let mut found: Option<(String, String)> = None;
    let mut default: Option<(String, String)> = None;
    let mut current: Option<(bool, bool)> = None;
    let mut entry = (String::new(), String::new());
    let mut finish = |current: Option<(bool, bool)>, entry: &mut (String, String)| {
        if let Some((is_host, is_default)) = current {
            let taken = std::mem::take(entry);
            if is_host && found.is_none() {
                found = Some(taken.clone());
            }
            if is_default && default.is_none() {
                default = Some(taken);
            }
        }
    };
    while let Some(word) = words.next() {
        match word {
            "machine" => {
                finish(current, &mut entry);
                current = Some((words.next() == Some(host), false));
            }
            "default" => {
                finish(current, &mut entry);
                current = Some((false, true));
            }
            "login" => entry.0 = words.next().unwrap_or_default().to_owned(),
            "password" => entry.1 = words.next().unwrap_or_default().to_owned(),
            "account" => {
                words.next();
            }
            "macdef" => {
                finish(current, &mut entry);
                current = None;
                words.next();
            }
            _ => {}
        }
    }
    finish(current, &mut entry);
    found.or(default)
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let joined = chunk
            .iter()
            .enumerate()
            .fold(0u32, |joined, (index, byte)| {
                joined | u32::from(*byte) << (16 - 8 * index)
            });
        for position in 0..4 {
            if position <= chunk.len() {
                encoded.push(ALPHABET[(joined >> (18 - 6 * position) & 0x3f) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_netrc_gives_a_host_its_machine_entry_else_the_default() {
        let text = concat!(
            "machine mock.example login mock-user password mock-secret\n",
            "machine api.github.com\n  login mock-owner\n  password mock-token\n",
            "default login anyone password any-secret\n",
        );
        assert_eq!(
            netrc_login(text, "api.github.com"),
            Some(("mock-owner".into(), "mock-token".into()))
        );
        assert_eq!(
            netrc_login(text, "other.example"),
            Some(("anyone".into(), "any-secret".into()))
        );
        assert_eq!(
            netrc_login("machine a.example login x password y", "b.example"),
            None
        );
    }

    #[test]
    fn a_basic_login_is_sent_base64_encoded() {
        let credential = Credential {
            origin: "mock".into(),
            secret: Secret::Basic {
                login: "mock-user".into(),
                password: "mock-secret".into(),
            },
            fix: String::new(),
        };
        assert_eq!(
            credential.authorization(),
            "Basic bW9jay11c2VyOm1vY2stc2VjcmV0"
        );
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
    }
}
