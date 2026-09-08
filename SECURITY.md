# Security policy

## Reporting a vulnerability

**Do not open a public issue for a security problem.**

Report it through GitHub's private vulnerability reporting:
**[Security → Report a vulnerability](https://github.com/Karanjot786/Visualize_Storage/security/advisories/new)**.
That channel is private between you and the maintainer until a fix ships. No email
address is published here on purpose; the advisory form is the whole contact surface.

Expect an acknowledgement within 7 days. If you have not heard anything in 14 days,
open a public issue saying only that you are waiting on a security response — no
details.

## What is in scope, and why it matters here

This tool moves and deletes files on behalf of the person running it. The interesting
bugs are therefore not memory-safety bugs; they are **path-handling bugs**. Anything in
the following list is worth reporting:

- A path outside a staging directory reaching `commit`, which is the only function in
  the project that deletes anything.
- Any way to defeat `assert_in_staging` — an intermediate symlink, a `..` component
  smuggled past the component scan, a `.sv-staging` marker in an attacker-chosen place,
  a race between the check and the removal.
- A path on the deny list being staged anyway, or the structural rules in
  `shape_problem` being bypassed so that a home directory, a volume root or a mount
  point becomes a candidate.
- A TOCTOU window between an item being planned and being moved, such that a different
  file than the one inspected is the one that moves.
- A manifest whose contents cause `restore` to write outside the original path it
  records, or to clobber a file that already exists there.
- Reclaimable bytes reported *above* what is physically allocated. This one is not
  memory-unsafe and it is still a serious defect: it promises the user space the
  filesystem cannot deliver, and it has happened before (fixed in `14ffe3a`).

## Out of scope

- Requiring an attacker who already has write access to your home directory. Such an
  attacker does not need this program.
- Denial of service by pointing the scanner at a pathological tree.
- Anything requiring a modified build of the tool itself.

## Supported versions

Only the latest commit on `main`. This project is at v0.1.0 and has no release
branches.
