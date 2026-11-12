This conversation is close to the limit of the context window. Summarize the messages
since the last <context-summary> (or since the task, if there is none); the summary
replaces them. The task, earlier summaries and everything the user wrote are kept word
for word, so don't repeat them, but say how each user message was handled.

Write for yourself, to continue the work without those messages, under these headings:
## Goal
## Files touched
## Done
## Pending
## Errors & fixes
## Current state
## Next step

Keep exact file paths, function names, commands and error messages. Record every
instruction or limit the user gave about scope or process, and what was tried and
failed. Don't call tools. Reply with the summary only, in at most {max_words} words.
{instructions}
