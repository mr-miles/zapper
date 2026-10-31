//! Git over HTTP, as zapper's facilities serve it to guests: `git
//! http-backend` run as a CGI for one request, and the `pre-receive` hook
//! that keeps branches apart from `main`. The office's git server and the
//! workbench's repository both use it; who may push what is the caller's
//! to say (`level`), the hook enforces it.

use std::path::Path;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{HeaderMap, Response, StatusCode};
use tokio::io::AsyncWriteExt;

/// One request to a repository under `root`.
pub struct Request<'a> {
    /// `GIT_PROJECT_ROOT`: the directory the repositories are in.
    pub root: &'a Path,
    /// Who pushes: `REMOTE_USER`, the committer's name, and the name the
    /// hook records with each commit it pushed.
    pub user: &'a str,
    /// The committer's address.
    pub email: &'a str,
    /// What the pusher may do, for the hook (`ZAPPER_GIT_LEVEL`): `write`
    /// (branches, never `main`, `main/…` or tags), `merge`, or `admin`.
    pub level: &'a str,
    pub method: &'a str,
    /// `/<repo>.git/…`, as `PATH_INFO`.
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a HeaderMap,
    pub body: Bytes,
}

fn plain(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from(format!("{msg}\n"))))
        .unwrap()
}

/// Runs `git http-backend` for `r`: its response, and whether git did what
/// was asked (for a push: the hook let it in).
pub async fn http_backend(r: Request<'_>) -> (Response<Full<Bytes>>, bool) {
    let header = |k: &str| {
        r.headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("http-backend")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("GIT_PROJECT_ROOT", r.root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", r.path)
        .env("QUERY_STRING", r.query)
        .env("REQUEST_METHOD", r.method)
        .env("CONTENT_TYPE", header("content-type"))
        .env("CONTENT_LENGTH", r.body.len().to_string())
        .env("HTTP_CONTENT_ENCODING", header("content-encoding"))
        .env("GIT_PROTOCOL", header("git-protocol"))
        .env("REMOTE_USER", r.user)
        .env("ZAPPER_GIT_LEVEL", r.level)
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("GIT_COMMITTER_NAME", r.user)
        .env("GIT_COMMITTER_EMAIL", r.email)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let Ok(mut child) = cmd.spawn() else {
        return (
            plain(
                StatusCode::INTERNAL_SERVER_ERROR,
                "git http-backend is missing on the host",
            ),
            false,
        );
    };
    let mut stdin = child.stdin.take().unwrap();
    let body = r.body;
    let feed = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
    });
    let out = match child.wait_with_output().await {
        Ok(o) => o,
        Err(e) => {
            return (
                plain(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
                false,
            )
        }
    };
    let _ = feed.await;
    // CGI: headers, a blank line, the body.
    let split = out
        .stdout
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| {
            out.stdout
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| (i, 2))
        });
    let Some((at, sep)) = split else {
        return (
            plain(
                StatusCode::INTERNAL_SERVER_ERROR,
                &String::from_utf8_lossy(&out.stderr),
            ),
            false,
        );
    };
    let head = String::from_utf8_lossy(&out.stdout[..at]).into_owned();
    let mut resp = Response::builder().status(200);
    for line in head.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("status") {
            let code = v
                .split_whitespace()
                .next()
                .and_then(|c| c.parse::<u16>().ok())
                .unwrap_or(500);
            resp = resp.status(code);
        } else {
            resp = resp.header(k, v);
        }
    }
    let resp = resp
        .body(Full::new(Bytes::copy_from_slice(&out.stdout[at + sep..])))
        .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "bad CGI response"));
    (resp, out.status.success())
}

/// The repository's `pre-receive` hook, by the pusher's grant
/// (`ZAPPER_GIT_LEVEL`, set by the git handler, as `REMOTE_USER` is):
///
/// - write: branches, never `main`, branches under `main/`, or tags;
/// - merge: `main` only, and only to merge a branch already on the server
///   that someone else pushed and wrote: a fast-forward to its tip, or a
///   merge commit of the old `main` and its tip whose tree is exactly the
///   merge's (nothing added in the merge itself);
/// - admin: anything.
///
/// It records who pushed each commit new to the repository
/// (`zapper-pushed`), which the merge check reads.
pub const PRE_RECEIVE: &str = r#"#!/bin/sh
# Installed by zapper: write pushes branches, merge merges someone else's
# branch into main, admin does anything.
zero=0000000000000000000000000000000000000000
pushed="$GIT_DIR/zapper-pushed"
refuse() { echo "zapper: $*" >&2; exit 1; }

# The branch other than main whose tip is $1, if there is one.
branch_at() {
    git for-each-ref --format='%(objectname) %(refname)' refs/heads/ |
        awk -v sha="$1" '$1 == sha && $2 != "refs/heads/main" { print $2; exit }'
}

# Refuses a merge of old..tip with any commit the merger pushed or wrote.
not_theirs() {
    for c in $(git rev-list "$1..$2"); do
        if [ -f "$pushed" ] && grep -q "^$c $REMOTE_USER\$" "$pushed"; then
            refuse "$REMOTE_USER pushed $c: someone else merges your work (ask for a review)"
        fi
    done
    if git log --format='%an%n%cn' "$1..$2" | grep -qx "$REMOTE_USER"; then
        refuse "$REMOTE_USER wrote commits on this branch: someone else merges your work (ask for a review)"
    fi
}

merge_into_main() {
    old=$1 new=$2
    [ "$old" != "$zero" ] || refuse "main doesn't exist yet: someone with admin starts it"
    [ "$new" != "$zero" ] || refuse "main can't be deleted"
    if [ -n "$(branch_at "$new")" ] && git merge-base --is-ancestor "$old" "$new"; then
        not_theirs "$old" "$new"
        return
    fi
    set -- $(git rev-list --parents -n 1 "$new")
    [ $# -eq 3 ] || refuse "with merge, main moves only to a branch's tip or to a merge of a branch into it"
    [ "$2" = "$old" ] || refuse "a merge's first parent must be main as it is ($old)"
    tip=$3
    [ -n "$(branch_at "$tip")" ] || refuse "merge a branch that is on the server ($tip isn't a branch's tip)"
    merged=$(git merge-tree --write-tree "$old" "$tip" 2>/dev/null) ||
        refuse "the branch doesn't merge cleanly into main (or git is older than 2.38): its author rebases it"
    [ "$(echo "$merged" | head -n 1)" = "$(git rev-parse "$new^{tree}")" ] ||
        refuse "a merge may add nothing of its own: push the merge exactly as git makes it"
    not_theirs "$old" "$tip"
}

while read old new ref; do
    # Who pushed each commit new to the repository, for the merge check.
    if [ "$new" != "$zero" ]; then
        git rev-list "$new" --not --all | sed "s/\$/ $REMOTE_USER/" >> "$pushed"
    fi
    case "$ZAPPER_GIT_LEVEL" in
        admin) ;;
        merge)
            [ "$ref" = refs/heads/main ] ||
                refuse "$REMOTE_USER has merge: they merge into main, and push nothing else ($ref); someone with write pushes branches"
            merge_into_main "$old" "$new" ;;
        *)
            case "$ref" in
                refs/heads/main|refs/heads/main/*|refs/tags/*)
                    refuse "$REMOTE_USER has write access: push a branch; $ref needs merge (into main) or admin (ask with credentials_request)" ;;
            esac ;;
    esac
done
exit 0
"#;

/// The hook for a repository the operator also pushes to on the host, with
/// no level set: their pushes are theirs to make; a guest's, through
/// `http_backend`, are checked as `PRE_RECEIVE` checks them.
pub fn pre_receive_with_local_admin() -> String {
    PRE_RECEIVE.replacen(
        "zero=0000000000000000000000000000000000000000",
        "# A push on the host itself (no level set) is the operator's.\n[ -n \"$ZAPPER_GIT_LEVEL\" ] || exit 0\nzero=0000000000000000000000000000000000000000",
        1,
    )
}
