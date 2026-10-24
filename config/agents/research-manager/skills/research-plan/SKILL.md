---
name: research-plan
description: "Scoping a question with web search and writing the research plan: sub-questions, strands with their sources, hosts and method, the pack each researcher returns, and staffing and deciding suggested sources against it. Use it whenever you plan research or change the plan."
---

# The research plan

## Scope with web search

Before you plan, look at the ground. Your agent's web search tool (`WebSearch`) runs on the model's side, not through the firm's network: use it freely, it needs no access request. Fetching pages (`WebFetch`, `browse`, `curl`) goes through the firm's network and needs access to each host.

- Search the question and its obvious sub-questions: what is known, who publishes on it, what is contested.
- Note the sources worth reading and the domains that hold them: official bodies, standards, datasets, papers, first-party documentation. Prefer few, primary domains to many secondary ones.
- Check the drive for earlier work (`drive_list(query: ...)`).
- Stop when you can name the sub-questions and, for each, two or three domains likely to answer it. Scoping is a few searches, not the research.

## The plan

Publish it as a `document` with the assignment's thread, so researchers and the CEO read the same thing:

```
# <Question>: research plan

The question in one sentence, the decision it informs, for whom, by when.

## Sub-questions
1. <Sub-question>: what an answer looks like; how deep.

## Strands
### Strand A: sub-questions 1, 2
Researcher: <name, once hired>
Sources: <title or kind, domain>, with what each should give
Access: <host> (why), one line each
Method: what to read, compare or compute

## The pack each researcher returns
## Out of scope
## Done when
Every sub-question answered with cited findings, or marked unknown with why.
```

Group sub-questions into strands that can run in parallel without the same sources. One researcher per strand; one strand is fine for a small question. Name every host a strand needs: security approves researchers' requests against this list, so a host on it is a quick yes and one off it is a question to you.

## Staffing it

- Hire (or contract, for a one-off question) one researcher per strand, with the strand as the brief: the plan's link, their sub-questions, sources and the hosts they need, the pack they return, the due date.
- Tell them to ask for their strand's hosts at once, all in their first turn, one `request_access` each, the reason naming the plan and the strand; and to bring new sources to you as suggestions, not to security.
- Security approves what the plan names. When security asks you about a request, answer in the turn you read it: yes with why, or no with the alternative. A denied host changes the plan: replace the source or narrow the sub-question, and tell the researcher.

## Suggested sources

Researchers suggest sources the plan didn't name: a host, what it holds, which sub-question it serves and what it would change. For each, decide:

- Yes, if it fills a gap in a sub-question's answer or is the primary source for a key claim: add it to the plan's access list (a new version of the plan, with `changes`) and reply telling them to ask for it, so security finds it in the plan.
- No, if the answer is already good enough, the source is secondary to one they have, or it is out of scope: reply with why, so they don't wait.

Answer suggestions in the turn you read them. More sources aren't better once they stop changing the answer.
