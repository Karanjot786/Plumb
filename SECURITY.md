# Security policy

## Reporting a vulnerability

**Do not open a public issue for a security problem.**

Report it through GitHub's private vulnerability reporting:
**[Security → Report a vulnerability](https://github.com/Karanjot786/Plumb/security/advisories/new)**.
That channel is private between you and the maintainer until a fix ships. No email
address is published here on purpose; the advisory form is the whole contact surface.

Expect an acknowledgement within 7 days. If you have not heard anything in 14 days,
open a public issue saying only that you are waiting on a security response. Include no
details.

## What is in scope, and why it matters here

This tool moves and deletes files on behalf of the person running it. The interesting bugs are not
memory-safety bugs. They are path-handling bugs. Report anything in this list:

- A path outside a staging directory reaching `commit`, the only function in the project
  deleting anything.
- Any way to defeat `assert_in_staging`: a `..` component smuggled past the component
  scan, a `.plumb-staging` marker in an attacker-chosen place, or a race between the
  check and the removal. An intermediate symlink inside staging is already known, listed
  under Known issues in the README, and needs no report.
- A path on the deny list being staged anyway, or the structural rules in
  `shape_problem` being bypassed, so a home directory, a volume root or a mount point
  becomes a candidate.
- A TOCTOU window between an item being planned and being moved, such that a different
  file than the one inspected is the one that moves.
- A manifest whose contents cause `restore` to write outside the original path it
  records, or to clobber a file that already exists there.
- Reclaimable bytes reported above what is physically allocated. Nothing here is
  memory-unsafe, and the defect is still serious. Plumb would promise space the
  filesystem never delivers. This has happened once already and was fixed.

## Out of scope

- Anything requiring an attacker with write access to your home directory already. Such
  an attacker has no need for this program.
- Denial of service by pointing the scanner at a pathological tree.
- Anything requiring a modified build of the tool itself.

## Supported versions

Only the latest commit on `main`. This project is at v0.1.0 and has no release
branches.
