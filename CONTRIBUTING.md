# Contributing to oh-my-pi

> [!NOTE]
> Pull requests are **temporarily open to everyone** as a trial. Before, we
> asked for a vouch before we accepted a PR. That requirement is lifted for now
> while we see how open contributions go. The vouch system can return,
> depending on the results.

## Before you start

Send small changes — bug fixes, documentation updates, narrow improvements —
directly as a pull request.

### Major changes

Discuss major features and broad architectural or behavioral changes in
[Discord](https://discord.gg/4NMW9cdXZa) **before you write the code**. Major
changes include new subsystems, large UI changes, new dependencies, and changes
that span several packages. A GitHub issue does not replace this discussion.
Prior discussion does not guarantee that we merge your pull request.

### Do not open an issue for work you are about to submit

If you plan to implement a change yourself, **do not create an issue for it
first**. robomp treats actionable issues as work to pick up. It can start the
same fix in parallel, which wastes compute and maintainer time.

If you are only reporting a problem or proposing work, open an issue. If a
relevant issue already exists, link it from your pull request instead of
creating another one.

## AI-assisted contributions

AI agents are welcome as tools, not as unattended contributors. Do not give an
agent a vague goal, then submit whatever it produces.

Before you open a pull request, you must:

- Keep the agent inside the agreed scope, and reject unrelated changes.
- Review every changed file, and understand the resulting behavior.
- Run the relevant checks, and exercise the changed behavior yourself.
- Submit the pull request only after that review. Do not let an agent publish
  it on its own.

You are responsible for the code, whoever or whatever wrote it.

## Pull request requirements

Every pull request body **MUST include at least one sentence that you write in
your own words**. That sentence explains what changed and why. A generated
summary, a pasted agent transcript, or a checklist alone does not satisfy this
requirement.

One honest line is enough:

> I reviewed the full diff. This change fixes duplicate PR reviews by reusing
> the existing delivery guard.

You **MUST verify that the change works as intended**. `bun check` and the
automated tests are expected where relevant, but they do not prove that the
behavior works. Exercise the changed path yourself, and report the exact
scenario and result in the pull request:

- For a bug fix, reproduce the bug, then confirm that the same reproduction no
  longer fails.
- For a feature, launch the product, then use the feature end to end.
- For a UI change, interact with the UI, then inspect the rendered result.

“`bun check` passes” alone is not sufficient verification.

For coding-agent development commands and repository structure, see
[`AGENTS.md`](AGENTS.md) in this repo. In the upstream tree, see
[`packages/coding-agent/DEVELOPMENT.md`](https://github.com/can1357/oh-my-pi/blob/main/packages/coding-agent/DEVELOPMENT.md).
That path exists only after the release tree is generated from upstream.

Keep each pull request to one logical change. Unrelated cleanup, drive-by
refactors, and out-of-scope features belong in their own pull request.

## Contribution licensing

Contributions submitted for inclusion in OMP are licensed under the MIT
License.

This policy does not relicense third-party or vendored code. You must have the
right to submit your contribution. You must also preserve the applicable
copyright, license, attribution, and notice material. You do not need to sign a
Contributor License Agreement (CLA) or certify a Developer Certificate of
Origin (DCO).

## Review

Answer review feedback yourself. Apply only the suggestions you checked.

If a pull request skips required prior discussion, lacks the human-written
explanation, contains unreviewed agent output, or mixes unrelated changes, we
can close it.
