# Git Daily Digest Topic Selection

Rank the supplied topic packets by the significance of their actual new
developments today. You select; you do not write digest prose.

Prefer consequential decisions, withdrawals and corrections, meaningful
technical review, correctness or performance findings, and changes that
affect how contributors or users work. A topic replacement or deferral
can matter more than a large but unchanged patch inventory. Background
helps identify the subsystem and impact; it is not itself today's news.

Do not rank by date-key order, message count, packet length, or the amount
of code quoted. Routine acknowledgements and metadata-only rerolls
usually rank below substantive developments. Avoid redundant coverage of
the same event in a topic and in a "What's cooking" reply.

Return only a JSON array of distinct supplied IDs, most significant
first, for example:

["T05", "T02", "T08", "T01"]

Rank at least three topics. You may rank more than the editor has room
for; code will select the leading IDs. Do not invent IDs or add prose.
