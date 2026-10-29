//! Pull requests and what happens to them, for the daemon that publishes
//! agents' branches: opening one, its state, its checks and their logs, and
//! its reviews and comments. Every call takes an installation token for the
//! repository.

use anyhow::{bail, Result};
use hyper::Method;
use serde::Serialize;
use serde_json::{json, Value};

use crate::github::{split_repo, GitHub, Token};
use crate::http;

/// A pull request, as far as an author needs to know it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pull {
    pub number: u64,
    pub url: String,
    /// `open` or `closed`.
    pub state: String,
    pub merged: bool,
    pub head_sha: String,
    pub title: String,
}

impl Pull {
    fn from(v: &Value) -> Pull {
        Pull {
            number: v["number"].as_u64().unwrap_or(0),
            url: v["html_url"].as_str().unwrap_or("").into(),
            state: v["state"].as_str().unwrap_or("").into(),
            merged: v["merged"].as_bool().unwrap_or(false) || v["merged_at"].is_string(),
            head_sha: v["head"]["sha"].as_str().unwrap_or("").into(),
            title: v["title"].as_str().unwrap_or("").into(),
        }
    }
}

/// One check on a commit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub id: u64,
    pub name: String,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// When completed: `success`, `failure`, `neutral`, `cancelled`,
    /// `skipped`, `timed_out` or `action_required`.
    pub conclusion: Option<String>,
    pub url: String,
}

impl Check {
    /// Completed without passing.
    pub fn failed(&self) -> bool {
        self.status == "completed"
            && !matches!(
                self.conclusion.as_deref(),
                Some("success" | "neutral" | "skipped")
            )
    }
}

/// A review, or a comment on the pull request or on a line of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Remark {
    pub id: u64,
    /// `review`, `comment` or `line`.
    pub kind: String,
    pub author: String,
    /// A review's verdict: `APPROVED`, `CHANGES_REQUESTED` or `COMMENTED`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    pub body: String,
    /// For a comment on a line: `path:line`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    pub when: String,
}

/// No more of a job's log than this is returned: its end, where failures are.
pub const MAX_LOG: usize = 16 << 10;

impl GitHub {
    async fn get(&self, token: &Token, path: &str) -> Result<Value> {
        self.call_token(Method::GET, path, token, None).await
    }

    /// The open pull request whose head is `branch` of the repository's
    /// owner, if there is one.
    pub async fn open_pull(&self, token: &Token, repo: &str, branch: &str) -> Result<Option<Pull>> {
        let (owner, _) = split_repo(repo)?;
        let v = self
            .get(
                token,
                &format!("/repos/{repo}/pulls?state=open&head={owner}:{branch}&per_page=10"),
            )
            .await?;
        Ok(v.as_array().and_then(|a| a.first()).map(Pull::from))
    }

    /// Opens a pull request of `branch` into `base`.
    pub async fn create_pull(
        &self,
        token: &Token,
        repo: &str,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<Pull> {
        let v = self
            .call_token(
                Method::POST,
                &format!("/repos/{repo}/pulls"),
                token,
                Some(&json!({ "head": branch, "base": base, "title": title, "body": body })),
            )
            .await?;
        Ok(Pull::from(&v))
    }

    pub async fn pull(&self, token: &Token, repo: &str, number: u64) -> Result<Pull> {
        Ok(Pull::from(
            &self
                .get(token, &format!("/repos/{repo}/pulls/{number}"))
                .await?,
        ))
    }

    /// The checks on a commit.
    pub async fn checks(&self, token: &Token, repo: &str, sha: &str) -> Result<Vec<Check>> {
        let v = self
            .get(
                token,
                &format!("/repos/{repo}/commits/{sha}/check-runs?per_page=100"),
            )
            .await?;
        Ok(v["check_runs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| Check {
                id: c["id"].as_u64().unwrap_or(0),
                name: c["name"].as_str().unwrap_or("").into(),
                status: c["status"].as_str().unwrap_or("").into(),
                conclusion: c["conclusion"].as_str().map(str::to_string),
                url: c["html_url"].as_str().unwrap_or("").into(),
            })
            .collect())
    }

    /// The end of an Actions job's log (a check's id is its job's). GitHub
    /// answers with a redirect to storage, followed without the token.
    pub async fn job_log(&self, token: &Token, repo: &str, job: u64) -> Result<String> {
        let path = format!("/repos/{repo}/actions/jobs/{job}/logs");
        let r = http::send(
            Method::GET,
            &format!("{}{path}", self.api()),
            Some(&token.value),
            None,
        )
        .await?;
        let r = match (r.status, &r.location) {
            (301 | 302 | 303 | 307 | 308, Some(to)) => {
                http::send(Method::GET, to, None, None).await?
            }
            _ => r,
        };
        if !(200..300).contains(&r.status) {
            bail!("GET {path}: HTTP {}", r.status);
        }
        let text = String::from_utf8_lossy(&r.body);
        let start = text.len().saturating_sub(MAX_LOG);
        let start = (start..text.len())
            .find(|i| text.is_char_boundary(*i))
            .unwrap_or(0);
        Ok(text[start..].to_string())
    }

    /// Reviews, comments and line comments, oldest first.
    pub async fn remarks(&self, token: &Token, repo: &str, number: u64) -> Result<Vec<Remark>> {
        let mut out = Vec::new();
        let author = |v: &Value| v["user"]["login"].as_str().unwrap_or("").to_string();
        for r in self
            .get(
                token,
                &format!("/repos/{repo}/pulls/{number}/reviews?per_page=100"),
            )
            .await?
            .as_array()
            .into_iter()
            .flatten()
        {
            out.push(Remark {
                id: r["id"].as_u64().unwrap_or(0),
                kind: "review".into(),
                author: author(r),
                verdict: r["state"].as_str().map(str::to_string),
                body: r["body"].as_str().unwrap_or("").into(),
                at: None,
                when: r["submitted_at"].as_str().unwrap_or("").into(),
            });
        }
        for c in self
            .get(
                token,
                &format!("/repos/{repo}/issues/{number}/comments?per_page=100"),
            )
            .await?
            .as_array()
            .into_iter()
            .flatten()
        {
            out.push(Remark {
                id: c["id"].as_u64().unwrap_or(0),
                kind: "comment".into(),
                author: author(c),
                verdict: None,
                body: c["body"].as_str().unwrap_or("").into(),
                at: None,
                when: c["created_at"].as_str().unwrap_or("").into(),
            });
        }
        for c in self
            .get(
                token,
                &format!("/repos/{repo}/pulls/{number}/comments?per_page=100"),
            )
            .await?
            .as_array()
            .into_iter()
            .flatten()
        {
            let line = c["line"].as_u64().or_else(|| c["original_line"].as_u64());
            out.push(Remark {
                id: c["id"].as_u64().unwrap_or(0),
                kind: "line".into(),
                author: author(c),
                verdict: None,
                body: c["body"].as_str().unwrap_or("").into(),
                at: Some(format!(
                    "{}:{}",
                    c["path"].as_str().unwrap_or(""),
                    line.map(|l| l.to_string()).unwrap_or_default()
                )),
                when: c["created_at"].as_str().unwrap_or("").into(),
            });
        }
        out.sort_by(|a, b| a.when.cmp(&b.when));
        Ok(out)
    }
}

/// The header git sends to authenticate with an installation token
/// (`-c http.extraHeader=...`): Basic, as `x-access-token`.
pub fn git_auth_header(token: &Token) -> String {
    use base64::Engine as _;
    let pair = format!("x-access-token:{}", token.value);
    format!(
        "Authorization: Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}
