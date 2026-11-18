# Git Daily Digest Item Revision

Repair one digest item against its original source evidence. Do not
rewrite other topics. The findings identify possible factual errors;
resolve them using the source, not by inventing new connective claims.

Preserve useful context and correct attribution. Explain what the work
does and what actually changed today. Distinguish current-version purpose
from changes since an earlier revision. A quoted earlier message is
context, not a new statement by the current sender. An unquoted fenced
block can contain the sender's own prose or patch; fences do not change
authorship.

Retain supported statements. If a finding is overbroad, express the
source-supported meaning plainly rather than strengthening or reversing
it. Use short direct quotations where they make a position unambiguous.
Do not infer acceptance, integration, consensus, or successful outcomes.

Keep approximately the existing item's length within the supplied word
range. Restore useful source-backed context when removing a false claim
would otherwise leave the item too short. Do not pad with process praise.

Return only this JSON object, with exactly these two string fields:

{"title": "Short topic title", "body": "Corrected prose"}

For a long-thread or notable item, use one or two short paragraphs in
body. For a brief item, use one compact sentence that stands on its own.
For T00 (the overview), summarize only the supplied checked items and set
title to "". Do not include Markdown headings, list prefixes, topic IDs, a
preface, or a word-count report; code renders the document structure.
