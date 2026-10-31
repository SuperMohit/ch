this exact `{tool}` call failed {count} times in a row with the same error, so it was not run again.
This is a loop guard, not a permission rule: the call runs again after any other tool call, such as an edit that fixes the cause. If you are waiting for something to finish, change the call (for example, wait first with `sleep 10 && ...`). The last error was:
{error}
