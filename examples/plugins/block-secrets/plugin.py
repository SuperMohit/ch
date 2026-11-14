"""An example external ch plugin in Python, with no dependencies.

A pre_tool hook on every tool: a call whose input contains something that looks like
a credential (a private key, an AWS access key id, a GitHub or Anthropic token) is
refused before it runs, so it can't be written to a file, sent in a command or posted
to a server. The protocol is newline-delimited JSON-RPC 2.0 on stdin and stdout; see
crates/ch-plugin-protocol.
"""

import json
import re
import sys

# Common credential formats. Not exhaustive: a secret split up or encoded gets past.
PATTERNS = [
    ("a private key", re.compile(r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----")),
    ("an AWS access key id", re.compile(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b")),
    (
        "an AWS secret access key",
        re.compile(r"(?i)aws_secret_access_key\s*[=:]\s*[\"']?[A-Za-z0-9/+=]{40}"),
    ),
    ("a GitHub token", re.compile(r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{20,})")),
    ("an Anthropic API key", re.compile(r"\bsk-ant-[A-Za-z0-9_-]{20,}")),
    ("an OpenAI API key", re.compile(r"\bsk-(?:proj-)?[A-Za-z0-9_-]{20,}")),
    ("a Slack token", re.compile(r"\bxox[abposr]-[A-Za-z0-9-]{10,}")),
    ("a Google API key", re.compile(r"\bAIza[0-9A-Za-z_-]{35}\b")),
]


def strings(value):
    """Every string in a JSON value, keys included."""
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from strings(item)
    elif isinstance(value, dict):
        for key, item in value.items():
            yield key
            yield from strings(item)


def check(event):
    call = event.get("call", {})
    for text in strings(call.get("input")):
        for what, pattern in PATTERNS:
            if pattern.search(text):
                return {
                    "decision": "deny",
                    "reason": f"the input contains what looks like {what}; ch won't "
                    "send it anywhere. Use a reference to the secret (an environment "
                    "variable name) instead of its value.",
                }
    return {"decision": "continue"}


def reply(msg_id, result=None, error=None):
    message = {"jsonrpc": "2.0", "id": msg_id}
    if error is not None:
        message["error"] = error
    else:
        message["result"] = result
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def main():
    for line in sys.stdin:
        try:
            message = json.loads(line)
        except ValueError:
            continue
        method = message.get("method")
        msg_id = message.get("id")
        if msg_id is None:
            continue  # notifications: nothing to do
        params = message.get("params") or {}
        if method == "initialize":
            reply(msg_id, {"protocol": 1, "hooks": [{"point": "pre_tool"}]})
        elif method == "hook/invoke":
            reply(msg_id, check(params.get("event", {})))
        elif method == "shutdown":
            reply(msg_id, None)
            return
        else:
            reply(msg_id, error={"code": -32601, "message": f"unknown method {method}"})


if __name__ == "__main__":
    main()
