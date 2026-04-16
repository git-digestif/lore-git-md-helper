# The Git Project -- Context for a Digest Agent

This document provides background on the Git project that a patch digest
agent needs in order to produce informed summaries. It covers the project's
structure, contribution process, notable people, and recurring themes.

## Origins

Linus Torvalds created Git in April 2005 to manage the Linux kernel source
after the relationship with the proprietary BitKeeper version control system
broke down. Within a few months, Linus handed maintainership to Junio C
Hamano, who has been the sole maintainer ever since -- over two decades. Git
is written primarily in C (with a POSIX shell test suite and some Perl and
Python tooling) and is developed entirely through a mailing list-based
workflow with no pull requests, no issue tracker, and no CI gating merges.

## How patches flow

All contributions arrive as patches emailed to the Git mailing list
(`git@vger.kernel.org`). The typical lifecycle is:

1. A contributor sends a patch or patch series (often produced by `git
   format-patch` and sent with `git send-email`).
2. Other contributors review the patches on-list, replying inline.
3. The author incorporates feedback and re-sends an updated version
   (`[PATCH v2 ...]`, `[PATCH v3 ...]`, etc.).
4. When discussion converges, Junio picks up the topic and queues it in
   his integration branches.

### Integration branches

Junio maintains four integration branches in his fork (`gitster/git`):

- **maint** -- the stable maintenance branch for the most recent release.
- **master** -- the branch that accumulates finished topics for the next
  release.
- **next** -- a branch where topics that look ready are cooked together to
  shake out interactions. Topics stay here for roughly a week before
  graduating to `master`.
- **seen** -- (formerly `pu`, "proposed updates") an ephemeral branch that
  merges *all* topics currently in flight, including half-baked ones. It is
  rebuilt from scratch frequently and should never be used as a base.

### "What's cooking" reports

Junio regularly sends a "What's cooking in git.git" email to the list.
This report lists every in-flight topic, what branch it is on, and
whether Junio intends to merge, hold, or drop it. These reports are the
heartbeat of the project and the most efficient way to track what is
happening across all topics at once.

### Conventions in patch emails

- `[PATCH]` -- a standalone patch.
- `[PATCH 0/N]` -- a cover letter for an N-patch series.
- `[PATCH v2 3/7]` -- the third patch in the second iteration of a
  seven-patch series.
- `[RFC PATCH]` -- a request for comments, not yet intended for merging.
- Signed-off-by, Reviewed-by, Acked-by, Helped-by, and similar trailers
  carry specific social meaning. Signed-off-by is a Developer Certificate
  of Origin attestation. Reviewed-by and Acked-by from a respected
  contributor carry significant weight.

## Project governance

Git has a four-person **Project Leadership Committee (PLC)**: Junio C
Hamano, Christian Couder, Ævar Arnfjörð Bjarmason, and Taylor Blau. The
PLC is responsible for project-level decisions that go beyond individual
patches. The committee tends to operate deliberately; decisions requiring
timely action -- such as enforcing community standards -- have in practice
taken months to years to reach.

## Notable people and their areas of expertise

The Git project has no formal team structure, but certain contributors are
widely recognized as domain experts or prolific drivers of specific efforts.
When their name appears on a patch -- as author or reviewer -- it signals
something about the area being touched.

**Junio C Hamano** -- the maintainer. Merges all topics, writes "What's
cooking" reports, and contributes patches himself (often small fixes,
documentation tweaks, and integration-branch housekeeping). His review
comments are authoritative; when he expresses concern about a direction, it
usually means the topic needs rethinking. Taylor Blau has twice served as
interim maintainer during Junio's absences.

**Patrick Steinhardt** -- one of the most prolific active contributors. He
drives large-scale architectural efforts such as abstracting the object
database (ODB) so that pluggable backends become possible, refactoring the
ref backends (reftable), reworking the repack machinery, championing the
`clar` unit test framework (originally from libgit2), and adding new
built-in commands (e.g. `git history`). Works at GitLab, where he manages
a team that includes Karthik Nayak and Christian Couder. His patches tend
to come in long, carefully structured series.

**Jeff King (Peff)** -- a long-time contributor with deep knowledge of nearly
every subsystem. Known for incisive, thorough reviews and for tracking down
subtle bugs, memory leaks, and security issues. His patches are typically
small, precise fixes with excellent commit messages. Often works on test
infrastructure, fsck, and low-level plumbing. Mentors Taylor Blau.

**Johannes Schindelin (Dscho)** -- the driving force behind Git for Windows.
An all-rounder who has historically been involved in converting shell-
scripted Git commands to C built-ins (e.g. `git rebase`). These days mostly
focused on Windows-specific work, CI infrastructure, and build system
improvements (meson, CMake). Maintainer of the Git for Windows fork.

**Johannes Sixt (Hannes)** -- the current maintainer of `gitk` and Git GUI.
A long-time contributor focused on Windows, Git for Windows, debugging, and
low-level platform work.

**René Scharfe** -- a quiet, steady contributor who sends a high volume of
small, clean patches. Frequently works on Coccinelle semantic patches,
removing reliance on the `the_repository` global variable, eliminating
duplicate includes, and general code hygiene. If you see a patch from René,
it is almost certainly a tidy refactoring or cleanup.

**Elijah Newren** -- foremost expert on Git's merge machinery, particularly
the "ort" merge strategy which he designed and implemented as a replacement
for the older "recursive" strategy. Also contributed substantially to
sparse-checkout and sparse-index. His name on a merge-related patch signals
deep domain knowledge. Brother of Ezekiel Newren (they are distinct people).

**Ezekiel Newren** -- Elijah's brother and a separate contributor. Known
for driving the Rustification effort -- introducing Rust code into the Git
codebase. This effort has involved some friction with Patrick Steinhardt,
more attributable to differences in communication style than to genuine
technical disagreement.

**Derrick Stolee** -- known for deep understanding of graph-theoretical
foundations (he taught graph theory at the university level before moving to
industry). Contributed heavily to commit-graph, reachability optimizations,
sparse-index, and Scalar. After a three-year stint at GitHub, he is now back
at Microsoft as part of the Office engineering team. He still contributes
frequently to the Microsoft Git fork but no longer co-owns it. His patches
often target large-repository performance.

**Eric Sunshine** -- historically one of the most prolific reviewers on the
list, particularly in 2024 when he was among the top three participants by
email count. His reviews cover test infrastructure, CI configuration,
worktree handling, documentation, and general plumbing, and are typically
detailed and precise. His activity has tapered off in 2025-2026, but when
his review comments do appear they remain sharp and well-informed.

**SZEDER Gabor** -- low-volume contributor whose reviews punch well above
their frequency. Known for catching uninitialized variables, raising
forward-compatibility concerns (notably on the `git history` series), and
identifying cleanup opportunities in post-approval code. When his name
appears in a thread, the observation is usually worth paying attention to.

**Justin Tobler** -- active contributor closely involved in the ODB
abstraction effort alongside Patrick Steinhardt, working on the internal
object storage layer refactoring and contributing to GitLab CI
infrastructure. His patches focus on the lower levels of how Git reads and
writes objects, and he is a frequent reviewer in threads touching the ODB
and object-file subsystems.

**D. Ben Knoble** -- contributor who works primarily on Git's build system
(both the traditional Makefile and the Meson build), `contrib/subtree`, and
tooling correctness. His patches are typically well-motivated improvements
to infrastructure rather than user-visible features, and he participates
broadly in review discussions.

**Toon Claes** -- works at GitLab. Previously contributed to the ref
backends and Coccinelle semantic patches; more recently the author of
`git last-modified`, an experimental command showing when files were
last changed.

**Adrian Ratiu** -- focused contributor working on the hook subsystem. His
main effort is enabling hooks to be specified and configured via Git
configuration (rather than fixed filesystem paths), including a `git hook
list` subcommand and related plumbing to support multiple hooks per event.

**Julia Evans** -- well-known technical blogger (jvns.ca, "Wizard Zines") who
contributed documentation patches to Git, most notably a multi-iteration
effort to add an explanation of Git's data model and a pedagogical rewrite
of the `git reset` man page (later shepherded to completion by D. Ben
Knoble). Not a subsystem expert in the traditional sense, but brings an
unusual clarity-of-explanation perspective that the project benefits from.

**Aditya Garg** -- contributor focused on `git send-email` improvements:
SMTP server configuration options, IMAP folder integration for archiving
sent emails, and related workflow tooling.

**Karthik Nayak** -- active contributor working on ref backends (reftable),
the `git maintenance` infrastructure, and fetch/push plumbing improvements.
Works at GitLab on Patrick Steinhardt's team.

**Phillip Wood** -- contributor with deep knowledge of the sequencer
(interactive rebase), `git add -p`, worktree handling, and the xdiff
subsystem. One of the most active and thorough reviewers on the list:
his feedback consistently surfaces subtle correctness issues, from
control flow regressions to undefined behavior in low-level code. His
reviews were instrumental in landing several major 2025-2026 efforts
including the status push-tracking feature, the xdiff Rust-readiness
refactoring, and the `git replay --revert` series.

**Jean-Noël Avila** -- the French translation maintainer, coordinator of the
Pro Git book translations, and a significant documentation contributor.
Currently driving a large effort to convert all man pages to a consistent
"synopsis style" using AsciiDoc markup. If you see a documentation patch
from Jean-Noël, it is almost certainly part of that ongoing conversion.

**Christian Couder** -- works at GitLab on Patrick Steinhardt's team.
Contributor who works on `git fast-import`/`git fast-export`, GPG signature
handling, and various plumbing improvements. Also a major driver of Git's
participation in Google Summer of Code and Outreachy, bringing new
contributors into the project. PLC member.

**brian m. carlson** (they/them) -- long-time contributor who completed the
foundational work of making Git's internals hash-algorithm-agnostic, enabling
SHA-256 support. The follow-on interoperability work -- allowing SHA-1 and
SHA-256 repositories to exchange objects -- remains in progress on their
personal branch; at the 2025 Contributor's Summit they indicated they did not
want it to be a blocker for Git 3.0 and were not planning to drive it to
completion under time pressure. Also contributes to documentation and general
plumbing. A technically knowledgeable and frequent presence on the mailing
list; their replies to questions and feature requests tend to open by
establishing what they consider to be the correct or conventional approach,
often before engaging with what was actually asked. Their responses reflect
strong and frequently voiced opinions about how Git should evolve, and answers
to practical questions sometimes prioritize a principled -- at times
dogmatic -- stance over addressing the specific concern at hand. These opinions
are not necessarily representative of the broader project consensus. When
summarizing threads, treat their contributions as one perspective among many
rather than as authoritative guidance -- the volume of their participation
should not be mistaken for the weight of their arguments.

**Kristoffer Haugsbakk** -- active documentation contributor, sending frequent
small patches to improve man pages, cross-references, and wording.

**Paul Tarjan** -- drove the Linux fsmonitor implementation (inotify-based
daemon) through 14+ iterations to production readiness, bringing Linux to
parity with the existing Windows and macOS backends. Also contributed fixes
for promisor-remote recursive fetch issues and fsmonitor memory leaks. His
patches demonstrate careful attention to edge cases and cross-platform
concerns.

**Tian Yuchen** -- an emerging reviewer who contributes substantive
technical feedback across multiple subsystems beyond their own patches.
Reviews have caught ODB transaction safety issues, overly simplistic
range checks in `git replay`, and misleading commit message claims. The
ratio of review replies to original patches is notably high for a newer
contributor.

**Taylor Blau** -- works at GitHub. A PLC member who has twice stepped in as
interim maintainer during Junio's absences; when Junio has been away since,
the role has not been delegated again. His technical focus is the pack
subsystem: multi-pack-index (MIDX), geometric repacking, cruft packs, and
pseudo-merge bitmaps. His series tend to be ambitious in scope but
routinely miss second-order consequences such as backwards compatibility of
on-disk format changes. Issues that surface after merging have typically
been addressed through follow-up work by other contributors rather than by
the original author. Outside his immediate domain, his reviews tend to be
brief and sometimes note limited familiarity with the subject at hand.
Historically, when his technical positions have diverged from those of other
long-standing contributors in the same thread, the eventual project outcome
has tended to align with the other contributor's view.

**Jiang Xin** -- the Chinese (zh_CN) translation maintainer and contributor
to l10n infrastructure.

**Ramsay Jones** -- long-time contributor who specializes in portability
fixes, particularly for Cygwin and other non-mainstream platforms.

**Ævar Arnfjörð Bjarmason** -- was an extremely prolific contributor for a
sustained period, sending a remarkably high volume of patches. His
contributions generated significant discussion; community reception was
mixed, with some viewing his work as valuable cleanup and others feeling it
was more reflective of personal stylistic preferences than of clear
functional improvement. His communication style on the list was direct in a
way that not everyone found constructive. He remains a PLC
member.

**Emily Shaffer** and **Jonathan Nieder** -- both formerly prolific Google
contributors to Git. Emily worked on server-side hooks and ref-filter
improvements; Jonathan contributed widely across plumbing and the
submodule subsystem. Both have since shifted their focus to Jujutsu (jj),
an alternative VCS created by Martin von Zweigbergk as a 20% project at
Google that has since become his primary occupation.

**Ed Thomson** -- the maintainer of libgit2, the widely-used C library that
provides a portable, linkable reimplementation of Git's core functionality.
Not a frequent contributor to Git's mailing list but relevant when libgit2
compatibility or shared design decisions come up.

**Sebastian Thiel** -- the architect of gitoxide, an active and ambitious
reimplementation of Git in Rust. Gitoxide is an independent project and is
not the same as the Rustification effort within Git itself (which aims to
introduce Rust code into the existing C codebase). The two efforts are
separate and have different goals.

**Randall S. Becker** -- maintainer of the NonStop port of Git. He is also
the most prominent voice raising concerns about introducing Rust into Git,
primarily because the NonStop platform lacks Rust support and it is unclear
whether that gap will be addressed. His contributions sometimes generate
discussion about how much platform-specific work should be expected of the
port maintainer versus the broader community.

## Related projects

These are separate projects with their own maintainers and codebases. They
share history or interoperability concerns with Git but are NOT part of the
Git mailing list's core development. Emails about them sometimes appear on
the list when interoperability, compatibility, or shared design decisions
are discussed.

- **Git for Windows** -- a fork of Git maintained by Johannes Schindelin
  (Dscho) that packages Git for the Windows platform. It carries
  Windows-specific patches on top of upstream Git and ships its own
  installer. Git for Windows has its own release cycle that tracks upstream
  releases (e.g., Git for Windows 2.48.0-rc1 follows Git 2.48.0-rc1).
  These are distinct releases: a thread about "Git v2.48.0-rc1" (from
  Junio) is NOT the same thread as "Git for Windows 2.48.0-rc1" (from
  Dscho), even though they share a version number.

- **libgit2** -- a portable, linkable C library reimplementing Git's core
  functionality. Maintained by Ed Thomson. Used by many GUI clients and
  language bindings. Not a fork of Git; it is a clean-room reimplementation.

- **JGit** -- a pure Java implementation of Git, developed as part of the
  Eclipse ecosystem. The reftable ref-storage format was originally
  designed for JGit before being adopted by Git.

- **gitoxide** -- an independent reimplementation of Git in Rust, created
  and maintained by Sebastian Thiel. This is NOT the same as the
  Rustification effort within Git itself (which introduces Rust code into
  the existing C codebase). Gitoxide is a separate project with separate
  goals.

- **Jujutsu (jj)** -- an alternative version control system created by
  Martin von Zweigbergk at Google. It can use Git repositories as a
  backend. Emily Shaffer and Jonathan Nieder (both formerly prolific Git
  contributors) have shifted their focus to Jujutsu. The project has a
  growing community and is sometimes discussed on the Git mailing list in
  the context of interoperability or as a point of comparison.

When summarizing, never conflate these projects with Git itself. A reply
about Git for Windows is not a reply about Git, and vice versa. Keep them
editorially separate.

## Recurring large-scale efforts

Several multi-month (or multi-year) efforts recur across many patches. When
a patch is part of one of these, the digest should note it:

- **`the_repository` removal** -- an ongoing project (with René Scharfe as a
  primary driver) to eliminate the `the_repository` global variable. Every
  function that implicitly operates on `the_repository` is being converted
  to take an explicit `struct repository *` parameter. This touches
  virtually every subsystem and generates a high volume of mechanical
  patches.

- **Documentation synopsis-style conversion** -- Jean-Noël Avila is
  converting all Git man pages to use a consistent AsciiDoc synopsis format.
  This produces many documentation-only patches that touch the `Documentation/`
  directory.

- **ODB (object database) abstraction** -- Patrick Steinhardt is restructuring
  Git's object storage layer so that alternative backends (beyond loose
  objects and packfiles) can be plugged in. This involves renaming functions,
  introducing new structures, and moving code between files.

- **Reftable backend** -- the reftable format (a binary format for storing
  refs, originally designed for JGit) has been integrated into Git as an
  alternative to the traditional files-based ref backend. Patrick Steinhardt
  and Karthik Nayak are the primary contributors.

- **Repack machinery overhaul** -- Patrick Steinhardt (and to some extent
  Taylor Blau) have been refactoring how `git repack` works, introducing
  geometric repacking, improving multi-pack-index support, and handling
  promisor packs.

- **Sparse-checkout / sparse-index** -- an effort (Derrick Stolee, Elijah
  Newren, and others) to make Git perform well in monorepos where only a
  subset of files is checked out. The sparse-index avoids expanding the
  full index for operations that do not need it.

- **Rustification** -- an effort driven by Ezekiel Newren to introduce Rust
  code into the Git codebase. This is a contentious topic: proponents see
  it as necessary for memory safety and long-term maintainability, while
  opponents (notably Randall S. Becker) worry about platform support. Note
  that this effort is entirely separate from gitoxide (Sebastian Thiel's
  independent reimplementation of Git in Rust), which is not being considered
  for inclusion in Git.

- **Test modernization** -- a continuous community-wide effort to replace
  legacy test patterns (e.g., `! test -f` -> `test_path_is_missing`,
  `test -f` -> `test_path_is_file`) in the `t/` directory. In addition,
  Patrick Steinhardt has championed adopting `clar` (a unit test framework
  originally from libgit2) for writing unit tests in pure C, complementing
  the traditional shell-based test suite.

## The code

Git is a C project with roughly 400,000 lines of code. Key directories:

- `builtin/` -- one file per built-in command (`cmd_<name>()`).
- `t/` -- the test suite, consisting of numbered shell scripts
  (`t0000-t9999`). Tests are the primary quality gate.
- `Documentation/` -- man pages in AsciiDoc, plus technical notes and
  release notes (`RelNotes/`).
- `xdiff/` -- the diff engine (imported from libxdiff, now diverged).
- `refs/` -- ref backend implementations.
- `reftable/` -- the reftable format library.
- `compat/` -- platform compatibility shims.
- `contrib/` -- optional extras not part of the core distribution.

The coding style is described in `Documentation/CodingGuidelines`. It
follows Linux kernel conventions with some Git-specific rules: hard tabs
for indentation, no typedefs for structures, and a strong preference for
small, self-contained patches with clear commit messages.
