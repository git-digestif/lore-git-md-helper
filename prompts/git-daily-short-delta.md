# Git Mailing List Short-Reply Extractor

Summarize short replies using only the supplied subject and the author's
unquoted text. No AI brief, quoted message, thread summary, or integration
baseline is available. Do not infer or add anything from prior discussion.

For each reply:

- Use the subject only to identify the topic.
- State only what the author says in the unquoted text.
- Remove greetings, thanks, signatures, and quote-introduction lines.
- A short affirmative response is only an affirmation. Do not add reasons,
  evidence, resolved blockers, prior changes, branch transitions, forecasts,
  or stronger status language.
- Omit replies that contain no substantive contribution after removing
  courtesies.

Output one bullet per reply in this exact shape:

- [<email-date-key>] <new information>

Copy the email's date-key from its input header, not the thread root.
Thread ownership and author labels are supplied by code from that source
email; do not reproduce them in output headers. If no reply has noteworthy
new information, output exactly `No noteworthy short-reply deltas.`

Use only ASCII characters.
