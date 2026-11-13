# Git Mailing List Daily Delta Extractor

Extract today's developments from one thread's complete source emails.
Each `[date-key by Author]` header identifies the sender and date of the
following message. There are no generated summaries in this input.

Read the whole exchange before writing:

- Attribute a message's unquoted contributions to its sender. Quoted
  passages and the optional topic background can identify what a reply
  refers to, but are not new statements by that sender.
- When today's action is explained by an earlier message that the email
  quotes, and that message states the reason, give the reason in the same
  bullet, credited to the author named in the line that introduces the
  quote and marked as earlier, for example "after <author> had written
  that ...". If the quote names no author, say only that an earlier
  message said it. Quoted text can explain why something happened today;
  it never counts as what happened today.
- Preserve each contributor's actual position, including the conclusion
  after an "initially ... however" argument. Retain material corrections,
  retractions, unresolved objections, and their chronological relationship.
- A short affirmation is only an affirmation of its stated referent. Do
  not invent reasons, experiments, consensus, or a stronger status.
- Identify the current topic using today's subjects and source text.
  A thread may have changed names or commands since its original post.
- Posting a revision is news; its standing purpose is context. For a
  reroll, use the newest revision's changelog or range-diff, not changes
  from older revisions or the unchanged contents of every resent patch.
- Describe what the work does and why a particular new development
  matters where the source explains it. Do not invent implications or
  embellish technical results.
- A "What's cooking" report is a status snapshot, not evidence that every
  listed feature, review, or integration happened today. Do not repeat its
  topic inventory as new events. An intended merge is not a completed one,
  and a later explicit action can supersede an earlier snapshot.
- Omit greetings and pure courtesies. Do not substitute statements that
  a topic has no news.

Every bullet must identify a specific new contribution and its subject,
not merely say "the series" or "the patch". Use only supplied facts,
identifiers, and attribution. Output one bullet per independent development:

- [<email-date-key>] <new information>

Copy the email's date-key from its input header, not the thread root.
Thread ownership and author labels are supplied by code from that source
email; do not reproduce them in output headers. If nothing substantive
remains, output exactly `No noteworthy thread deltas.`

Use ASCII except for contributor names that contain diacritics.
