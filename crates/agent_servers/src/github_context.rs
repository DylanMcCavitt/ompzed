//! Read-only GitHub context bridge for OMP workflows (AGE-649 / ZED-08).
//!
//! Fork-native per the extension capability audit: the extension sandbox has no
//! keychain, panel, or agent-thread access, so a GitHub bridge cannot live there.
//! We drive everything through the `gh` CLI in read-only mode. That satisfies the
//! audit's privilege boundary cleanly:
//!
//! * **Credentials** — `gh` self-manages auth (keychain / `GH_TOKEN`); we never
//!   read, re-store, or log a token.
//! * **Egress** — `gh` only talks to GitHub hosts.
//! * **Fail closed** — when `gh` is missing, unauthenticated, or the directory is
//!   not a GitHub repo, we return a degraded [`GithubContext`] and surface nothing.
//! * **Read-only** — only `repo view`, `issue list`, and `pr list` are ever
//!   constructed (see [`fetch_github_context`]); no subcommand mutates state.

use gpui::SharedString;
use std::path::{Path, PathBuf};

/// Default number of open issues / PRs to request from `gh`.
pub const DEFAULT_LIMIT: usize = 10;

/// A read-only snapshot of the current repository's GitHub context.
///
/// When [`available`](Self::available) is false the snapshot is *degraded*:
/// [`status`](Self::status) explains why (no `gh`, not authenticated, or not a
/// GitHub repo) and the lists are empty. Callers render the status instead of
/// inventing data — the bridge fails closed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GithubContext {
    pub available: bool,
    pub status: Option<SharedString>,
    pub repo: Option<GithubRepo>,
    pub issues: Vec<GithubRef>,
    pub pull_requests: Vec<GithubRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubRepo {
    pub name_with_owner: SharedString,
    pub default_branch: Option<SharedString>,
}

/// A minimal reference to an open issue or pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubRef {
    pub number: u64,
    pub title: SharedString,
}

impl GithubContext {
    fn degraded(status: impl Into<SharedString>) -> Self {
        Self {
            available: false,
            status: Some(status.into()),
            repo: None,
            issues: Vec::new(),
            pull_requests: Vec::new(),
        }
    }

    /// One-line label for the panel header: the repo plus open counts when
    /// available, otherwise the degraded status.
    pub fn summary_label(&self) -> SharedString {
        match &self.repo {
            Some(repo) => format!(
                "{} · {} · {}",
                repo.name_with_owner,
                count_label(self.issues.len(), "issue"),
                count_label(self.pull_requests.len(), "PR"),
            )
            .into(),
            None => self
                .status
                .clone()
                .unwrap_or_else(|| "GitHub context unavailable".into()),
        }
    }
}

fn count_label(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 open {noun}")
    } else {
        format!("{count} open {noun}s")
    }
}

/// Resolve the `gh` binary. With an explicit override, use it only if it exists
/// (so a missing override degrades cleanly rather than falling through to a real
/// `gh` on PATH — this keeps the "missing gh" path deterministic in tests).
fn resolve_gh_binary(binary_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = binary_override {
        return path.exists().then(|| path.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.push("/opt/homebrew/bin".into());
    dirs.push("/usr/local/bin".into());
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local/bin"));
    }
    dirs.into_iter()
        .map(|dir| dir.join("gh"))
        .find(|candidate| candidate.exists())
}

/// Run a read-only `gh` command in `work_dir`, returning captured stdout on a
/// clean exit and `None` on any failure (spawn error, non-zero exit). Failures
/// degrade silently — we never surface a token or stderr.
async fn run_gh(gh: &Path, work_dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = util::command::new_command(gh)
        .args(args)
        .current_dir(work_dir)
        .output()
        .await
        .ok()?;
    output.status.success().then_some(output.stdout)
}

#[derive(serde::Deserialize)]
struct RepoView {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
    #[serde(rename = "defaultBranchRef")]
    default_branch_ref: Option<BranchRef>,
}

#[derive(serde::Deserialize)]
struct BranchRef {
    name: String,
}

#[derive(serde::Deserialize)]
struct RefRow {
    number: u64,
    title: String,
}

fn parse_repo(stdout: &[u8]) -> Option<GithubRepo> {
    let view: RepoView = serde_json::from_slice(stdout).ok()?;
    Some(GithubRepo {
        name_with_owner: view.name_with_owner.into(),
        default_branch: view.default_branch_ref.map(|branch| branch.name.into()),
    })
}

fn parse_refs(stdout: &[u8]) -> Option<Vec<GithubRef>> {
    let rows: Vec<RefRow> = serde_json::from_slice(stdout).ok()?;
    Some(
        rows.into_iter()
            .map(|row| GithubRef {
                number: row.number,
                title: row.title.into(),
            })
            .collect(),
    )
}

/// Fetch a read-only GitHub context snapshot for `work_dir`.
///
/// `gh` is the binary override (`None` → resolve from PATH / common dirs).
/// `limit` caps the open issues and PRs requested. Any missing piece degrades:
/// no `gh` or no resolvable repo returns a degraded snapshot; failing issue/PR
/// lookups leave their lists empty while the repo still resolves.
///
/// Read-only invariant: the only `gh` subcommands constructed here are
/// `repo view`, `issue list`, and `pr list`.
pub async fn fetch_github_context(
    gh: Option<PathBuf>,
    work_dir: PathBuf,
    limit: usize,
) -> GithubContext {
    let Some(gh) = resolve_gh_binary(gh.as_deref()) else {
        return GithubContext::degraded("GitHub CLI (`gh`) not found");
    };

    let Some(repo) = run_gh(
        &gh,
        &work_dir,
        &["repo", "view", "--json", "nameWithOwner,defaultBranchRef"],
    )
    .await
    .and_then(|stdout| parse_repo(&stdout)) else {
        return GithubContext::degraded("No GitHub repository here, or `gh` is not authenticated");
    };

    let limit = limit.to_string();

    let issues = run_gh(
        &gh,
        &work_dir,
        &[
            "issue", "list", "--state", "open", "--limit", &limit, "--json", "number,title",
        ],
    )
    .await
    .and_then(|stdout| parse_refs(&stdout))
    .unwrap_or_default();

    let pull_requests = run_gh(
        &gh,
        &work_dir,
        &[
            "pr", "list", "--state", "open", "--limit", &limit, "--json", "number,title",
        ],
    )
    .await
    .and_then(|stdout| parse_refs(&stdout))
    .unwrap_or_default();

    GithubContext {
        available: true,
        status: None,
        repo: Some(repo),
        issues,
        pull_requests,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use indoc::formatdoc;
    use std::{fs, os::unix::fs::PermissionsExt};

    /// A fake `gh` binary: a shell script that records every argv line it is
    /// invoked with and emits canned JSON per read-only subcommand.
    struct FakeGh {
        _dir: tempfile::TempDir,
        script_path: PathBuf,
        args_path: PathBuf,
    }

    impl FakeGh {
        /// `mode` is "ok" (repo + issues + PRs resolve) or "no-repo"
        /// (`repo view` exits non-zero, as outside a GitHub remote / unauth).
        fn new(mode: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script_path = dir.path().join("gh");
            let args_path = dir.path().join("args");
            fs::write(
                &script_path,
                formatdoc! {r#"
                    #!/bin/sh
                    printf '%s\n' "$*" >> "{args_path}"
                    case "$1 $2" in
                      "repo view")
                        if [ "{mode}" = "no-repo" ]; then
                          echo "not a github repository" 1>&2
                          exit 1
                        fi
                        printf '%s' '{{"nameWithOwner":"octo/demo","defaultBranchRef":{{"name":"main"}}}}'
                        ;;
                      "issue list")
                        printf '%s' '[{{"number":7,"title":"Fix the bug"}},{{"number":9,"title":"Add a feature"}}]'
                        ;;
                      "pr list")
                        printf '%s' '[{{"number":12,"title":"Implement bridge"}}]'
                        ;;
                      *)
                        echo "unexpected: $*" 1>&2
                        exit 2
                        ;;
                    esac
                "#,
                    args_path = args_path.display(),
                    mode = mode,
                },
            )
            .unwrap();
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
            Self {
                _dir: dir,
                script_path,
                args_path,
            }
        }

        fn recorded_args(&self) -> String {
            fs::read_to_string(&self.args_path).unwrap_or_default()
        }
    }

    #[gpui::test]
    async fn fetch_parses_repo_issues_and_prs(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let fake = FakeGh::new("ok");
        let work_dir = tempfile::tempdir().unwrap();
        let ctx = fetch_github_context(
            Some(fake.script_path.clone()),
            work_dir.path().to_path_buf(),
            DEFAULT_LIMIT,
        )
        .await;

        assert!(ctx.available);
        assert_eq!(ctx.status, None);
        let repo = ctx.repo.as_ref().expect("repo resolved");
        assert_eq!(repo.name_with_owner.as_ref(), "octo/demo");
        assert_eq!(repo.default_branch.as_deref(), Some("main"));
        assert_eq!(
            ctx.issues,
            vec![
                GithubRef {
                    number: 7,
                    title: "Fix the bug".into()
                },
                GithubRef {
                    number: 9,
                    title: "Add a feature".into()
                },
            ]
        );
        assert_eq!(
            ctx.pull_requests,
            vec![GithubRef {
                number: 12,
                title: "Implement bridge".into()
            }]
        );
        assert_eq!(
            ctx.summary_label().as_ref(),
            "octo/demo · 2 open issues · 1 open PR"
        );
    }

    #[gpui::test]
    async fn no_repo_fails_closed_to_degraded(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let fake = FakeGh::new("no-repo");
        let work_dir = tempfile::tempdir().unwrap();
        let ctx = fetch_github_context(
            Some(fake.script_path.clone()),
            work_dir.path().to_path_buf(),
            DEFAULT_LIMIT,
        )
        .await;

        assert!(!ctx.available);
        assert!(ctx.repo.is_none());
        assert!(ctx.issues.is_empty());
        assert!(ctx.pull_requests.is_empty());
        let summary = ctx.summary_label();
        let status = ctx.status.expect("degraded status");
        assert_eq!(summary, status);
        // `repo view` ran; issue/pr list never did (we fail closed first).
        let recorded = fake.recorded_args();
        assert!(recorded.contains("repo view"));
        assert!(!recorded.contains("issue list"));
        assert!(!recorded.contains("pr list"));
    }

    #[gpui::test]
    async fn missing_gh_degrades(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let work_dir = tempfile::tempdir().unwrap();
        let missing = work_dir.path().join("does-not-exist-gh");
        let ctx =
            fetch_github_context(Some(missing), work_dir.path().to_path_buf(), DEFAULT_LIMIT).await;

        assert!(!ctx.available);
        assert!(ctx.repo.is_none());
        assert!(ctx.status.is_some());
    }

    #[gpui::test]
    async fn only_read_only_subcommands_are_invoked(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let fake = FakeGh::new("ok");
        let work_dir = tempfile::tempdir().unwrap();
        let _ = fetch_github_context(
            Some(fake.script_path.clone()),
            work_dir.path().to_path_buf(),
            DEFAULT_LIMIT,
        )
        .await;

        let recorded = fake.recorded_args();
        assert!(recorded.contains("repo view"));
        assert!(recorded.contains("issue list --state open"));
        assert!(recorded.contains("pr list --state open"));
        // No mutating verb may ever reach `gh`.
        for forbidden in [
            "create", "edit", "comment", "close", "merge", "delete", "reopen", "lock", "transfer",
            "ready", "review",
        ] {
            assert!(
                !recorded.contains(forbidden),
                "read-only invariant violated: `gh` argv contained `{forbidden}`:\n{recorded}"
            );
        }
    }

    #[test]
    fn summary_label_singular_and_zero() {
        let ctx = GithubContext {
            available: true,
            status: None,
            repo: Some(GithubRepo {
                name_with_owner: "octo/demo".into(),
                default_branch: None,
            }),
            issues: vec![GithubRef {
                number: 1,
                title: "only".into(),
            }],
            pull_requests: Vec::new(),
        };
        assert_eq!(
            ctx.summary_label().as_ref(),
            "octo/demo · 1 open issue · 0 open PRs"
        );
    }

    #[test]
    fn degraded_summary_uses_status() {
        let ctx = GithubContext::degraded("nope");
        assert_eq!(ctx.summary_label().as_ref(), "nope");
    }
}
