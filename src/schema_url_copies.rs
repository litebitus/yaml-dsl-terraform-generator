//! Schemas read from URLs, and the copies kept of them, by the editor's rules:
//! `http(s)://` links, GitHub release assets, and `s3://` objects through the AWS
//! command line, as a role the link's `assume_role` names where it names one.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::schema_credentials::{Credential, CredentialSource, MachineCredentials};

const EDITOR_PARAMETER_PREFIX: &str = "yaml_dsl_";
const MAX_AGE_PARAMETER: &str = "yaml_dsl_max_age_days";
const MACHINE_CREDENTIALS_PARAMETER: &str = "yaml_dsl_machine_credentials";
const DAY_SECONDS: u64 = 24 * 60 * 60;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_OK: u16 = 200;
const STATUS_NOT_MODIFIED: u16 = 304;
const STATUS_UNAUTHORIZED: u16 = 401;
const STATUS_FORBIDDEN: u16 = 403;
const STATUS_NOT_FOUND: u16 = 404;
const GITHUB_API: &str = "https://api.github.com";
/// Where a GitHub link starts, before `<owner>/<repo>/releases/`.
const GITHUB_LINK_PREFIXES: [&str; 2] = ["https://github.com/", "https://www.github.com/"];
const S3_PREFIX: &str = "s3://";
const AWS_COMMAND: &str = "aws";
const AWS_TIMEOUT: Duration = Duration::from_secs(60);
/// The parameter of an S3 link naming the role its object is read as.
const ASSUME_ROLE_PARAMETER: &str = "assume_role";
const ROLE_SESSION_NAME: &str = "tfgen";
const COMMAND_POLL: Duration = Duration::from_millis(20);
const COPY_EXTENSION: &str = "json";

/// Whether a schema location is a URL rather than a path.
pub fn is_schema_url(location: &str) -> bool {
    ["http://", "https://", S3_PREFIX]
        .iter()
        .any(|scheme| location.starts_with(scheme))
}

/// Where the copies of schema URLs are kept: the user's cache folder, across runs.
pub fn schema_url_copies_directory() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let cache = match std::env::var_os("XDG_CACHE_HOME") {
        Some(cache) => PathBuf::from(cache),
        None if cfg!(target_os = "macos") => home.join("Library/Caches"),
        None => home.join(".cache"),
    };
    cache.join("tfgen/schema-urls")
}

/// Downloads schema URLs and keeps a copy of each in a folder the caller names,
/// up to its capacity, the least recently used dropped first.
pub struct SchemaUrlCopies {
    directory: PathBuf,
    capacity: usize,
    agent: ureq::Agent,
    credentials: Box<dyn CredentialSource>,
    github_api: String,
    /// Where a GitHub link starts, before `<owner>/<repo>/releases/`.
    github_link_prefixes: Vec<String>,
    /// The AWS command line `s3://` objects are read through.
    aws_command: PathBuf,
    /// The credentials of each role assumed this run, by role.
    assumed_roles: Mutex<BTreeMap<String, RoleCredentials>>,
}

/// The temporary credentials an assumed role is read as.
#[derive(Clone)]
struct RoleCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
}

/// An `s3://<bucket>/<key>` link: where its object is, and the role it is read as.
struct S3Object<'u> {
    bucket: &'u str,
    key: &'u str,
    assume_role: Option<&'u str>,
}

impl<'u> S3Object<'u> {
    fn parse(url: &'u str) -> Result<Self, String> {
        let link = url.strip_prefix(S3_PREFIX).unwrap_or(url);
        let (path, query) = match link.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (link, None),
        };
        let (bucket, key) = path
            .split_once('/')
            .filter(|(bucket, key)| !bucket.is_empty() && !key.is_empty())
            .ok_or_else(|| "an S3 link is `s3://<bucket>/<key>`".to_owned())?;
        let mut assume_role = None;
        for parameter in query.into_iter().flat_map(|query| query.split('&')) {
            match parameter.split_once('=') {
                Some((ASSUME_ROLE_PARAMETER, role)) if !role.is_empty() => {
                    assume_role = Some(role);
                }
                _ => {
                    return Err(format!(
                        "`{parameter}` is not a parameter an S3 link takes; it takes \
                         `{ASSUME_ROLE_PARAMETER}=<role ARN>`"
                    ));
                }
            }
        }
        Ok(Self {
            bucket,
            key,
            assume_role,
        })
    }
}

/// A schema's text, and what a report should warn of about how it was read.
#[derive(Debug, PartialEq, Eq)]
pub struct SchemaText {
    pub text: String,
    pub warnings: Vec<String>,
}

/// A URL as fetched, without the editor's parameters, the max age it gives, and
/// whether it opts in to the machine's credentials.
#[derive(Debug, PartialEq, Eq)]
struct EditorParameters {
    fetch_url: String,
    max_age_days: u64,
    machine_credentials: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct KeptCopy {
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    /// When the server last sent or confirmed the copy, in seconds since the epoch.
    confirmed_at: u64,
    body: String,
}

/// A GitHub link under `<owner>/<repo>/releases/`: its repository, and the
/// release and asset its path reads, where it reads one.
#[derive(Debug, PartialEq, Eq)]
struct GithubRelease {
    owner: String,
    repository: String,
    asset: Option<ReleaseAsset>,
}

/// A release asset a GitHub link's path reads.
#[derive(Debug, PartialEq, Eq)]
struct ReleaseAsset {
    release: ReleaseName,
    asset: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ReleaseName {
    Latest,
    Tag(String),
}

impl ReleaseName {
    /// The path of the release under the repository's `releases` in GitHub's API.
    fn api_path(&self) -> String {
        match self {
            Self::Latest => "latest".to_owned(),
            Self::Tag(tag) => format!("tags/{}", tag.replace('/', "%2F")),
        }
    }

    /// The release as a report names it.
    fn shown(&self) -> String {
        match self {
            Self::Latest => "the latest release".to_owned(),
            Self::Tag(tag) => format!("release `{tag}`"),
        }
    }
}

/// A credential a source refused: what it answered, and what renews it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub reason: String,
    pub fix: String,
}

/// Why a schema URL was not read: what failed, the source's own reason, and what
/// fixes it, for a report to place where the URL was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFailure {
    pub message: String,
    pub reason: Option<String>,
    pub help: Option<String>,
}

impl ReadFailure {
    fn of(message: String) -> Self {
        Self {
            message,
            reason: None,
            help: None,
        }
    }
}

/// The reasons of several refusals, and their fixes once each.
fn refusal_notes(refusals: &[Refusal]) -> (String, String) {
    let reasons: Vec<&str> = refusals
        .iter()
        .map(|refusal| refusal.reason.as_str())
        .collect();
    let mut fixes: Vec<&str> = Vec::new();
    for refusal in refusals {
        if !fixes.contains(&refusal.fix.as_str()) {
            fixes.push(&refusal.fix);
        }
    }
    (reasons.join(", "), fixes.join("; "))
}

/// What asking the server came to.
enum Answer {
    Sent {
        etag: Option<String>,
        last_modified: Option<String>,
        body: String,
    },
    Confirmed,
    /// Every credential tried was refused, by the source that refused them.
    Refused {
        refuser: String,
        refusals: Vec<Refusal>,
    },
    /// The server did not answer, or answered with something else.
    Failed(String),
}

impl SchemaUrlCopies {
    pub fn new(directory: impl Into<PathBuf>, capacity: usize) -> Self {
        Self::with_credentials(
            directory,
            capacity,
            Box::new(MachineCredentials::default()),
            GITHUB_API,
        )
    }

    fn with_credentials(
        directory: impl Into<PathBuf>,
        capacity: usize,
        credentials: Box<dyn CredentialSource>,
        github_api: &str,
    ) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .into();
        Self {
            directory: directory.into(),
            capacity,
            agent,
            credentials,
            github_api: github_api.to_owned(),
            github_link_prefixes: GITHUB_LINK_PREFIXES.map(str::to_owned).to_vec(),
            aws_command: PathBuf::from(AWS_COMMAND),
            assumed_roles: Mutex::new(BTreeMap::new()),
        }
    }

    /// The schema text at `url`: the kept copy while it is younger than its max
    /// age, else what the server sends or confirms, else the kept copy when the
    /// server does not answer or refuses every credential, with a warning of each
    /// refusal.
    pub fn read(&self, url: &str) -> Result<SchemaText, ReadFailure> {
        let parameters = editor_parameters(url).map_err(ReadFailure::of)?;
        let copy_path = self.copy_path(&parameters.fetch_url);
        let kept: Option<KeptCopy> = fs::read_to_string(&copy_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        let now = seconds_now();
        if let Some(kept) = &kept
            && parameters.max_age_days > 0
            && now.saturating_sub(kept.confirmed_at) < parameters.max_age_days * DAY_SECONDS
        {
            mark_used(&copy_path);
            return Ok(SchemaText {
                text: kept.body.clone(),
                warnings: Vec::new(),
            });
        }
        let (answer, mut warnings) = if parameters.fetch_url.starts_with(S3_PREFIX) {
            (
                self.s3_answer(&parameters.fetch_url, &copy_path, kept.as_ref()),
                Vec::new(),
            )
        } else {
            self.link_answer(
                &parameters.fetch_url,
                parameters.machine_credentials,
                kept.as_ref(),
            )
        };
        let copy = match (answer, kept) {
            (
                Answer::Sent {
                    etag,
                    last_modified,
                    body,
                },
                _,
            ) => KeptCopy {
                url: parameters.fetch_url.clone(),
                etag,
                last_modified,
                confirmed_at: now,
                body,
            },
            (Answer::Confirmed, Some(kept)) => KeptCopy {
                confirmed_at: now,
                ..kept
            },
            (Answer::Refused { refuser, refusals }, Some(kept)) => {
                let (reasons, fixes) = refusal_notes(&refusals);
                warnings.push(format!(
                    "{refuser} refused to read {url}, so its kept copy was read \
                     ({reasons}): {fixes}"
                ));
                mark_used(&copy_path);
                return Ok(SchemaText {
                    text: kept.body,
                    warnings,
                });
            }
            (Answer::Failed(_), Some(kept)) => {
                mark_used(&copy_path);
                return Ok(SchemaText {
                    text: kept.body,
                    warnings,
                });
            }
            (Answer::Refused { refuser, refusals }, None) => {
                let (reasons, fixes) = refusal_notes(&refusals);
                return Err(ReadFailure {
                    message: format!("{refuser} refused to read {url}"),
                    reason: Some(reasons),
                    help: Some(fixes),
                });
            }
            (Answer::Failed(reason), None) => {
                return Err(ReadFailure {
                    message: format!("reading {url} failed"),
                    reason: Some(reason),
                    help: None,
                });
            }
            (Answer::Confirmed, None) => {
                return Err(ReadFailure::of(format!(
                    "reading {url} failed: the server confirmed a copy never kept"
                )));
            }
        };
        self.keep(&copy_path, &copy);
        Ok(SchemaText {
            text: copy.body,
            warnings,
        })
    }

    /// A link read as written, without credentials; and, where it opts in to the
    /// machine's credentials and that read is refused, read again with them.
    fn link_answer(
        &self,
        url: &str,
        machine_credentials: bool,
        kept: Option<&KeptCopy>,
    ) -> (Answer, Vec<String>) {
        let response = match conditional(self.agent.get(url), kept).call() {
            Ok(response) => response,
            Err(error) => return (Answer::Failed(error.to_string()), Vec::new()),
        };
        let status = response.status().as_u16();
        let release = self.github_release(url);
        let refused = matches!(status, STATUS_UNAUTHORIZED | STATUS_FORBIDDEN)
            || (release.is_some() && status == STATUS_NOT_FOUND);
        if !machine_credentials || !refused {
            return (sent_answer(response), Vec::new());
        }
        let credentialed = match &release {
            Some(GithubRelease {
                owner,
                repository,
                asset: Some(asset),
            }) => {
                let tokens = self.credentials.github_tokens();
                (!tokens.is_empty())
                    .then(|| self.release_answer(owner, repository, asset, &tokens, kept))
            }
            Some(_) => None,
            None => self
                .login_answer(url, kept)
                .map(|answer| (answer, Vec::new())),
        };
        credentialed.unwrap_or_else(|| (sent_answer(response), Vec::new()))
    }

    /// A link read with the `.netrc` login of its host; none where it has none.
    fn login_answer(&self, url: &str, kept: Option<&KeptCopy>) -> Option<Answer> {
        let host = url_host(url)?;
        let login = self.credentials.host_login(host)?;
        let request =
            conditional(self.agent.get(url), kept).header("Authorization", login.authorization());
        Some(match request.call() {
            Ok(response) => match response.status().as_u16() {
                status @ (STATUS_UNAUTHORIZED | STATUS_FORBIDDEN) => Answer::Refused {
                    refuser: host.to_owned(),
                    refusals: vec![refusal(&login, status)],
                },
                _ => sent_answer(response),
            },
            Err(error) => Answer::Failed(error.to_string()),
        })
    }

    /// The GitHub link `url` is, where it is one under `<owner>/<repo>/releases/`.
    fn github_release(&self, url: &str) -> Option<GithubRelease> {
        self.github_link_prefixes
            .iter()
            .find_map(|prefix| url.strip_prefix(prefix.as_str()))
            .and_then(github_release)
    }

    /// A release asset read through GitHub's API, each token tried in turn until
    /// one is let in; the warnings name every token refused before it. When none
    /// reads it, the answer says what is known: a release or asset the repository
    /// lacks, a repository no token can see, or every token refused.
    fn release_answer(
        &self,
        owner: &str,
        repository: &str,
        asset: &ReleaseAsset,
        tokens: &[Credential],
        kept: Option<&KeptCopy>,
    ) -> (Answer, Vec<String>) {
        let warnings_of = |refusals: &[Refusal]| -> Vec<String> {
            refusals
                .iter()
                .map(|refusal| format!("GitHub refused {}: {}", refusal.reason, refusal.fix))
                .collect()
        };
        let repository_name = format!("`{owner}/{repository}`");
        let repository_url = format!("{}/repos/{owner}/{repository}", self.github_api);
        let mut refusals = Vec::new();
        let mut missing = None;
        for token in tokens {
            let release_url = format!("{repository_url}/releases/{}", asset.release.api_path());
            let release = self
                .agent
                .get(&release_url)
                .header("Authorization", token.authorization())
                .header("Accept", "application/vnd.github+json")
                .call();
            let mut release = match release {
                Ok(release) => release,
                Err(error) => return (Answer::Failed(error.to_string()), warnings_of(&refusals)),
            };
            let status = release.status().as_u16();
            if matches!(status, STATUS_UNAUTHORIZED | STATUS_FORBIDDEN) {
                refusals.push(refusal(token, status));
                continue;
            }
            if status != STATUS_OK {
                let seen = self
                    .agent
                    .get(&repository_url)
                    .header("Authorization", token.authorization())
                    .header("Accept", "application/vnd.github+json")
                    .call()
                    .is_ok_and(|repository| repository.status().as_u16() == STATUS_OK);
                if seen {
                    missing = Some(format!(
                        "{repository_name} has no {}",
                        asset.release.shown()
                    ));
                    break;
                }
                continue;
            }
            let listing: serde_json::Value = match release
                .body_mut()
                .read_to_string()
                .map_err(|error| error.to_string())
                .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
            {
                Ok(listing) => listing,
                Err(error) => return (Answer::Failed(error), warnings_of(&refusals)),
            };
            let Some(asset_url) = listing["assets"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|listed| listed["name"] == asset.asset.as_str())
                .and_then(|listed| listed["url"].as_str())
            else {
                missing = Some(format!(
                    "{} of {repository_name} has no asset `{}`",
                    asset.release.shown(),
                    asset.asset
                ));
                break;
            };
            let download = conditional(self.agent.get(asset_url), kept)
                .header("Authorization", token.authorization())
                .header("Accept", "application/octet-stream")
                .call();
            match download {
                Ok(response)
                    if matches!(
                        response.status().as_u16(),
                        STATUS_UNAUTHORIZED | STATUS_FORBIDDEN
                    ) =>
                {
                    refusals.push(refusal(token, response.status().as_u16()));
                }
                Ok(response) => return (sent_answer(response), warnings_of(&refusals)),
                Err(error) => return (Answer::Failed(error.to_string()), warnings_of(&refusals)),
            }
        }
        if let Some(missing) = missing {
            return (Answer::Failed(missing), warnings_of(&refusals));
        }
        if refusals.len() == tokens.len() {
            let refused = Answer::Refused {
                refuser: "GitHub".to_owned(),
                refusals,
            };
            return (refused, Vec::new());
        }
        let unseen = format!("no credential can see {repository_name}");
        (Answer::Failed(unseen), warnings_of(&refusals))
    }

    /// An `s3://<bucket>/<key>` object read through the AWS command line, with the
    /// machine's AWS credentials or as the role its `assume_role` names, asking
    /// first whether the kept copy is current.
    fn s3_answer(&self, url: &str, copy_path: &Path, kept: Option<&KeptCopy>) -> Answer {
        let object = match S3Object::parse(url) {
            Ok(object) => object,
            Err(reason) => return Answer::Failed(reason),
        };
        let role_credentials = match object.assume_role {
            Some(role) => match self.role_credentials(role) {
                Ok(credentials) => Some(credentials),
                Err(answer) => return answer,
            },
            None => None,
        };
        let (bucket, key) = (object.bucket, object.key);
        if let Err(error) = fs::create_dir_all(&self.directory) {
            return Answer::Failed(format!("keeping a copy: {error}"));
        }
        let object_file = copy_path.with_extension(format!("{}.object", std::process::id()));
        let mut arguments = vec![
            "s3api".to_owned(),
            "get-object".to_owned(),
            "--bucket".to_owned(),
            bucket.to_owned(),
            "--key".to_owned(),
            key.to_owned(),
        ];
        if let Some(etag) = kept.and_then(|kept| kept.etag.as_ref()) {
            arguments.extend(["--if-none-match".to_owned(), etag.clone()]);
        }
        arguments.push(object_file.to_string_lossy().into_owned());
        let ran = run_bounded(
            &self.aws_command,
            &arguments,
            role_credentials.as_ref(),
            AWS_TIMEOUT,
        );
        let answer = match ran {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Answer::Failed(
                "the `aws` command is not installed, and an S3 link is read through it".to_owned(),
            ),
            Err(error) => Answer::Failed(format!("running `aws`: {error}")),
            Ok(ran) if ran.succeeded => {
                let fields: serde_json::Value =
                    serde_json::from_str(&ran.stdout).unwrap_or_default();
                match fs::read_to_string(&object_file) {
                    Ok(body) => Answer::Sent {
                        etag: fields["ETag"].as_str().map(str::to_owned),
                        last_modified: fields["LastModified"].as_str().map(str::to_owned),
                        body,
                    },
                    Err(error) => Answer::Failed(format!("reading the object: {error}")),
                }
            }
            Ok(ran) => {
                let denied_fix = match object.assume_role {
                    Some(role) => format!("allow {role} `s3:GetObject` on it"),
                    None => "use a profile allowed `s3:GetObject` on it, such as \
                             `AWS_PROFILE=<profile>`"
                        .to_owned(),
                };
                aws_failure(&ran.stderr, &denied_fix)
            }
        };
        let _ = fs::remove_file(&object_file);
        answer
    }

    /// The credentials of `role`, assumed once a run with the machine's AWS
    /// credentials, `AWS_PROFILE` among them; a refusal as the answer to report.
    fn role_credentials(&self, role: &str) -> Result<RoleCredentials, Answer> {
        if let Some(credentials) = self
            .assumed_roles
            .lock()
            .ok()
            .and_then(|assumed| assumed.get(role).cloned())
        {
            return Ok(credentials);
        }
        let arguments: Vec<String> = [
            "sts",
            "assume-role",
            "--role-arn",
            role,
            "--role-session-name",
            ROLE_SESSION_NAME,
            "--output",
            "json",
        ]
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect();
        let ran = match run_bounded(&self.aws_command, &arguments, None, AWS_TIMEOUT) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Answer::Failed(
                    "the `aws` command is not installed, and an S3 link is read through it"
                        .to_owned(),
                ));
            }
            Err(error) => return Err(Answer::Failed(format!("running `aws`: {error}"))),
            Ok(ran) if !ran.succeeded => {
                let denied_fix = format!(
                    "use a profile allowed to assume {role}, such as `AWS_PROFILE=<profile>`"
                );
                return Err(aws_failure(&ran.stderr, &denied_fix));
            }
            Ok(ran) => ran,
        };
        let answered: serde_json::Value = serde_json::from_str(&ran.stdout).unwrap_or_default();
        let field = |name: &str| answered["Credentials"][name].as_str().map(str::to_owned);
        let (Some(access_key_id), Some(secret_access_key), Some(session_token)) = (
            field("AccessKeyId"),
            field("SecretAccessKey"),
            field("SessionToken"),
        ) else {
            return Err(Answer::Failed(format!(
                "assuming {role} answered no credentials"
            )));
        };
        let credentials = RoleCredentials {
            access_key_id,
            secret_access_key,
            session_token,
        };
        if let Ok(mut assumed) = self.assumed_roles.lock() {
            assumed.insert(role.to_owned(), credentials.clone());
        }
        Ok(credentials)
    }

    fn copy_path(&self, fetch_url: &str) -> PathBuf {
        let digest = Sha256::digest(fetch_url.as_bytes());
        let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        self.directory.join(format!("{name}.{COPY_EXTENSION}"))
    }

    /// Drops the least recently used copies beyond the capacity.
    pub fn drop_least_recently_used(&self) {
        let Ok(listing) = fs::read_dir(&self.directory) else {
            return;
        };
        let mut copies: Vec<(SystemTime, PathBuf)> = listing
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == COPY_EXTENSION)
            })
            .filter_map(|path| Some((fs::metadata(&path).ok()?.modified().ok()?, path)))
            .collect();
        copies.sort_by_key(|(used_at, _)| std::cmp::Reverse(*used_at));
        for (_, path) in copies.iter().skip(self.capacity) {
            let _ = fs::remove_file(path);
        }
    }

    /// Keeps `copy`, written beside its place and moved there, so a run reading the
    /// same URL never reads half a copy. A copy that cannot be kept is fetched again.
    fn keep(&self, copy_path: &PathBuf, copy: &KeptCopy) {
        let Ok(text) = serde_json::to_string(copy) else {
            return;
        };
        let partial = copy_path.with_extension(format!("{}.partial", std::process::id()));
        let kept = fs::create_dir_all(&self.directory)
            .and_then(|()| fs::write(&partial, text))
            .and_then(|()| fs::rename(&partial, copy_path));
        if kept.is_err() {
            let _ = fs::remove_file(&partial);
        }
    }
}

/// Marks the copy at `copy_path` used now, its modified time being when it was
/// last used.
fn mark_used(copy_path: &Path) {
    if let Ok(copy) = fs::File::options().write(true).open(copy_path) {
        let _ = copy.set_modified(SystemTime::now());
    }
}

/// What a bounded run of a command printed, and whether it succeeded.
struct CommandRun {
    succeeded: bool,
    stdout: String,
    stderr: String,
}

/// Runs `command`, as `role` where it names one, killing it once `limit` passes.
/// A role's credentials stand in for the machine's; every other AWS setting, such
/// as the profile's region, is left as the machine has it.
fn run_bounded(
    command: &Path,
    arguments: &[String],
    role: Option<&RoleCredentials>,
    limit: Duration,
) -> std::io::Result<CommandRun> {
    let mut command_line = Command::new(command);
    if let Some(role) = role {
        command_line
            .env("AWS_ACCESS_KEY_ID", &role.access_key_id)
            .env("AWS_SECRET_ACCESS_KEY", &role.secret_access_key)
            .env("AWS_SESSION_TOKEN", &role.session_token);
    }
    let mut child = command_line
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other(format!(
                "`{}` did not answer within {} seconds",
                command.display(),
                limit.as_secs()
            )));
        }
        thread::sleep(COMMAND_POLL);
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    Ok(CommandRun {
        succeeded: status.success(),
        stdout,
        stderr,
    })
}

/// What the AWS command line's failure comes to: the kept copy confirmed, the
/// machine's credentials refused with what renews them, access denied with
/// `denied_fix`, or a failure.
fn aws_failure(stderr: &str, denied_fix: &str) -> Answer {
    let said = stderr
        .trim()
        .lines()
        .last()
        .unwrap_or("no reason given")
        .to_owned();
    let credentials_refused = [
        "ExpiredToken",
        "expired",
        "Unable to locate credentials",
        "InvalidAccessKeyId",
        "InvalidClientTokenId",
        "SignatureDoesNotMatch",
    ];
    let refused = |fix: &str| Answer::Refused {
        refuser: "AWS".to_owned(),
        refusals: vec![Refusal {
            reason: aws_reason(&said),
            fix: fix.to_owned(),
        }],
    };
    if said.contains("(304)") || said.contains("Not Modified") {
        Answer::Confirmed
    } else if credentials_refused
        .iter()
        .any(|marker| said.contains(marker))
    {
        refused("run `aws sso login`, or set `AWS_PROFILE` to a profile that can read it")
    } else if said.contains("AccessDenied") || said.contains("(403)") {
        refused(denied_fix)
    } else {
        Answer::Failed(said)
    }
}

/// The AWS command line's error, cut to its code and the identity it names, such
/// as `AccessDenied for arn:aws:iam::<account>:user/<name>`.
fn aws_reason(said: &str) -> String {
    let code = said
        .split_once("An error occurred (")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(code, _)| code);
    let identity = said
        .split_once("User: ")
        .and_then(|(_, rest)| rest.split_whitespace().next());
    match (code, identity) {
        (Some(code), Some(identity)) => format!("{code} for {identity}"),
        (Some(code), None) => code.to_owned(),
        _ => said.to_owned(),
    }
}

/// A request asking the server whether the kept copy is still current.
fn conditional(
    request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    kept: Option<&KeptCopy>,
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    match kept {
        Some(KeptCopy {
            etag: Some(etag), ..
        }) => request.header("If-None-Match", etag),
        Some(KeptCopy {
            last_modified: Some(last_modified),
            ..
        }) => request.header("If-Modified-Since", last_modified),
        _ => request,
    }
}

/// A server's answer to a request for the schema itself.
fn sent_answer(mut response: ureq::http::Response<ureq::Body>) -> Answer {
    match response.status().as_u16() {
        STATUS_OK => {
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            let (etag, last_modified) = (header("etag"), header("last-modified"));
            match response.body_mut().read_to_string() {
                Ok(body) => Answer::Sent {
                    etag,
                    last_modified,
                    body,
                },
                Err(error) => Answer::Failed(error.to_string()),
            }
        }
        STATUS_NOT_MODIFIED => Answer::Confirmed,
        status => Answer::Failed(format!("answered {status}")),
    }
}

fn refusal(credential: &Credential, status: u16) -> Refusal {
    let answered = match status {
        STATUS_UNAUTHORIZED => "401 Bad credentials".to_owned(),
        status => status.to_string(),
    };
    Refusal {
        reason: format!("{}: {answered}", credential.origin),
        fix: credential.fix.clone(),
    }
}

/// The repository a GitHub link's `path`, after its host, names under
/// `<owner>/<repo>/releases/`, and the release asset that path reads:
/// `download/<tag>/<asset>` or `latest/download/<asset>`.
fn github_release(path: &str) -> Option<GithubRelease> {
    let path = path.split(['?', '#']).next()?;
    let steps: Vec<&str> = path.split('/').collect();
    let [owner, repository, "releases", release_steps @ ..] = steps.as_slice() else {
        return None;
    };
    if owner.is_empty() || repository.is_empty() {
        return None;
    }
    let asset = match release_steps {
        ["latest", "download", asset] => Some(ReleaseAsset {
            release: ReleaseName::Latest,
            asset: (*asset).to_owned(),
        }),
        ["download", tag @ .., asset] if !tag.is_empty() => Some(ReleaseAsset {
            release: ReleaseName::Tag(tag.join("/")),
            asset: (*asset).to_owned(),
        }),
        _ => None,
    };
    Some(GithubRelease {
        owner: (*owner).to_owned(),
        repository: (*repository).to_owned(),
        asset,
    })
}

fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    authority.rsplit('@').next()?.split(':').next()
}

/// The URL the server is asked for, with the editor's parameters taken out and
/// every other parameter kept as written, in its order.
fn editor_parameters(url: &str) -> Result<EditorParameters, String> {
    let Some((base, query)) = url.split_once('?') else {
        return Ok(EditorParameters {
            fetch_url: url.to_owned(),
            max_age_days: 0,
            machine_credentials: false,
        });
    };
    let mut passed = Vec::new();
    let mut max_age_days = 0;
    let mut machine_credentials = false;
    for parameter in query.split('&') {
        let (name, value) = parameter.split_once('=').unwrap_or((parameter, ""));
        if !name.starts_with(EDITOR_PARAMETER_PREFIX) {
            passed.push(parameter);
        } else if name == MACHINE_CREDENTIALS_PARAMETER {
            machine_credentials = value.parse().map_err(|_| {
                format!("`{MACHINE_CREDENTIALS_PARAMETER}` must be `true` or `false`")
            })?;
        } else if name != MAX_AGE_PARAMETER {
            return Err(format!("`{name}` is not a parameter the editor reads"));
        } else {
            max_age_days = value
                .parse()
                .map_err(|_| format!("`{MAX_AGE_PARAMETER}` must be a whole number of days"))?;
        }
    }
    let fetch_url = if passed.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", passed.join("&"))
    };
    Ok(EditorParameters {
        fetch_url,
        max_age_days,
        machine_credentials,
    })
}

fn seconds_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread::{self, JoinHandle};

    use super::*;

    const SCHEMA: &str = r#"{"type": "object"}"#;
    const ETAG: &str = "\"mock-etag\"";
    const MOCK_CAPACITY: usize = 8;

    const SILENT_SERVER_POLL: Duration = Duration::from_millis(5);

    /// An HTTP server on a port of its own, answering every request with the schema,
    /// or with `304` to a request carrying its ETag, and counting the full answers it
    /// sends. Dropping it, as a failing test does too, stops and joins its thread.
    struct MockSchemaServer {
        url: String,
        full_answers: Arc<AtomicUsize>,
        server: Arc<tiny_http::Server>,
        serving: Option<JoinHandle<()>>,
    }

    impl MockSchemaServer {
        fn start() -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let url = format!("http://{}/mock.schema.json", server.server_addr());
            let full_answers = Arc::new(AtomicUsize::new(0));
            let (serving, counting) = (Arc::clone(&server), Arc::clone(&full_answers));
            let serving = thread::spawn(move || {
                for request in serving.incoming_requests() {
                    let confirms = request.headers().iter().any(|header| {
                        header.field.equiv("If-None-Match") && header.value.as_str() == ETAG
                    });
                    let etag = tiny_http::Header::from_bytes("ETag", ETAG).unwrap();
                    let response = if confirms {
                        tiny_http::Response::from_string("").with_status_code(304)
                    } else {
                        counting.fetch_add(1, Ordering::SeqCst);
                        tiny_http::Response::from_string(SCHEMA)
                    };
                    let _ = request.respond(response.with_header(etag));
                }
            });
            Self {
                url,
                full_answers,
                server,
                serving: Some(serving),
            }
        }
    }

    impl Drop for MockSchemaServer {
        fn drop(&mut self) {
            self.server.unblock();
            if let Some(serving) = self.serving.take() {
                let _ = serving.join();
            }
        }
    }

    /// A port of its own that takes every connection and closes it unanswered.
    /// Dropping it, as a failing test does too, stops and joins its thread.
    struct SilentServer {
        url: String,
        stopped: Arc<AtomicBool>,
        serving: Option<JoinHandle<()>>,
    }

    impl SilentServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/mock.schema.json", listener.local_addr().unwrap());
            let stopped = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stopped);
            let serving = thread::spawn(move || {
                while !stopping.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((connection, _)) => drop(connection),
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(SILENT_SERVER_POLL);
                        }
                        Err(_) => return,
                    }
                }
            });
            Self {
                url,
                stopped,
                serving: Some(serving),
            }
        }
    }

    impl Drop for SilentServer {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            if let Some(serving) = self.serving.take() {
                let _ = serving.join();
            }
        }
    }

    struct ScratchDirectory(PathBuf);

    impl ScratchDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("schema-urls-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for ScratchDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Credentials a test names, in place of the machine's.
    #[derive(Default)]
    struct GivenCredentials {
        github: Vec<Credential>,
        login: Option<Credential>,
    }

    impl CredentialSource for GivenCredentials {
        fn github_tokens(&self) -> Vec<Credential> {
            self.github.clone()
        }

        fn host_login(&self, _host: &str) -> Option<Credential> {
            self.login.clone()
        }
    }

    fn token(origin: &str, secret: &str, fix: &str) -> Credential {
        Credential {
            origin: format!("`{origin}`"),
            secret: crate::schema_credentials::Secret::Bearer(secret.to_owned()),
            fix: fix.to_owned(),
        }
    }

    const GOOD_TOKEN: &str = "Bearer mock-good-token";
    const BLIND_TOKEN: &str = "Bearer mock-blind-token";
    const NETRC_LOGIN: &str = "Basic bW9jay11c2VyOm1vY2stc2VjcmV0";
    const OPT_IN: &str = "?yaml_dsl_machine_credentials=true";
    const PRIVATE_RELEASE: &str = "mock-org/mock-repo/releases/download/mock/v1.0/mock.schema.json";
    const PUBLIC_RELEASE: &str = "mock-org/public-repo/releases/download/v1/mock.schema.json";

    /// GitHub for one public and one private repository: its pages answer the
    /// public release and 404 for the private one, and its API lets the good token
    /// see the private release, by tag and as the latest, whose asset redirects to
    /// a file store that refuses any request still carrying a token. The blind
    /// token sees nothing. Dropping it stops and joins its thread.
    struct MockGithub {
        api: String,
        server: Arc<tiny_http::Server>,
        serving: Option<JoinHandle<()>>,
    }

    impl MockGithub {
        fn start() -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let api = format!("http://{}", server.server_addr());
            let (serving, base) = (Arc::clone(&server), api.clone());
            let serving = thread::spawn(move || {
                for request in serving.incoming_requests() {
                    let authorization = request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv("Authorization"))
                        .map(|header| header.value.as_str().to_owned());
                    let answer = |status: u16, body: String| {
                        tiny_http::Response::from_string(body).with_status_code(status)
                    };
                    let assets = format!(
                        concat!(
                            r#"{{"assets": [{{"name": "mock.schema.json", "#,
                            r#""url": "{}/assets/7"}}]}}"#
                        ),
                        base
                    );
                    let path = request.url().to_owned();
                    let response = match path.as_str() {
                        "/netrc.json" if authorization.as_deref() == Some(NETRC_LOGIN) => {
                            answer(200, SCHEMA.to_owned())
                        }
                        "/netrc.json" => answer(401, String::new()),
                        page if page.starts_with("/mock-org/public-repo/") => {
                            answer(200, SCHEMA.to_owned())
                        }
                        page if page.starts_with("/mock-org/") => answer(404, String::new()),
                        "/stored/7" if authorization.is_none() => answer(200, SCHEMA.to_owned()),
                        "/stored/7" => answer(400, String::new()),
                        _ if authorization.as_deref() == Some(BLIND_TOKEN) => {
                            answer(404, String::new())
                        }
                        _ if authorization.as_deref() != Some(GOOD_TOKEN) => {
                            answer(401, String::new())
                        }
                        "/repos/mock-org/mock-repo" => answer(200, "{}".to_owned()),
                        "/repos/mock-org/mock-repo/releases/tags/mock%2Fv1.0"
                        | "/repos/mock-org/mock-repo/releases/latest" => answer(200, assets),
                        "/assets/7" => answer(302, String::new()).with_header(
                            tiny_http::Header::from_bytes("Location", format!("{base}/stored/7"))
                                .unwrap(),
                        ),
                        _ => answer(404, String::new()),
                    };
                    let _ = request.respond(response);
                }
            });
            Self {
                api,
                server,
                serving: Some(serving),
            }
        }

        /// A link to `path` on this GitHub's pages.
        fn link(&self, path: &str) -> String {
            format!("{}/{path}", self.api)
        }

        /// Schema URL copies reading this GitHub with `credentials`.
        fn copies(
            &self,
            scratch: &ScratchDirectory,
            credentials: GivenCredentials,
        ) -> SchemaUrlCopies {
            let mut copies = SchemaUrlCopies::with_credentials(
                &scratch.0,
                MOCK_CAPACITY,
                Box::new(credentials),
                &self.api,
            );
            copies.github_link_prefixes = vec![format!("{}/", self.api)];
            copies
        }
    }

    impl Drop for MockGithub {
        fn drop(&mut self) {
            self.server.unblock();
            if let Some(serving) = self.serving.take() {
                let _ = serving.join();
            }
        }
    }

    fn tokens(tokens: Vec<Credential>) -> GivenCredentials {
        GivenCredentials {
            github: tokens,
            login: None,
        }
    }

    fn stale_token() -> Credential {
        token("GH_TOKEN", "mock-stale-token", "replace `GH_TOKEN`")
    }

    fn good_token() -> Credential {
        token("gh auth token", "mock-good-token", "run `gh auth login`")
    }

    #[test]
    fn a_github_link_names_its_repository_and_the_release_asset_its_path_reads() {
        let tagged = github_release(PRIVATE_RELEASE).unwrap();
        assert_eq!(
            (tagged.owner.as_str(), tagged.repository.as_str()),
            ("mock-org", "mock-repo")
        );
        assert_eq!(
            tagged.asset,
            Some(ReleaseAsset {
                release: ReleaseName::Tag("mock/v1.0".into()),
                asset: "mock.schema.json".into(),
            })
        );
        let latest = github_release("mock-org/mock-repo/releases/latest/download/a.json?x=1");
        assert_eq!(
            latest.unwrap().asset,
            Some(ReleaseAsset {
                release: ReleaseName::Latest,
                asset: "a.json".into(),
            })
        );
        assert_eq!(
            github_release("mock-org/mock-repo/releases/tag/v1")
                .unwrap()
                .asset,
            None
        );
        assert_eq!(github_release("mock-org/mock-repo/blob/main/a.json"), None);
    }

    #[test]
    fn a_link_without_the_opt_in_is_read_without_credentials_public_or_not() {
        let github = MockGithub::start();
        let scratch = ScratchDirectory::new("no-opt-in");
        let copies = github.copies(&scratch, tokens(vec![good_token()]));
        let public = copies.read(&github.link(PUBLIC_RELEASE)).unwrap();
        assert_eq!((public.text.as_str(), public.warnings.len()), (SCHEMA, 0));
        let private = copies.read(&github.link(PRIVATE_RELEASE)).unwrap_err();
        assert_eq!(private.reason.as_deref(), Some("answered 404"));
        assert_eq!(private.help, None);
    }

    #[test]
    fn an_opted_in_public_link_is_read_without_credentials_and_no_token_warned_of() {
        let github = MockGithub::start();
        let scratch = ScratchDirectory::new("public-opt-in");
        let copies = github.copies(&scratch, tokens(vec![stale_token()]));
        let read = copies
            .read(&format!("{}{OPT_IN}", github.link(PUBLIC_RELEASE)))
            .unwrap();
        assert_eq!((read.text.as_str(), read.warnings.len()), (SCHEMA, 0));
    }

    #[test]
    fn an_opted_in_private_release_is_read_with_the_first_token_let_in_and_the_others_warned_of() {
        let github = MockGithub::start();
        let scratch = ScratchDirectory::new("release");
        let copies = github.copies(&scratch, tokens(vec![stale_token(), good_token()]));
        let read = copies
            .read(&format!("{}{OPT_IN}", github.link(PRIVATE_RELEASE)))
            .unwrap();
        assert_eq!(read.text, SCHEMA);
        assert_eq!(read.warnings.len(), 1);
        assert!(
            read.warnings[0].contains("`GH_TOKEN`")
                && read.warnings[0].contains("replace `GH_TOKEN`")
        );
        let latest = github.link("mock-org/mock-repo/releases/latest/download/mock.schema.json");
        let scratch = ScratchDirectory::new("latest");
        let copies = github.copies(&scratch, tokens(vec![good_token()]));
        assert_eq!(
            copies.read(&format!("{latest}{OPT_IN}")).unwrap().text,
            SCHEMA
        );
    }

    #[test]
    fn a_release_or_an_asset_the_repository_lacks_is_reported_as_missing_never_as_a_credential() {
        let github = MockGithub::start();
        let scratch = ScratchDirectory::new("missing");
        let copies = github.copies(&scratch, tokens(vec![good_token()]));
        let absent_release =
            github.link("mock-org/mock-repo/releases/download/v9/mock.schema.json");
        let failure = copies
            .read(&format!("{absent_release}{OPT_IN}"))
            .unwrap_err();
        assert_eq!(
            failure.reason.as_deref(),
            Some("`mock-org/mock-repo` has no release `v9`")
        );
        let absent_asset = github.link("mock-org/mock-repo/releases/download/mock/v1.0/other.json");
        let failure = copies.read(&format!("{absent_asset}{OPT_IN}")).unwrap_err();
        assert_eq!(
            failure.reason.as_deref(),
            Some("release `mock/v1.0` of `mock-org/mock-repo` has no asset `other.json`")
        );
    }

    #[test]
    fn a_repository_no_token_can_see_is_reported_by_name_with_the_refused_tokens_warned_of() {
        let github = MockGithub::start();
        let scratch = ScratchDirectory::new("unseen");
        let blind = token("GITHUB_TOKEN", "mock-blind-token", "replace `GITHUB_TOKEN`");
        let copies = github.copies(&scratch, tokens(vec![stale_token(), blind]));
        let failure = copies
            .read(&format!("{}{OPT_IN}", github.link(PRIVATE_RELEASE)))
            .unwrap_err();
        assert_eq!(
            failure.reason.as_deref(),
            Some("no credential can see `mock-org/mock-repo`")
        );
    }

    #[test]
    fn every_token_refused_reads_the_kept_copy_with_warnings_else_fails_naming_each() {
        let github = MockGithub::start();
        let scratch = ScratchDirectory::new("refused");
        let expired = token("gh auth token", "mock-expired-token", "run `gh auth login`");
        let copies = github.copies(&scratch, tokens(vec![expired]));
        let release = github.link(PRIVATE_RELEASE);
        let opted_in = format!("{release}{OPT_IN}");
        let failure = copies.read(&opted_in).unwrap_err();
        assert_eq!(
            failure.message,
            format!("GitHub refused to read {opted_in}")
        );
        assert_eq!(
            failure.reason.as_deref(),
            Some("`gh auth token`: 401 Bad credentials")
        );
        assert_eq!(failure.help.as_deref(), Some("run `gh auth login`"));
        copies.keep(
            &copies.copy_path(&release),
            &KeptCopy {
                url: release.clone(),
                etag: None,
                last_modified: None,
                confirmed_at: 0,
                body: SCHEMA.to_owned(),
            },
        );
        let read = copies.read(&opted_in).unwrap();
        assert_eq!(read.text, SCHEMA);
        assert_eq!(read.warnings.len(), 1);
    }

    #[test]
    fn another_host_is_read_with_its_netrc_login_only_where_the_link_opts_in() {
        let github = MockGithub::start();
        let scratch = ScratchDirectory::new("netrc");
        let login = Credential {
            origin: "the `.netrc` entry for 127.0.0.1".to_owned(),
            secret: crate::schema_credentials::Secret::Basic {
                login: "mock-user".to_owned(),
                password: "mock-secret".to_owned(),
            },
            fix: "update the `.netrc` entry for 127.0.0.1".to_owned(),
        };
        let credentials = GivenCredentials {
            github: Vec::new(),
            login: Some(login),
        };
        let copies = github.copies(&scratch, credentials);
        let link = github.link("netrc.json");
        assert!(copies.read(&link).is_err());
        assert_eq!(
            copies.read(&format!("{link}{OPT_IN}")).unwrap().text,
            SCHEMA
        );
    }

    const S3_LINK: &str = "s3://mock-schemas/mock/1.0/mock.schema.json";

    /// The AWS command line's answers to `s3api get-object`: the object, or `304`
    /// to a request carrying an ETag.
    const AWS_ANSWERING: &str = r#"#!/bin/sh
out=""
etag=""
while [ $# -gt 0 ]; do
  case "$1" in
    --if-none-match) etag="$2"; shift 2 ;;
    --bucket|--key) shift 2 ;;
    s3api|get-object) shift ;;
    *) out="$1"; shift ;;
  esac
done
if [ -n "$etag" ]; then
  echo 'An error occurred (304) when calling the GetObject operation: Not Modified' >&2
  exit 254
fi
printf '%s' '{"type": "object"}' > "$out"
echo '{"ETag": "\"mock-s3-etag\"", "LastModified": "Sat, 31 Oct 2026 00:00:00 GMT"}'
"#;

    /// The AWS command line once the machine's SSO session has expired.
    const AWS_EXPIRED: &str = r#"#!/bin/sh
echo 'An error occurred (ExpiredToken) when calling the GetObject operation:' \
  'The provided token has expired.' >&2
exit 254
"#;

    const ROLE_LINK: &str = "s3://mock-schemas/mock/1.0/mock.schema.json\
                             ?assume_role=arn:aws:iam::123456789012:role/mock-reader";

    /// The AWS command line assuming `mock-reader` for the profile `mock-profile`
    /// alone, and handing the object only to a request carrying that role's session.
    const AWS_ASSUMING: &str = r#"#!/bin/sh
if [ "$1" = "sts" ]; then
  if [ "$AWS_PROFILE" != "mock-profile" ]; then
    echo 'An error occurred (AccessDenied) when calling the AssumeRole operation:' \
      'User: arn:aws:iam::123456789012:user/mock-user is not authorized' >&2
    exit 254
  fi
  echo '{"Credentials": {"AccessKeyId": "mock-key", "SecretAccessKey": "mock-secret",' \
    '"SessionToken": "mock-session"}}'
  exit 0
fi
if [ "$AWS_SESSION_TOKEN" != "mock-session" ] || [ "$AWS_PROFILE" != "mock-profile" ]; then
  echo 'An error occurred (AccessDenied) when calling the GetObject operation' >&2
  exit 254
fi
for argument in "$@"; do out="$argument"; done
printf '%s' '{"type": "object"}' > "$out"
echo '{"ETag": "\"mock-s3-etag\""}'
"#;

    fn aws_copies(scratch: &ScratchDirectory, script: &str) -> SchemaUrlCopies {
        let command = scratch.0.join("mock-aws");
        fs::create_dir_all(&scratch.0).unwrap();
        fs::write(&command, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut copies = SchemaUrlCopies::with_credentials(
            scratch.0.join("copies"),
            MOCK_CAPACITY,
            Box::new(GivenCredentials::default()),
            GITHUB_API,
        );
        copies.aws_command = command;
        copies
    }

    #[test]
    fn an_s3_object_is_read_through_the_aws_command_and_its_copy_confirmed_by_etag() {
        let scratch = ScratchDirectory::new("s3");
        let copies = aws_copies(&scratch, AWS_ANSWERING);
        assert_eq!(copies.read(S3_LINK).unwrap().text, SCHEMA);
        let kept: KeptCopy =
            serde_json::from_str(&fs::read_to_string(copies.copy_path(S3_LINK)).unwrap()).unwrap();
        assert_eq!(kept.etag.as_deref(), Some("\"mock-s3-etag\""));
        let confirmed = copies.read(S3_LINK).unwrap();
        assert_eq!(
            (confirmed.text.as_str(), confirmed.warnings.len()),
            (SCHEMA, 0)
        );
    }

    #[test]
    fn expired_aws_credentials_read_the_kept_copy_with_a_warning_else_fail_with_the_fix() {
        let scratch = ScratchDirectory::new("s3-expired");
        let copies = aws_copies(&scratch, AWS_EXPIRED);
        let failure = copies.read(S3_LINK).unwrap_err();
        assert_eq!(failure.message, format!("AWS refused to read {S3_LINK}"));
        assert_eq!(failure.reason.as_deref(), Some("ExpiredToken"));
        assert!(failure.help.unwrap().contains("aws sso login"));
        copies.keep(
            &copies.copy_path(S3_LINK),
            &KeptCopy {
                url: S3_LINK.to_owned(),
                etag: Some("\"mock-s3-etag\"".to_owned()),
                last_modified: None,
                confirmed_at: 0,
                body: SCHEMA.to_owned(),
            },
        );
        let read = copies.read(S3_LINK).unwrap();
        assert_eq!((read.text.as_str(), read.warnings.len()), (SCHEMA, 1));
    }

    #[test]
    fn an_s3_object_is_read_as_the_role_its_link_names_assumed_by_the_profile() {
        let scratch = ScratchDirectory::new("s3-role");
        let copies = aws_copies(&scratch, AWS_ASSUMING);
        let script = scratch.0.join("mock-aws");
        let profiled = scratch.0.join("profiled-aws");
        fs::write(
            &profiled,
            format!(
                "#!/bin/sh\nAWS_PROFILE=mock-profile exec {} \"$@\"\n",
                script.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&profiled, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let refused = copies.read(ROLE_LINK).unwrap_err();
        assert_eq!(
            refused.reason.as_deref(),
            Some("AccessDenied for arn:aws:iam::123456789012:user/mock-user")
        );
        assert!(refused.help.unwrap().contains("allowed to assume"));
        let mut copies = copies;
        copies.aws_command = profiled;
        assert_eq!(copies.read(ROLE_LINK).unwrap().text, SCHEMA);
    }

    #[test]
    fn an_s3_link_takes_only_the_assume_role_parameter() {
        let parsed = S3Object::parse(ROLE_LINK).unwrap();
        assert_eq!(
            (parsed.bucket, parsed.key, parsed.assume_role),
            (
                "mock-schemas",
                "mock/1.0/mock.schema.json",
                Some("arn:aws:iam::123456789012:role/mock-reader")
            )
        );
        assert!(S3Object::parse("s3://mock-schemas/mock.json?role=x").is_err());
        assert!(S3Object::parse("s3://mock-schemas").is_err());
    }

    #[test]
    fn an_aws_error_is_cut_to_its_code_and_the_identity_it_names() {
        let said = "aws: [ERROR]: An error occurred (AccessDenied) when calling the GetObject \
                    operation: User: arn:aws:iam::123456789012:user/mock-user is not authorized";
        assert_eq!(
            aws_reason(said),
            "AccessDenied for arn:aws:iam::123456789012:user/mock-user"
        );
    }

    #[test]
    fn the_editor_parameters_are_taken_out_and_others_kept_in_order() {
        assert_eq!(
            editor_parameters("https://mock.example/s.json?b=2&yaml_dsl_max_age_days=7&a=1"),
            Ok(EditorParameters {
                fetch_url: "https://mock.example/s.json?b=2&a=1".into(),
                max_age_days: 7,
                machine_credentials: false,
            })
        );
        assert!(editor_parameters("https://mock.example/s.json?yaml_dsl_other=1").is_err());
        assert!(editor_parameters("https://mock.example/s.json?yaml_dsl_max_age_days=x").is_err());
        let opted_in = "https://mock.example/s.json?yaml_dsl_machine_credentials=true";
        assert!(editor_parameters(opted_in).unwrap().machine_credentials);
        let opted_out = "https://mock.example/s.json?yaml_dsl_machine_credentials=false";
        assert!(!editor_parameters(opted_out).unwrap().machine_credentials);
        let misspelt = "https://mock.example/s.json?yaml_dsl_machine_credentials=yes";
        assert_eq!(
            editor_parameters(misspelt),
            Err("`yaml_dsl_machine_credentials` must be `true` or `false`".to_owned())
        );
    }

    #[test]
    fn a_copy_younger_than_its_max_age_is_read_without_asking_the_server() {
        let server = MockSchemaServer::start();
        let scratch = ScratchDirectory::new("max-age");
        let copies = SchemaUrlCopies::with_credentials(
            &scratch.0,
            MOCK_CAPACITY,
            Box::new(GivenCredentials::default()),
            GITHUB_API,
        );
        let aged = format!("{}?yaml_dsl_max_age_days=1", server.url);
        assert_eq!(copies.read(&aged).unwrap().text, SCHEMA);
        assert_eq!(copies.read(&aged).unwrap().text, SCHEMA);
        assert_eq!(server.full_answers.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn with_no_max_age_the_copy_is_checked_and_a_304_keeps_it() {
        let server = MockSchemaServer::start();
        let scratch = ScratchDirectory::new("checked");
        let copies = SchemaUrlCopies::with_credentials(
            &scratch.0,
            MOCK_CAPACITY,
            Box::new(GivenCredentials::default()),
            GITHUB_API,
        );
        assert_eq!(copies.read(&server.url).unwrap().text, SCHEMA);
        assert_eq!(copies.read(&server.url).unwrap().text, SCHEMA);
        assert_eq!(server.full_answers.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_kept_copy_is_read_when_the_server_does_not_answer() {
        let server = SilentServer::start();
        let scratch = ScratchDirectory::new("offline");
        let copies = SchemaUrlCopies::with_credentials(
            &scratch.0,
            MOCK_CAPACITY,
            Box::new(GivenCredentials::default()),
            GITHUB_API,
        );
        let unanswered = server.url.as_str();
        assert!(copies.read(unanswered).is_err());
        copies.keep(
            &copies.copy_path(unanswered),
            &KeptCopy {
                url: unanswered.to_owned(),
                etag: Some(ETAG.to_owned()),
                last_modified: None,
                confirmed_at: 0,
                body: SCHEMA.to_owned(),
            },
        );
        assert_eq!(copies.read(unanswered).unwrap().text, SCHEMA);
    }

    #[test]
    fn the_least_recently_used_copies_beyond_the_capacity_are_dropped() {
        let scratch = ScratchDirectory::new("capacity");
        let copies = SchemaUrlCopies::with_credentials(
            &scratch.0,
            2,
            Box::new(GivenCredentials::default()),
            GITHUB_API,
        );
        let links = [
            "https://mock.example/first.schema.json",
            "https://mock.example/second.schema.json",
            "https://mock.example/third.schema.json",
        ];
        for (order, link) in links.iter().enumerate() {
            let copy_path = copies.copy_path(link);
            copies.keep(
                &copy_path,
                &KeptCopy {
                    url: (*link).to_owned(),
                    etag: None,
                    last_modified: None,
                    confirmed_at: seconds_now(),
                    body: SCHEMA.to_owned(),
                },
            );
            let used_at = UNIX_EPOCH + Duration::from_secs(order as u64 + 1);
            let copy = fs::File::options().write(true).open(&copy_path).unwrap();
            copy.set_modified(used_at).unwrap();
        }
        let first = format!("{}?yaml_dsl_max_age_days=1", links[0]);
        assert_eq!(copies.read(&first).unwrap().text, SCHEMA);
        copies.drop_least_recently_used();
        assert!(copies.copy_path(links[0]).exists());
        assert!(!copies.copy_path(links[1]).exists());
        assert!(copies.copy_path(links[2]).exists());
    }
}
