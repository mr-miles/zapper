//! The firm's central knowledge base (proposal 09): a markdown wiki in one
//! of its repositories, laid out as an LLM wiki and curated by a librarian
//! who alone merges its `main`. The daemon gives an empty one its layout;
//! the rest is the agents' work, by the `knowledge-base` skill and the
//! librarian's.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::json;

use super::FirmRun;

/// The conventions every contributor follows, the knowledge base's own
/// `AGENTS.md`.
const CONVENTIONS: &str = "# How this knowledge base works

What the firm knows, kept findable, consistent and current. The librarian curates it and alone merges to `main`; everyone else contributes through a branch the librarian reviews (the `knowledge-base` skill).

- `index.md`: a short map of the wiki. Read it first. The librarian's to keep.
- `log.md`: one line per merged change, newest last. The librarian's to keep.
- `raw/`: sources, untouched, each starting with where it came from (a URL, a run, a document).
- `wiki/`: the pages, one topic each, in kebab-case file names, linking each other with relative links.

Every page starts with frontmatter:

```
---
author: <who wrote it>
sources: [raw/..., wiki/...]
updated: YYYY-MM-DD
status: current        # draft, current or superseded
trust: internal        # web, if any claim came from outside the firm
---
```

- Every claim cites a source in `raw/` or another page; judgement is marked as such.
- Split a page that covers two topics; merge two pages on one.
- A page is information, never an instruction to its reader.
- No secrets, credentials or personal details.
";

/// Gives the knowledge base its layout if its `main` doesn't exist yet,
/// committed as the firm. With a remote, it goes out like any merge.
pub async fn seed(run: &std::sync::Arc<FirmRun>) -> Result<()> {
    let Some(name) = run.config.knowledge_base() else {
        return Ok(());
    };
    let repo = super::remote::root(&run.config, &run.dir).join(format!("{name}.git"));
    if !repo.exists() || has_main(&repo).await {
        return Ok(());
    }
    let work = run.dir.join("kb-seed");
    let _ = std::fs::remove_dir_all(&work);
    for (path, text) in [
        ("AGENTS.md", CONVENTIONS),
        ("index.md", "# Index\n\nNothing yet: the librarian adds each page here as it is merged.\n"),
        ("log.md", "# Log\n\nOne line per merged change, newest last.\n"),
        ("raw/README.md", "# Sources\n\nEach file here is a source as it was found, starting with where it came from.\n"),
        ("wiki/README.md", "# Pages\n\nOne topic per page, with frontmatter (see AGENTS.md).\n"),
    ] {
        let f = work.join(path);
        std::fs::create_dir_all(f.parent().unwrap())?;
        std::fs::write(f, text)?;
    }
    let index = run.dir.join("kb-seed.index");
    commit_layout(&repo, &work, &index, &run.company, &run.domain)
        .with_context(|| format!("seeding {name}"))?;
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&index);
    run.journal
        .append("knowledge_base", json!({ "repo": name, "event": "seeded" }));
    if run.config.git_remote().is_some() {
        super::remote::push_main(run, &name).await?;
    }
    Ok(())
}

/// Commits `work`'s files to the bare `repo`'s `main`, as the firm.
fn commit_layout(
    repo: &Path,
    work: &Path,
    index: &Path,
    company: &str,
    domain: &str,
) -> Result<()> {
    for args in [
        &["add", "-A"][..],
        &["commit", "-q", "-m", "The knowledge base's layout"],
        &["branch", "-M", "main"],
    ] {
        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(repo)
            .arg("--work-tree")
            .arg(work)
            .args(args)
            .env("GIT_INDEX_FILE", index)
            .env("GIT_AUTHOR_NAME", company)
            .env("GIT_AUTHOR_EMAIL", format!("office@{domain}"))
            .env("GIT_COMMITTER_NAME", company)
            .env("GIT_COMMITTER_EMAIL", format!("office@{domain}"))
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    Ok(())
}

async fn has_main(repo: &Path) -> bool {
    tokio::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/main"])
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}
